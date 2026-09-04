use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod policy;

pub const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_GIT_POINTER_BYTES: u64 = 64 * 1024;

pub fn protect_current_process_if_service_account_token_present() -> Result<()> {
    if std::env::var_os("OP_SERVICE_ACCOUNT_TOKEN").is_some() {
        protect_current_process_from_peer_inspection()?;
    }
    Ok(())
}

pub fn protect_current_process_from_peer_inspection() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let result = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to disable peer process inspection");
        }
    }
    Ok(())
}

pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
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
    provided_git_metadata_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    let validated_roots = git_metadata_roots(cwd)?;
    anyhow::ensure!(
        provided_git_metadata_roots == validated_roots,
        "Git metadata roots do not match the validated repository at {}",
        cwd.display()
    );
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let mut permitted_roots = vec![cwd.clone()];
    permitted_roots.extend(
        writable_roots
            .iter()
            .map(|path| {
                std::fs::canonicalize(path)
                    .with_context(|| format!("cannot resolve writable root {}", path.display()))
            })
            .collect::<Result<Vec<_>>>()?,
    );
    for git_root in &validated_roots {
        anyhow::ensure!(
            permitted_roots
                .iter()
                .any(|permitted| git_root.starts_with(permitted)),
            "Git metadata root is outside the permitted session roots: {}",
            git_root.display()
        );
    }
    run_with_metadata_roots(command, &cwd, writable_roots, &validated_roots, stdin).await
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
    validate_writable_scope(&cwd, writable_roots)?;
    #[cfg(target_os = "macos")]
    let spec = if git_metadata_roots.is_empty() {
        policy::SandboxSpec::command(&cwd, writable_roots)?
    } else {
        policy::SandboxSpec::git(&cwd, writable_roots, git_metadata_roots)?
    };

    #[cfg(target_os = "linux")]
    let mut process = linux::command(command, &cwd, writable_roots, git_metadata_roots)?;

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
    let child = process
        .spawn()
        .context("failed to start sandboxed command")?;
    wait_with_limited_output(child, stdin).await
}

/// Resolves the worktree's private Git directory and its common repository
/// directory. The latter is needed for linked worktrees, whose `.git` file
/// points below the common repository metadata directory.
pub fn git_metadata_roots(cwd: &Path) -> Result<Vec<PathBuf>> {
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let worktree_root = git_worktree_root(&cwd)?;
    let dot_git_path = worktree_root.join(".git");
    let dot_git_metadata = std::fs::symlink_metadata(&dot_git_path).with_context(|| {
        format!(
            "cannot inspect Git metadata pointer {}",
            dot_git_path.display()
        )
    })?;
    anyhow::ensure!(
        !dot_git_metadata.file_type().is_symlink(),
        "symbolic-link .git metadata pointers are not supported: {}",
        dot_git_path.display()
    );
    let dot_git_is_directory = dot_git_metadata.file_type().is_dir();
    let dot_git = std::fs::canonicalize(&dot_git_path).with_context(|| {
        format!(
            "cannot resolve Git metadata pointer {}",
            dot_git_path.display()
        )
    })?;
    anyhow::ensure!(
        dot_git_is_directory || dot_git_metadata.file_type().is_file(),
        "Git metadata pointer is neither a directory nor a file: {}",
        dot_git_path.display()
    );
    let git_dir = if dot_git_is_directory {
        dot_git.clone()
    } else {
        resolve_git_pointer(&dot_git_path)?
    };
    let common_dir = match read_git_control_file(&git_dir.join("commondir"), "Git commondir")? {
        Some(value) => {
            let mut lines = value.lines().filter(|line| !line.trim().is_empty());
            let relative = lines.next().map(str::trim).unwrap_or_default();
            anyhow::ensure!(!relative.is_empty(), "Git commondir is empty");
            anyhow::ensure!(
                lines.next().is_none(),
                "Git commondir contains unexpected extra content"
            );
            let path = git_dir.join(relative);
            std::fs::canonicalize(&path).with_context(|| {
                format!("cannot resolve Git common directory {}", path.display())
            })?
        }
        None => git_dir.clone(),
    };
    if dot_git_is_directory {
        anyhow::ensure!(
            common_dir == git_dir,
            "unexpected Git commondir in a regular repository: {}",
            dot_git_path.display()
        );
    }
    let common_parent = common_dir
        .parent()
        .context("Git common directory has no parent")?;
    if !dot_git_is_directory {
        // A linked worktree may live outside the common repository directory.
        // Only accept Git's standard private worktree metadata in that case,
        // and verify its back-pointer so an arbitrary `.git` pointer cannot
        // grant write access to an unrelated directory.
        let worktrees = common_dir.join("worktrees");
        anyhow::ensure!(
            git_dir != common_dir
                && git_dir.parent() == Some(worktrees.as_path())
                && worktrees.is_dir(),
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

fn read_git_control_file(path: &Path, label: &str) -> Result<Option<String>> {
    use std::io::Read as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot open {label} {} safely", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect {label} {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "{label} is not a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_GIT_POINTER_BYTES,
        "{label} exceeds {MAX_GIT_POINTER_BYTES} bytes: {}",
        path.display()
    );
    let mut bytes =
        Vec::with_capacity((metadata.len() as usize).min(MAX_GIT_POINTER_BYTES as usize));
    file.by_ref()
        .take(MAX_GIT_POINTER_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {label} {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_GIT_POINTER_BYTES,
        "{label} exceeds {MAX_GIT_POINTER_BYTES} bytes: {}",
        path.display()
    );
    let contents = String::from_utf8(bytes)
        .with_context(|| format!("{label} is not valid UTF-8: {}", path.display()))?;
    Ok(Some(contents))
}

fn resolve_git_pointer(dot_git: &Path) -> Result<PathBuf> {
    let contents = read_git_control_file(dot_git, "Git pointer")?
        .with_context(|| format!("Git pointer does not exist: {}", dot_git.display()))?;
    let mut lines = contents.lines();
    let value = lines
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("Git pointer does not contain a gitdir path")?;
    anyhow::ensure!(
        lines.all(|line| line.trim().is_empty()),
        "Git pointer contains unexpected extra content"
    );
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
    let contents = read_git_control_file(pointer, "Git pointer")?
        .with_context(|| format!("Git pointer does not exist: {}", pointer.display()))?;
    let mut lines = contents.lines().filter(|line| !line.trim().is_empty());
    let value = lines.next().map(str::trim);
    anyhow::ensure!(
        value.is_some() && lines.next().is_none(),
        "Git pointer must contain exactly one non-empty path"
    );
    let value = value.expect("validated Git pointer path");
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

fn validate_writable_scope(cwd: &Path, writable_roots: &[PathBuf]) -> Result<()> {
    anyhow::ensure!(
        !is_protected_metadata_location(cwd),
        "sandbox cwd must not be inside protected metadata: {}",
        cwd.display()
    );
    for root in writable_roots {
        let canonical = std::fs::canonicalize(root)
            .with_context(|| format!("cannot resolve writable root {}", root.display()))?;
        anyhow::ensure!(
            !is_protected_metadata_location(&canonical),
            "writable root must not be inside protected metadata: {}",
            canonical.display()
        );
    }
    Ok(())
}

fn is_protected_metadata_location(path: &Path) -> bool {
    path.components().any(|component| {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        matches!(name.to_str(), Some(".git" | ".agents" | ".codex"))
    })
}

pub async fn run_unrestricted(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_unrestricted_with_env(command, cwd, stdin, &HashMap::new(), &[]).await
}

pub async fn run_unrestricted_with_env(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
    environment: &HashMap<String, String>,
    remove_environment: &[&str],
) -> Result<Output> {
    run_unrestricted_with_env_mode(command, cwd, stdin, environment, remove_environment, false)
        .await
}

pub async fn run_unrestricted_with_env_and_spawn_hook<F>(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
    environment: &HashMap<String, String>,
    remove_environment: &[&str],
    on_spawn: F,
) -> Result<Output>
where
    F: FnOnce(u32) -> Result<()>,
{
    run_unrestricted_with_env_mode_and_spawn_hook(
        command,
        cwd,
        stdin,
        environment,
        remove_environment,
        false,
        on_spawn,
    )
    .await
}

#[cfg(target_os = "linux")]
pub async fn run_unrestricted_with_env_and_spawn_hook_private_pid<F>(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
    environment: &HashMap<String, String>,
    remove_environment: &[&str],
    on_spawn: F,
) -> Result<Output>
where
    F: FnOnce(u32) -> Result<()>,
{
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let bwrap = trusted_service_account_bwrap()?;
    let mut wrapped = vec![
        bwrap.to_string_lossy().into_owned(),
        "--bind".to_owned(),
        "/".to_owned(),
        "/".to_owned(),
        "--dev-bind".to_owned(),
        "/dev".to_owned(),
        "/dev".to_owned(),
        "--proc".to_owned(),
        "/proc".to_owned(),
        "--unshare-pid".to_owned(),
        "--die-with-parent".to_owned(),
        "--new-session".to_owned(),
        "--chdir".to_owned(),
        cwd.to_string_lossy().into_owned(),
        "--".to_owned(),
    ];
    wrapped.extend(command.iter().cloned());
    run_unrestricted_with_env_mode_and_spawn_hook(
        &wrapped,
        &cwd,
        stdin,
        environment,
        remove_environment,
        false,
        on_spawn,
    )
    .await
}

#[cfg(target_os = "linux")]
fn trusted_service_account_bwrap() -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    for candidate in [Path::new("/usr/bin/bwrap"), Path::new("/bin/bwrap")] {
        let Ok(path) = std::fs::canonicalize(candidate) else {
            continue;
        };
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("failed to inspect bubblewrap at {}", path.display()))?;
        if !metadata.is_file()
            || metadata.uid() != 0
            || metadata.mode() & 0o111 == 0
            || metadata.mode() & 0o022 != 0
        {
            continue;
        }
        let mut trusted = true;
        for ancestor in path.ancestors().skip(1) {
            let metadata = std::fs::metadata(ancestor).with_context(|| {
                format!("failed to inspect bubblewrap parent {}", ancestor.display())
            })?;
            if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                trusted = false;
                break;
            }
        }
        if trusted {
            return Ok(path);
        }
    }
    anyhow::bail!(
        "service-account target isolation requires a root-owned, non-writable /usr/bin/bwrap"
    )
}

pub async fn run_unrestricted_with_only_env(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
    environment: &HashMap<String, String>,
) -> Result<Output> {
    run_unrestricted_with_env_mode(command, cwd, stdin, environment, &[], true).await
}

async fn run_unrestricted_with_env_mode(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
    environment: &HashMap<String, String>,
    remove_environment: &[&str],
    clear_environment: bool,
) -> Result<Output> {
    run_unrestricted_with_env_mode_and_spawn_hook(
        command,
        cwd,
        stdin,
        environment,
        remove_environment,
        clear_environment,
        |_| Ok(()),
    )
    .await
}

async fn run_unrestricted_with_env_mode_and_spawn_hook<F>(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
    environment: &HashMap<String, String>,
    remove_environment: &[&str],
    clear_environment: bool,
    on_spawn: F,
) -> Result<Output>
where
    F: FnOnce(u32) -> Result<()>,
{
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let mut process = Command::new(&command[0]);
    process
        .kill_on_drop(true)
        .args(&command[1..])
        .current_dir(cwd);
    if clear_environment {
        process.env_clear();
    }
    for name in remove_environment {
        process.env_remove(name);
    }
    process
        .envs(environment)
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
    let pid = child
        .id()
        .context("unsandboxed child PID is unavailable after spawn")?;
    if let Err(error) = on_spawn(pid) {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(error);
    }
    wait_with_limited_output(child, stdin).await
}

async fn wait_with_limited_output(
    mut child: tokio::process::Child,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    let child_stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .context("sandbox command stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("sandbox command stderr was not captured")?;
    let remaining = Arc::new(AtomicUsize::new(MAX_COMMAND_OUTPUT_BYTES));
    let write_stdin = async move {
        if let Some(bytes) = stdin {
            let mut child_stdin = child_stdin.context("sandbox command stdin was not captured")?;
            child_stdin.write_all(bytes).await?;
            child_stdin.shutdown().await?;
        }
        Result::<()>::Ok(())
    };
    let (stdout, stderr, stdin_result) = tokio::join!(
        read_limited(stdout, remaining.clone()),
        read_limited(stderr, remaining),
        write_stdin
    );
    let (stdout, stdout_truncated) = stdout?;
    let (stderr, stderr_truncated) = stderr?;
    stdin_result?;
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
    environment.insert("TEMOTE_MCP_SANDBOX".to_owned(), "1".to_owned());
    environment
}

#[cfg(test)]
mod generic_tests {
    use super::*;
    use crate::test_support;

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

    #[cfg(unix)]
    #[test]
    fn generated_bidirectional_process_io_does_not_deadlock() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cwd = std::env::current_dir().unwrap();

        test_support::run(0x5049_5045_494f_0001, 32, |ctx| {
            let input_len = noprop::sample_usize_in(ctx, 64 * 1024..=256 * 1024);
            let output_len = noprop::sample_usize_in(ctx, 64 * 1024..=256 * 1024);
            let input = vec![b'i'; input_len];
            let command = vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                format!("head -c {output_len} /dev/zero; cat >/dev/null"),
            ];

            let output = runtime.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    run_unrestricted(&command, &cwd, Some(&input)),
                )
                .await
                .expect("bidirectional child I/O deadlocked")
                .unwrap()
            });
            assert_eq!(output.status, 0);
            assert_eq!(output.stdout.len(), output_len);
            assert!(!output.truncated);
            Ok(())
        })
    }

    #[test]
    fn generated_shared_output_budget_never_overcaptures() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        test_support::run(0x4f55_5450_5554_0001, 256, |ctx| {
            let budget = noprop::sample_usize_in(ctx, 0..=2048);
            let stdout_len = noprop::sample_usize_in(ctx, 0..=3072);
            let stderr_len = noprop::sample_usize_in(ctx, 0..=3072);

            runtime.block_on(async {
                let (mut stdout_writer, stdout_reader) = tokio::io::duplex(4096);
                let (mut stderr_writer, stderr_reader) = tokio::io::duplex(4096);
                stdout_writer
                    .write_all(&vec![b'o'; stdout_len])
                    .await
                    .unwrap();
                stdout_writer.shutdown().await.unwrap();
                stderr_writer
                    .write_all(&vec![b'e'; stderr_len])
                    .await
                    .unwrap();
                stderr_writer.shutdown().await.unwrap();

                let remaining = Arc::new(AtomicUsize::new(budget));
                let (stdout, stderr) = tokio::join!(
                    read_limited(stdout_reader, remaining.clone()),
                    read_limited(stderr_reader, remaining.clone())
                );
                let (stdout, stdout_truncated) = stdout.unwrap();
                let (stderr, stderr_truncated) = stderr.unwrap();
                let captured = stdout.len() + stderr.len();
                let total = stdout_len + stderr_len;

                assert!(captured <= budget, "captured={captured} budget={budget}");
                assert_eq!(remaining.load(Ordering::SeqCst), budget - captured);
                if total <= budget {
                    assert_eq!(captured, total);
                    assert!(!stdout_truncated && !stderr_truncated);
                } else {
                    assert_eq!(captured, budget);
                    assert!(stdout_truncated || stderr_truncated);
                }
            });
            Ok(())
        })
    }

    #[test]
    fn generated_reservations_never_exceed_atomic_budget() -> noprop::TestResult {
        test_support::run(0x5245_5345_5256_4501, test_support::DEFAULT_CASES, |ctx| {
            let initial = noprop::sample_usize_in(ctx, 0..=8192);
            let requests = (0..32)
                .map(|_| noprop::sample_usize_in(ctx, 0..=2048))
                .collect::<Vec<_>>();
            let remaining = AtomicUsize::new(initial);
            let mut reserved_total = 0usize;

            for request in requests {
                let before = remaining.load(Ordering::SeqCst);
                let reserved = reserve_bytes(&remaining, request);
                assert_eq!(reserved, before.min(request));
                reserved_total += reserved;
                assert_eq!(remaining.load(Ordering::SeqCst), initial - reserved_total);
            }
            assert!(reserved_total <= initial);
            Ok(())
        })
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
                .get("TEMOTE_MCP_SANDBOX")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn protected_metadata_detection_matches_component_reference_model() -> noprop::TestResult {
        const PROTECTED: [&str; 3] = [".git", ".agents", ".codex"];

        test_support::run(0x5341_4e44_424f_5801, test_support::DEFAULT_CASES, |ctx| {
            let count = noprop::sample_usize_in(ctx, 1..=6);
            let mut components = (0..count)
                .map(|_| test_support::safe_component(ctx))
                .collect::<Vec<_>>();
            if noprop::sample_bool(ctx) {
                let index = noprop::sample_usize_in(ctx, 0..components.len());
                components[index] =
                    PROTECTED[noprop::sample_usize_in(ctx, 0..PROTECTED.len())].to_owned();
            }
            let path = components.iter().collect::<PathBuf>();
            let expected = components
                .iter()
                .any(|component| PROTECTED.contains(&component.as_str()));
            assert_eq!(
                is_protected_metadata_location(&path),
                expected,
                "metadata classification mismatch for {path:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_writable_scope_fails_closed_for_protected_metadata() -> noprop::TestResult {
        const PROTECTED: [&str; 3] = [".git", ".agents", ".codex"];
        let fixture = tempfile::tempdir().unwrap();
        let cwd = fixture.path().join("workspace");
        std::fs::create_dir(&cwd).unwrap();
        let cwd = std::fs::canonicalize(cwd).unwrap();

        test_support::run(0x5341_4e44_5343_4f50, 512, |ctx| {
            let protected = noprop::sample_bool(ctx);
            let mut path = cwd.clone();
            if protected {
                path.push(PROTECTED[noprop::sample_usize_in(ctx, 0..PROTECTED.len())]);
            } else {
                path.push(test_support::safe_component(ctx));
            }
            path.push(test_support::safe_component(ctx));
            std::fs::create_dir_all(&path).unwrap();

            assert_eq!(
                validate_writable_scope(&cwd, std::slice::from_ref(&path)).is_ok(),
                !protected,
                "writable scope classification mismatch for {path:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_git_pointer_grammar_is_fail_closed() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let pointer = fixture.path().join(".git");
        let target = fixture.path().join("metadata");
        std::fs::create_dir(&target).unwrap();
        let target = std::fs::canonicalize(target).unwrap();

        test_support::run(0x4749_5450_5452_0001, 512, |ctx| {
            let relative = noprop::sample_bool(ctx);
            let rendered_target = if relative {
                "metadata".to_owned()
            } else {
                target.display().to_string()
            };
            let valid = noprop::sample_bool(ctx);
            let contents = if valid {
                let trailing_blanks = noprop::sample_usize_in(ctx, 0..=3);
                format!(
                    "gitdir: {rendered_target}\n{}",
                    "\n".repeat(trailing_blanks)
                )
            } else {
                match noprop::sample_usize_in(ctx, 0..=3) {
                    0 => format!("{rendered_target}\n"),
                    1 => "gitdir:   \n".to_owned(),
                    2 => format!("gitdir: {rendered_target}\nunexpected\n"),
                    _ => format!(" gitdir: {rendered_target}\n"),
                }
            };
            std::fs::write(&pointer, contents).unwrap();
            let result = resolve_git_pointer(&pointer);
            assert_eq!(
                result.is_ok(),
                valid,
                "Git pointer classification mismatch: {result:?}"
            );
            if let Ok(resolved) = result {
                assert_eq!(resolved, target);
            }
            Ok(())
        })
    }

    #[test]
    fn generated_plain_git_pointer_requires_one_nonempty_path() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let pointer = fixture.path().join("gitdir");
        let target = fixture.path().join("worktree-dot-git");
        std::fs::write(&target, b"gitdir marker").unwrap();
        let target = std::fs::canonicalize(target).unwrap();

        test_support::run(0x4749_5450_4c41_494e, 512, |ctx| {
            let valid = noprop::sample_bool(ctx);
            let contents = if valid {
                let leading_blanks = noprop::sample_usize_in(ctx, 0..=2);
                let trailing_blanks = noprop::sample_usize_in(ctx, 0..=2);
                format!(
                    "{}worktree-dot-git\n{}",
                    "\n".repeat(leading_blanks),
                    "\n".repeat(trailing_blanks)
                )
            } else {
                match noprop::sample_usize_in(ctx, 0..=2) {
                    0 => "\n   \n".to_owned(),
                    1 => "worktree-dot-git\nsecond\n".to_owned(),
                    _ => "missing-target\n".to_owned(),
                }
            };
            std::fs::write(&pointer, contents).unwrap();
            let result = resolve_plain_git_pointer(&pointer);
            assert_eq!(
                result.is_ok(),
                valid,
                "plain Git pointer classification mismatch: {result:?}"
            );
            if let Ok(resolved) = result {
                assert_eq!(resolved, target);
            }
            Ok(())
        })
    }

    #[test]
    fn generated_git_control_file_size_bound_matches_reference_model() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("pointer");
        test_support::run(0x4749_5443_5452_4c53, 256, |ctx| {
            let extra = noprop::sample_usize_in(ctx, 0..=32);
            let below = noprop::sample_bool(ctx);
            let len = if below {
                noprop::sample_usize_in(ctx, 0..=MAX_GIT_POINTER_BYTES as usize)
            } else {
                MAX_GIT_POINTER_BYTES as usize + 1 + extra
            };
            std::fs::write(&path, vec![b'x'; len]).unwrap();
            let result = read_git_control_file(&path, "test pointer");
            assert_eq!(
                result.is_ok(),
                len <= MAX_GIT_POINTER_BYTES as usize,
                "len={len} result={result:?}"
            );
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn git_control_files_reject_symlinks_and_oversized_contents() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("target");
        let link = fixture.path().join("link");
        std::fs::write(&target, b"metadata").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_git_control_file(&link, "test pointer").is_err());

        let oversized = fixture.path().join("oversized");
        std::fs::write(&oversized, vec![b'x'; MAX_GIT_POINTER_BYTES as usize + 1]).unwrap();
        assert!(read_git_control_file(&oversized, "test pointer").is_err());
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

    #[cfg(unix)]
    #[test]
    fn rejects_a_symbolic_link_git_pointer() {
        let worktree = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(unrelated.path(), worktree.path().join(".git")).unwrap();

        let error = git_metadata_roots(worktree.path()).unwrap_err();

        assert!(error.to_string().contains("symbolic-link .git"));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use uuid::Uuid;

    fn test_root() -> tempfile::TempDir {
        tempfile::tempdir_in("/var/tmp").expect("/var/tmp is required for Linux sandbox tests")
    }

    fn command(program: &str, args: &[&str]) -> Vec<String> {
        std::iter::once(program.to_owned())
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .collect()
    }

    fn host_git(cwd: &Path, args: &[&str]) -> Result<()> {
        let output = std::process::Command::new("/usr/bin/git")
            .args(args)
            .current_dir(cwd)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "host git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn proc_environ_contains(pid: u32, marker: &[u8]) -> Result<bool> {
        match std::fs::read(format!("/proc/{pid}/environ")) {
            Ok(bytes) => Ok(bytes.windows(marker.len()).any(|window| window == marker)),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    #[test]
    fn service_account_startup_environment_is_process_inspection_protected() -> Result<()> {
        const ROLE: &str = "TEMOTE_TEST_STARTUP_ENV_PROTECTION_ROLE";
        const MARKER: &str = "fabricated-startup-token-7f2c";
        const TEST_NAME: &str = "sandbox::linux_tests::service_account_startup_environment_is_process_inspection_protected";

        if std::env::var(ROLE).as_deref() == Ok("fixture") {
            assert_eq!(
                std::env::var("OP_SERVICE_ACCOUNT_TOKEN").as_deref(),
                Ok(MARKER)
            );
            protect_current_process_if_service_account_token_present()?;
            let supervisor_pid = std::process::id();
            let output = std::process::Command::new("/bin/cat")
                .arg(format!("/proc/{supervisor_pid}/environ"))
                .output()?;
            assert!(
                !output.status.success()
                    || !output
                        .stdout
                        .windows(MARKER.len())
                        .any(|window| window == MARKER.as_bytes()),
                "target child recovered the supervisor startup token"
            );
            return Ok(());
        }

        let current_exe = std::env::current_exe()?;
        let output = std::process::Command::new(current_exe)
            .env("OP_SERVICE_ACCOUNT_TOKEN", MARKER)
            .env(ROLE, "fixture")
            .args(["--exact", TEST_NAME, "--nocapture"])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "startup environment protection fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[tokio::test]
    async fn independent_processes_cannot_inspect_sensitive_supervisor_or_cli() -> Result<()> {
        const ROLE: &str = "TEMOTE_TEST_CROSS_SUPERVISOR_ROLE";
        const PID_FILE_ENV: &str = "TEMOTE_TEST_CROSS_SUPERVISOR_PID_FILE";
        const SUPERVISOR_PID_ENV: &str = "TEMOTE_TEST_CROSS_SUPERVISOR_B_PID";
        const SENSITIVE_PID_ENV: &str = "TEMOTE_TEST_CROSS_SUPERVISOR_SENSITIVE_PID";
        const MARKER: &str = "fabricated-cross-supervisor-token-91ab";
        const TEST_NAME: &str = "sandbox::linux_tests::independent_processes_cannot_inspect_sensitive_supervisor_or_cli";

        match std::env::var(ROLE).as_deref() {
            Ok("supervisor-b") => {
                protect_current_process_if_service_account_token_present()?;
                let pid_file = std::env::var(PID_FILE_ENV)?;
                let mut child = std::process::Command::new("/bin/sleep")
                    .arg("2")
                    .env("OP_SERVICE_ACCOUNT_TOKEN", MARKER)
                    .spawn()?;
                std::fs::write(&pid_file, child.id().to_string())?;
                std::thread::sleep(std::time::Duration::from_millis(1200));
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
            Ok("supervisor-a") => {
                let supervisor_pid = std::env::var(SUPERVISOR_PID_ENV)?.parse::<u32>()?;
                let sensitive_pid = std::env::var(SENSITIVE_PID_ENV)?.parse::<u32>()?;
                let command = command(
                    "/bin/sh",
                    &[
                        "-c",
                        &format!(
                            r#"
set -eu
if [ -e /proc/{supervisor_pid}/environ ]; then
  exit 91
fi
if [ -e /proc/{sensitive_pid}/environ ]; then
  exit 92
fi
for path in /proc/[0-9]*/environ; do
  if grep -a -F -q '{MARKER}' "$path" 2>/dev/null; then
    exit 93
  fi
done
"#
                        ),
                    ],
                );
                let output = run_unrestricted_with_env_and_spawn_hook_private_pid(
                    &command,
                    Path::new("/tmp"),
                    None,
                    &HashMap::new(),
                    &[],
                    |_| Ok(()),
                )
                .await?;
                anyhow::ensure!(
                    output.status == 0,
                    "supervisor A target observed host credential process: {}",
                    output.stderr
                );
                return Ok(());
            }
            _ => {}
        }

        let root = tempfile::tempdir()?;
        let pid_file = root.path().join("sensitive.pid");
        let current_exe = std::env::current_exe()?;
        let mut supervisor_b = std::process::Command::new(&current_exe)
            .env("OP_SERVICE_ACCOUNT_TOKEN", MARKER)
            .env(ROLE, "supervisor-b")
            .env(PID_FILE_ENV, &pid_file)
            .args(["--exact", TEST_NAME, "--nocapture"])
            .spawn()?;
        let supervisor_b_pid = supervisor_b.id();

        for _ in 0..200 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        anyhow::ensure!(
            pid_file.exists(),
            "supervisor B never started sensitive CLI fixture"
        );
        let sensitive_pid = std::fs::read_to_string(&pid_file)?.trim().parse::<u32>()?;
        anyhow::ensure!(
            proc_environ_contains(sensitive_pid, MARKER.as_bytes())?,
            "fixture did not expose the sibling credential environment on the host"
        );

        let supervisor_a = std::process::Command::new(&current_exe)
            .env(ROLE, "supervisor-a")
            .env(SUPERVISOR_PID_ENV, supervisor_b_pid.to_string())
            .env(SENSITIVE_PID_ENV, sensitive_pid.to_string())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .output()?;
        anyhow::ensure!(
            supervisor_a.status.success(),
            "supervisor A isolation fixture failed: {}",
            String::from_utf8_lossy(&supervisor_a.stderr)
        );
        let status = supervisor_b.wait()?;
        anyhow::ensure!(status.success(), "supervisor B fixture failed");
        Ok(())
    }

    #[tokio::test]
    async fn service_account_private_pid_namespace_hides_host_peer_environments() -> Result<()> {
        const MARKER: &str = "fabricated-host-peer-token-c8f4";

        let mut peer = std::process::Command::new("/bin/sleep")
            .arg("2")
            .env("OP_SERVICE_ACCOUNT_TOKEN", MARKER)
            .spawn()?;
        let peer_pid = peer.id();
        let command = command(
            "/bin/sh",
            &[
                "-c",
                &format!(
                    r#"
set -eu
if [ -e /proc/{peer_pid}/environ ]; then
  exit 91
fi
for path in /proc/[0-9]*/environ; do
  if grep -a -F -q '{MARKER}' "$path" 2>/dev/null; then
    exit 92
  fi
done
"#
                ),
            ],
        );
        let output = run_unrestricted_with_env_and_spawn_hook_private_pid(
            &command,
            Path::new("/tmp"),
            None,
            &HashMap::new(),
            &[],
            |_| Ok(()),
        )
        .await?;
        let _ = peer.kill();
        let _ = peer.wait();
        assert_eq!(output.status, 0, "{}", output.stderr);
        Ok(())
    }

    #[tokio::test]
    async fn linux_filesystem_policy_allows_only_workspace_explicit_and_tmp_writes() -> Result<()> {
        let root = test_root();
        let workspace = root.path().join("workspace");
        let explicit = root.path().join("explicit");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&explicit)?;
        std::fs::write(workspace.join("input"), b"readable\n")?;

        let read = run(
            &command("/bin/sh", &["-c", "test \"$(cat input)\" = readable"]),
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_eq!(read.status, 0, "{}", read.stderr);

        let cwd_write = run(
            &command("/usr/bin/touch", &["cwd-write"]),
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_eq!(cwd_write.status, 0, "{}", cwd_write.stderr);
        assert!(workspace.join("cwd-write").is_file());

        let explicit_marker = explicit.join("explicit-write");
        let explicit_write = run(
            &command("/usr/bin/touch", &[explicit_marker.to_str().unwrap()]),
            &workspace,
            std::slice::from_ref(&explicit),
            None,
        )
        .await?;
        assert_eq!(explicit_write.status, 0, "{}", explicit_write.stderr);
        assert!(explicit_marker.is_file());

        let tmp_marker = PathBuf::from("/tmp").join(format!("temote-mcp-{}", Uuid::new_v4()));
        let tmp_write = run(
            &command("/usr/bin/touch", &[tmp_marker.to_str().unwrap()]),
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_eq!(tmp_write.status, 0, "{}", tmp_write.stderr);
        assert!(tmp_marker.is_file());

        let outside_marker =
            PathBuf::from("/var/tmp").join(format!("temote-mcp-{}", Uuid::new_v4()));
        let outside_write = run(
            &command("/usr/bin/touch", &[outside_marker.to_str().unwrap()]),
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(outside_write.status, 0);
        assert!(!outside_marker.exists());

        let _ = std::fs::remove_file(tmp_marker);
        Ok(())
    }

    #[tokio::test]
    async fn linux_normal_git_metadata_is_read_only_but_run_git_can_commit() -> Result<()> {
        let root = test_root();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;
        host_git(&workspace, &["init", "-q"])?;
        std::fs::write(workspace.join("tracked.txt"), b"tracked\n")?;
        let writable_root = root.path().to_path_buf();
        let git = workspace.join(".git");
        let config = git.join("config");
        let git_roots = git_metadata_roots(&workspace)?;

        let ordinary = run(
            &command("/usr/bin/touch", &[git.join("index").to_str().unwrap()]),
            &workspace,
            std::slice::from_ref(&writable_root),
            None,
        )
        .await?;
        assert_ne!(ordinary.status, 0);
        assert!(!git.join("index").exists());

        let add = run_git(
            &command("/usr/bin/git", &["add", "--", "tracked.txt"]),
            &workspace,
            std::slice::from_ref(&writable_root),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(add.status, 0, "{}", add.stderr);

        let commit = run_git(
            &command(
                "/usr/bin/git",
                &[
                    "-c",
                    "user.name=temote-mcp test",
                    "-c",
                    "user.email=temote-mcp@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                    "commit",
                    "--no-verify",
                    "--no-gpg-sign",
                    "-m",
                    "linux sandbox acceptance",
                ],
            ),
            &workspace,
            std::slice::from_ref(&writable_root),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(commit.status, 0, "{}", commit.stderr);

        let protected = run_git(
            &command("/usr/bin/touch", &[config.to_str().unwrap()]),
            &workspace,
            std::slice::from_ref(&writable_root),
            &git_roots,
            None,
        )
        .await?;
        assert_ne!(protected.status, 0);

        let head = std::process::Command::new("/usr/bin/git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(&workspace)
            .output()?;
        assert!(
            head.status.success(),
            "{}",
            String::from_utf8_lossy(&head.stderr)
        );
        Ok(())
    }

    #[tokio::test]
    async fn linux_linked_worktree_git_operation_is_supported() -> Result<()> {
        let root = test_root();
        let repository = root.path().join("repository");
        let worktree = root.path().join("worktree");
        std::fs::create_dir_all(&repository)?;
        host_git(&repository, &["init", "-q"])?;
        std::fs::write(repository.join("base.txt"), b"base\n")?;
        host_git(&repository, &["add", "--", "base.txt"])?;
        host_git(
            &repository,
            &[
                "-c",
                "user.name=temote-mcp test",
                "-c",
                "user.email=temote-mcp@example.invalid",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        )?;
        host_git(
            &repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        )?;
        std::fs::write(worktree.join("feature.txt"), b"feature\n")?;
        let writable_root = root.path().to_path_buf();
        let git_roots = git_metadata_roots(&worktree)?;
        assert_eq!(git_roots.len(), 2);

        let add = run_git(
            &command("/usr/bin/git", &["add", "--", "feature.txt"]),
            &worktree,
            std::slice::from_ref(&writable_root),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(add.status, 0, "{}", add.stderr);

        let commit = run_git(
            &command(
                "/usr/bin/git",
                &[
                    "-c",
                    "user.name=temote-mcp test",
                    "-c",
                    "user.email=temote-mcp@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                    "commit",
                    "--no-verify",
                    "--no-gpg-sign",
                    "-m",
                    "linked worktree acceptance",
                ],
            ),
            &worktree,
            std::slice::from_ref(&writable_root),
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
        Ok(())
    }

    #[tokio::test]
    async fn linux_network_and_child_hardening_are_restricted() -> Result<()> {
        let root = test_root();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;

        let no_new_privs = run(
            &command(
                "/bin/sh",
                &[
                    "-c",
                    "grep -q '^NoNewPrivs:[[:space:]]*1' /proc/self/status",
                ],
            ),
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_eq!(no_new_privs.status, 0, "{}", no_new_privs.stderr);

        let network = run(
            &command("/bin/bash", &["-c", "echo >/dev/tcp/198.51.100.1/80"]),
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(network.status, 0);
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_directory() -> PathBuf {
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(".temote-mcp-sandbox-test-{}", Uuid::new_v4()))
    }

    #[tokio::test]
    async fn seatbelt_allows_workspace_writes_and_denies_other_writes() -> Result<()> {
        // Nix's macOS build sandbox does not allow a nested Seatbelt profile.
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
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
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
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
                "temote-mcp-test".into(),
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
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
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
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
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
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
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
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
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
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
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
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
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
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
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
                "user.name=temote-mcp test".into(),
                "-c".into(),
                "user.email=temote-mcp@example.invalid".into(),
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
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
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
                "user.name=temote-mcp test",
                "-c",
                "user.email=temote-mcp@example.invalid",
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
                "user.name=temote-mcp test".into(),
                "-c".into(),
                "user.email=temote-mcp@example.invalid".into(),
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
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
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
