use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
#[cfg(target_os = "linux")]
use codex_protocol::models::PermissionProfile;
#[cfg(target_os = "linux")]
use codex_protocol::permissions::{
    FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxPolicy,
    NetworkSandboxPolicy,
};
#[cfg(target_os = "linux")]
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod policy;

pub const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;

pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

#[cfg(target_os = "linux")]
fn absolute(path: &Path) -> Result<AbsolutePathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    AbsolutePathBuf::from_absolute_path(path).map_err(|error| anyhow::anyhow!(error))
}

pub async fn run(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_with_metadata_roots(command, cwd, writable_roots, &[], stdin).await
}

/// Runs a narrowly validated Git operation with write access to the repository
/// metadata needed by `git add` and `git commit`. Ordinary sandboxed commands
/// continue to keep `.git` read-only.
pub async fn run_git(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    git_metadata_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_with_metadata_roots(command, cwd, writable_roots, git_metadata_roots, stdin).await
}

async fn run_with_metadata_roots(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    git_metadata_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    #[cfg(target_os = "linux")]
    let permissions = {
        let roots = writable_roots
            .iter()
            .map(|path| absolute(path))
            .collect::<Result<Vec<_>>>()?;
        let permissions = PermissionProfile::workspace_write_with(
            &roots,
            NetworkSandboxPolicy::Restricted,
            false, // keep the configured temporary directory available
            false, // keep /tmp available for ordinary shell commands
        )
        .materialize_project_roots_with_workspace_roots(&[absolute(&cwd)?]);
        if git_metadata_roots.is_empty() {
            permissions
        } else {
            let git_roots = git_metadata_roots
                .iter()
                .map(|path| absolute(path))
                .collect::<Result<Vec<_>>>()?;
            let mut file_system = permissions.file_system_sandbox_policy();
            file_system = file_system.with_additional_writable_roots(&cwd, &git_roots);
            add_git_metadata_read_only_entries(&mut file_system, &git_roots);
            PermissionProfile::from_runtime_permissions(
                &file_system,
                NetworkSandboxPolicy::Restricted,
            )
        }
    };

    #[cfg(target_os = "macos")]
    let spec = if git_metadata_roots.is_empty() {
        policy::SandboxSpec::command(&cwd, writable_roots)?
    } else {
        policy::SandboxSpec::git(&cwd, writable_roots, git_metadata_roots)?
    };

    #[cfg(target_os = "linux")]
    let mut process = {
        let args =
            codex_sandboxing::landlock::create_linux_sandbox_command_args_for_permission_profile(
                command.to_vec(),
                &cwd,
                &permissions,
                &cwd,
                false,
                false,
            );
        let executable = std::env::current_exe()?
            .parent()
            .context("local-mcp executable has no parent directory")?
            .join("codex-linux-sandbox");
        anyhow::ensure!(
            executable.is_file(),
            "sandbox helper is missing: {}",
            executable.display()
        );
        let mut process = Command::new(executable);
        process.args(args);
        process
    };

    #[cfg(target_os = "macos")]
    let mut process = macos::command(&spec, command)?;

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let mut process =
        { anyhow::bail!("sandboxed execution is currently implemented for Linux and macOS only") };

    process
        .kill_on_drop(true)
        .current_dir(&cwd)
        .env_clear()
        .envs(safe_environment())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process
        .spawn()
        .context("failed to start sandboxed command")?;
    if let Some(bytes) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin.write_all(bytes).await?;
    }
    wait_with_limited_output(child).await
}

/// Resolves the worktree's private Git directory and its common repository
/// directory. The latter is needed for linked worktrees, whose `.git` file
/// points below the common repository metadata directory.
pub fn git_metadata_roots(cwd: &Path) -> Result<Vec<PathBuf>> {
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let worktree_root = git_worktree_root(&cwd)?;
    let dot_git_path = worktree_root.join(".git");
    let dot_git = std::fs::canonicalize(&dot_git_path).with_context(|| {
        format!(
            "cannot resolve Git metadata pointer {}",
            dot_git_path.display()
        )
    })?;
    let git_dir = if dot_git.is_dir() {
        dot_git.clone()
    } else {
        resolve_git_pointer(&dot_git_path)?
    };
    let common_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(value) => {
            let relative = value.trim();
            anyhow::ensure!(!relative.is_empty(), "Git commondir is empty");
            let path = git_dir.join(relative);
            std::fs::canonicalize(&path).with_context(|| {
                format!("cannot resolve Git common directory {}", path.display())
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
        Err(error) => return Err(error).context("cannot read Git commondir"),
    };
    let common_parent = common_dir
        .parent()
        .context("Git common directory has no parent")?;
    if dot_git_path.is_file() {
        // A linked worktree may live outside the common repository directory.
        // Only accept Git's standard private worktree metadata in that case,
        // and verify its back-pointer so an arbitrary `.git` pointer cannot
        // grant write access to an unrelated directory.
        anyhow::ensure!(
            git_dir != common_dir && git_dir.starts_with(common_dir.join("worktrees")),
            "Git metadata is outside the working directory ancestry: {}",
            common_dir.display()
        );
        let linked_worktree = resolve_plain_git_pointer(&git_dir.join("gitdir"))?;
        anyhow::ensure!(
            linked_worktree == dot_git,
            "Git linked-worktree metadata does not point back to {}",
            dot_git.display()
        );
    } else {
        anyhow::ensure!(
            cwd.starts_with(common_parent),
            "Git metadata is outside the working directory ancestry: {}",
            common_dir.display()
        );
    }

    let mut roots = vec![git_dir, common_dir];
    roots.sort();
    roots.dedup();
    Ok(roots)
}

pub fn git_worktree_root(cwd: &Path) -> Result<PathBuf> {
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    cwd.ancestors()
        .find(|ancestor| {
            let dot_git = ancestor.join(".git");
            dot_git.is_dir() || dot_git.is_file()
        })
        .map(Path::to_owned)
        .context("no Git repository found from the working directory")
}

fn resolve_git_pointer(dot_git: &Path) -> Result<PathBuf> {
    let contents = std::fs::read_to_string(dot_git)
        .with_context(|| format!("cannot read Git pointer {}", dot_git.display()))?;
    let value = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("Git pointer does not contain a gitdir path")?;
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        dot_git
            .parent()
            .context("Git pointer has no parent")?
            .join(path)
    };
    std::fs::canonicalize(&path)
        .with_context(|| format!("cannot resolve Git directory {}", path.display()))
}

fn resolve_plain_git_pointer(pointer: &Path) -> Result<PathBuf> {
    let contents = std::fs::read_to_string(pointer)
        .with_context(|| format!("cannot read Git pointer {}", pointer.display()))?;
    let value = contents
        .trim()
        .lines()
        .next()
        .filter(|value| !value.is_empty())
        .context("Git pointer is empty")?;
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        pointer
            .parent()
            .context("Git pointer has no parent")?
            .join(path)
    };
    std::fs::canonicalize(&path)
        .with_context(|| format!("cannot resolve Git pointer target {}", path.display()))
}

#[cfg(target_os = "linux")]
fn add_git_metadata_read_only_entries(
    policy: &mut FileSystemSandboxPolicy,
    git_roots: &[AbsolutePathBuf],
) {
    // `git add` and `git commit` need the index, objects, refs, and reflogs.
    // Keep configuration, hooks, and unrelated ref/object stores protected.
    const READ_ONLY_PATHS: &[&str] = &[
        "config",
        "hooks",
        "info",
        "attributes",
        "description",
        "packed-refs",
        "shallow",
        "worktrees",
        "refs/tags",
        "refs/remotes",
        "objects/info",
        "objects/pack",
    ];
    for root in git_roots {
        // A linked worktree's private directory contains per-worktree state
        // that Git may lazily initialize (including config and hooks
        // directories). It is not shared with other worktrees, so leave that
        // private root writable while protecting the common repository root
        // below.
        if root.join("gitdir").is_file() {
            for suffix in ["gitdir", "commondir"] {
                policy.entries.push(FileSystemSandboxEntry {
                    path: FileSystemPath::Path {
                        path: root.join(suffix),
                    },
                    access: FileSystemAccessMode::Read,
                });
            }
            continue;
        }
        for suffix in READ_ONLY_PATHS {
            policy.entries.push(FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: root.join(suffix),
                },
                access: FileSystemAccessMode::Read,
            });
        }
    }
}

pub async fn run_unrestricted(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let mut process = Command::new(&command[0]);
    process
        .kill_on_drop(true)
        .args(&command[1..])
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process
        .spawn()
        .context("failed to start unsandboxed command")?;
    if let Some(bytes) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin.write_all(bytes).await?;
    }
    wait_with_limited_output(child).await
}

async fn wait_with_limited_output(mut child: tokio::process::Child) -> Result<Output> {
    let stdout = child
        .stdout
        .take()
        .context("sandbox command stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("sandbox command stderr was not captured")?;
    let remaining = Arc::new(AtomicUsize::new(MAX_COMMAND_OUTPUT_BYTES));
    let (stdout, stderr) = tokio::join!(
        read_limited(stdout, remaining.clone()),
        read_limited(stderr, remaining)
    );
    let (stdout, stdout_truncated) = stdout?;
    let (stderr, stderr_truncated) = stderr?;
    let status = child.wait().await?;
    Ok(Output {
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated: stdout_truncated || stderr_truncated,
    })
}

async fn read_limited<R>(mut reader: R, remaining: Arc<AtomicUsize>) -> Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    const CHUNK_SIZE: usize = 8192;
    let mut output = Vec::new();
    let mut truncated = false;
    loop {
        let allowance = reserve_bytes(&remaining, CHUNK_SIZE);
        if allowance == 0 {
            let mut discard = [0_u8; CHUNK_SIZE];
            loop {
                let read = reader.read(&mut discard).await?;
                if read == 0 {
                    break;
                }
                truncated = true;
            }
            break;
        }

        let mut buffer = vec![0_u8; allowance];
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            remaining.fetch_add(allowance, Ordering::SeqCst);
            break;
        }
        if read < allowance {
            remaining.fetch_add(allowance - read, Ordering::SeqCst);
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok((output, truncated))
}

fn reserve_bytes(remaining: &AtomicUsize, maximum: usize) -> usize {
    let mut current = remaining.load(Ordering::SeqCst);
    loop {
        if current == 0 {
            return 0;
        }
        let reserved = current.min(maximum);
        match remaining.compare_exchange(
            current,
            current - reserved,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return reserved,
            Err(next) => current = next,
        }
    }
}

fn safe_environment() -> HashMap<String, String> {
    let mut environment = ["PATH", "LANG", "LC_ALL", "TERM", "TMPDIR", "HOME"]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| (name.to_owned(), value))
        })
        .collect::<HashMap<_, _>>();
    environment.insert("LOCAL_MCP_SANDBOX".to_owned(), "1".to_owned());
    environment
}

#[cfg(test)]
mod generic_tests {
    use super::*;

    #[tokio::test]
    async fn limits_a_stream_and_drains_the_rest() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let writer_task = tokio::spawn(async move {
            writer.write_all(b"0123456789abcdef").await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let remaining = Arc::new(AtomicUsize::new(8));
        let (output, truncated) = read_limited(reader, remaining).await.unwrap();
        writer_task.await.unwrap();

        assert_eq!(output, b"01234567");
        assert!(truncated);
    }

    #[test]
    fn preserves_home_for_login_shells() {
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(
                safe_environment().get("HOME").map(String::as_str),
                Some(home.as_str())
            );
        }
        assert_eq!(
            safe_environment()
                .get("LOCAL_MCP_SANDBOX")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn resolves_normal_git_metadata_roots() {
        let repository = tempfile::tempdir().unwrap();
        std::fs::create_dir(repository.path().join(".git")).unwrap();

        let roots = git_metadata_roots(repository.path()).unwrap();

        assert_eq!(
            roots,
            vec![std::fs::canonicalize(repository.path().join(".git")).unwrap()]
        );
    }

    #[test]
    fn resolves_linked_worktree_git_metadata_roots() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        let common = repository.join(".git");
        let private = common.join("worktrees").join("feature");
        let worktree = root.path().join("worktree");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", private.display()),
        )
        .unwrap();
        std::fs::write(private.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            private.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();

        let roots = git_metadata_roots(&worktree).unwrap();

        assert_eq!(
            roots,
            vec![
                std::fs::canonicalize(common).unwrap(),
                std::fs::canonicalize(private).unwrap(),
            ]
        );
    }

    #[test]
    fn rejects_an_unrelated_git_pointer() {
        let worktree = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        std::fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}\n", unrelated.path().display()),
        )
        .unwrap();

        let error = git_metadata_roots(worktree.path()).unwrap_err();

        assert!(error.to_string().contains("outside the working directory"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_git_metadata_policy_keeps_hooks_and_config_read_only() {
        let repository = tempfile::tempdir().unwrap();
        let git = repository.path().join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        let root = absolute(repository.path()).unwrap();
        let mut file_system = PermissionProfile::workspace_write_with(
            std::slice::from_ref(&root),
            NetworkSandboxPolicy::Restricted,
            false,
            false,
        )
        .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&root))
        .file_system_sandbox_policy();
        let git = absolute(&git).unwrap();
        file_system = file_system
            .with_additional_writable_roots(repository.path(), std::slice::from_ref(&git));
        add_git_metadata_read_only_entries(&mut file_system, std::slice::from_ref(&git));

        assert!(file_system.can_write_path_with_cwd(&git.join("objects"), repository.path()));
        assert!(!file_system.can_write_path_with_cwd(&git.join("config"), repository.path()));
        assert!(!file_system.can_write_path_with_cwd(&git.join("hooks"), repository.path()));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_directory() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(".local-mcp-sandbox-test-{}", Uuid::new_v4()))
    }

    #[tokio::test]
    async fn seatbelt_allows_workspace_writes_and_denies_other_writes() -> Result<()> {
        // Nix's macOS build sandbox does not allow a nested Seatbelt profile.
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("LOCAL_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&outside)?;

        let allowed = run(
            &["/usr/bin/touch".into(), "allowed".into()],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_eq!(allowed.status, 0, "{}", allowed.stderr);
        assert!(workspace.join("allowed").is_file());

        let denied_path = outside.join("denied");
        let denied = run(
            &[
                "/usr/bin/touch".into(),
                denied_path.to_string_lossy().into_owned(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(denied.status, 0);
        assert!(!denied_path.exists());

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_denies_update_and_delete_outside_workspace() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("LOCAL_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&outside)?;
        let protected = outside.join("protected");
        std::fs::write(&protected, b"original\n")?;

        let update = run(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "printf 'changed\\n' > \"$1\"".into(),
                "local-mcp-test".into(),
                protected.to_string_lossy().into_owned(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(update.status, 0);
        assert_eq!(std::fs::read(&protected)?, b"original\n");

        let delete = run(
            &["/bin/rm".into(), protected.to_string_lossy().into_owned()],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(delete.status, 0);
        assert_eq!(std::fs::read(&protected)?, b"original\n");

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_allows_an_explicit_extra_writable_root() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("LOCAL_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        let extra = root.join("extra");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&extra)?;

        let marker = extra.join("allowed");
        let output = run(
            &[
                "/usr/bin/touch".into(),
                marker.to_string_lossy().into_owned(),
            ],
            &workspace,
            std::slice::from_ref(&extra),
            None,
        )
        .await?;

        assert_eq!(output.status, 0, "{}", output.stderr);
        assert!(marker.is_file());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_denies_symlink_escape_from_workspace() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("LOCAL_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&outside)?;
        std::os::unix::fs::symlink(&outside, workspace.join("escape"))?;

        let marker = workspace.join("escape").join("denied");
        let output = run(
            &[
                "/usr/bin/touch".into(),
                marker.to_string_lossy().into_owned(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;

        assert_ne!(output.status, 0);
        assert!(!outside.join("denied").exists());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_denies_rename_and_hardlink_escape_from_workspace() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("LOCAL_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&outside)?;

        let rename_source = workspace.join("rename-source");
        std::fs::write(&rename_source, b"rename")?;
        let rename_target = outside.join("rename-target");
        let rename = run(
            &[
                "/bin/mv".into(),
                rename_source.to_string_lossy().into_owned(),
                rename_target.to_string_lossy().into_owned(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(rename.status, 0);
        assert!(rename_source.is_file());
        assert!(!rename_target.exists());

        let link_source = workspace.join("link-source");
        std::fs::write(&link_source, b"link")?;
        let link_target = outside.join("link-target");
        let link = run(
            &[
                "/bin/ln".into(),
                link_source.to_string_lossy().into_owned(),
                link_target.to_string_lossy().into_owned(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(link.status, 0);
        assert!(!link_target.exists());

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_keeps_git_metadata_read_only_for_normal_commands() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("LOCAL_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        let git = workspace.join(".git");
        std::fs::create_dir_all(&git)?;

        let index = git.join("index");
        let output = run(
            &[
                "/usr/bin/touch".into(),
                index.to_string_lossy().into_owned(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;

        assert_ne!(output.status, 0);
        assert!(!index.exists());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn broader_writable_root_does_not_bypass_workspace_git_protection() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("LOCAL_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        let git = workspace.join(".git");
        std::fs::create_dir_all(&git)?;

        let index = git.join("index");
        let output = run(
            &[
                "/usr/bin/touch".into(),
                index.to_string_lossy().into_owned(),
            ],
            &workspace,
            std::slice::from_ref(&root),
            None,
        )
        .await?;

        assert_ne!(output.status, 0);
        assert!(!index.exists());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_git_mode_allows_index_but_protects_config() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("LOCAL_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        let git = workspace.join(".git");
        std::fs::create_dir_all(&git)?;
        let config = git.join("config");
        std::fs::write(&config, b"protected\n")?;
        let git_roots = git_metadata_roots(&workspace)?;

        let index = git.join("index");
        let allowed = run_git(
            &[
                "/usr/bin/touch".into(),
                index.to_string_lossy().into_owned(),
            ],
            &workspace,
            std::slice::from_ref(&root),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(allowed.status, 0, "{}", allowed.stderr);
        assert!(index.is_file());

        let denied = run_git(
            &[
                "/usr/bin/touch".into(),
                config.to_string_lossy().into_owned(),
            ],
            &workspace,
            std::slice::from_ref(&root),
            &git_roots,
            None,
        )
        .await?;
        assert_ne!(denied.status, 0);
        assert_eq!(std::fs::read(&config)?, b"protected\n");

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_git_mode_runs_real_add_and_commit_and_protects_sensitive_metadata()
    -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("LOCAL_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace)?;
        let init = std::process::Command::new("/usr/bin/git")
            .args(["init", "-q"])
            .current_dir(&workspace)
            .status()?;
        assert!(init.success());
        std::fs::write(workspace.join("tracked.txt"), b"tracked\n")?;
        let git_roots = git_metadata_roots(&workspace)?;

        let add = run_git(
            &[
                "/usr/bin/git".into(),
                "add".into(),
                "--".into(),
                "tracked.txt".into(),
            ],
            &workspace,
            std::slice::from_ref(&root),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(add.status, 0, "{}", add.stderr);

        let commit = run_git(
            &[
                "/usr/bin/git".into(),
                "-c".into(),
                "user.name=local-mcp test".into(),
                "-c".into(),
                "user.email=local-mcp@example.invalid".into(),
                "-c".into(),
                "core.hooksPath=/dev/null".into(),
                "-c".into(),
                "commit.gpgSign=false".into(),
                "commit".into(),
                "--no-verify".into(),
                "--no-gpg-sign".into(),
                "-m".into(),
                "sandbox acceptance".into(),
            ],
            &workspace,
            std::slice::from_ref(&root),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(commit.status, 0, "{}", commit.stderr);

        let git = workspace.join(".git");
        for protected in [
            "config",
            "hooks/blocked",
            "refs/tags/blocked",
            "refs/remotes/blocked",
            "objects/pack/blocked",
        ] {
            let path = git.join(protected);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let denied = run_git(
                &["/usr/bin/touch".into(), path.to_string_lossy().into_owned()],
                &workspace,
                std::slice::from_ref(&root),
                &git_roots,
                None,
            )
            .await?;
            assert_ne!(denied.status, 0, "unexpectedly wrote {}", path.display());
            assert!(!path.exists() || protected == "config");
        }

        let head = std::process::Command::new("/usr/bin/git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(&workspace)
            .output()?;
        assert!(
            head.status.success(),
            "{}",
            String::from_utf8_lossy(&head.stderr)
        );

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_git_mode_commits_in_a_linked_worktree() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("LOCAL_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let root = test_directory();
        let repository = root.join("repository");
        let worktree = root.join("worktree");
        std::fs::create_dir_all(&repository)?;

        let init = std::process::Command::new("/usr/bin/git")
            .args(["init", "-q"])
            .current_dir(&repository)
            .status()?;
        assert!(init.success());
        std::fs::write(repository.join("base.txt"), b"base\n")?;
        for args in [
            vec!["add", "--", "base.txt"],
            vec![
                "-c",
                "user.name=local-mcp test",
                "-c",
                "user.email=local-mcp@example.invalid",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        ] {
            let status = std::process::Command::new("/usr/bin/git")
                .args(args)
                .current_dir(&repository)
                .status()?;
            assert!(status.success());
        }
        let status = std::process::Command::new("/usr/bin/git")
            .args(["worktree", "add", "-q", "-b", "feature"])
            .arg(&worktree)
            .current_dir(&repository)
            .status()?;
        assert!(status.success());

        std::fs::write(worktree.join("feature.txt"), b"feature\n")?;
        let git_roots = git_metadata_roots(&worktree)?;
        assert_eq!(git_roots.len(), 2);

        let add = run_git(
            &[
                "/usr/bin/git".into(),
                "add".into(),
                "--".into(),
                "feature.txt".into(),
            ],
            &worktree,
            std::slice::from_ref(&root),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(add.status, 0, "{}", add.stderr);

        let commit = run_git(
            &[
                "/usr/bin/git".into(),
                "-c".into(),
                "user.name=local-mcp test".into(),
                "-c".into(),
                "user.email=local-mcp@example.invalid".into(),
                "-c".into(),
                "core.hooksPath=/dev/null".into(),
                "-c".into(),
                "commit.gpgSign=false".into(),
                "commit".into(),
                "--no-verify".into(),
                "--no-gpg-sign".into(),
                "-m".into(),
                "linked worktree acceptance".into(),
            ],
            &worktree,
            std::slice::from_ref(&root),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(commit.status, 0, "{}", commit.stderr);

        let head = std::process::Command::new("/usr/bin/git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(&worktree)
            .output()?;
        assert!(
            head.status.success(),
            "{}",
            String::from_utf8_lossy(&head.stderr)
        );

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_denies_network_access() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("LOCAL_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let workspace = test_directory();
        std::fs::create_dir_all(&workspace)?;
        let output = run(
            &[
                "/usr/bin/curl".into(),
                "--fail".into(),
                "--max-time".into(),
                "2".into(),
                "https://example.com".into(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(output.status, 0);

        std::fs::remove_dir_all(workspace)?;
        Ok(())
    }
}
