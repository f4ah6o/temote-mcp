use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
pub(crate) const PROTECTED_METADATA_NAMES: &[&str] = &[".git", ".agents", ".codex"];
const MAX_LOCAL_AGENT_PROTECTED_METADATA_SCAN_ENTRIES: usize = 2_000_000;
const MAX_LOCAL_AGENT_PROTECTED_METADATA_SCAN_DEPTH: usize = 64;
const MAX_LOCAL_AGENT_PROTECTED_METADATA_PATHS: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProtectedMetadataScanLimits {
    pub(crate) max_entries: usize,
    pub(crate) max_depth: usize,
    pub(crate) max_paths: usize,
}

const LOCAL_AGENT_PROTECTED_METADATA_SCAN_LIMITS: ProtectedMetadataScanLimits =
    ProtectedMetadataScanLimits {
        max_entries: MAX_LOCAL_AGENT_PROTECTED_METADATA_SCAN_ENTRIES,
        max_depth: MAX_LOCAL_AGENT_PROTECTED_METADATA_SCAN_DEPTH,
        max_paths: MAX_LOCAL_AGENT_PROTECTED_METADATA_PATHS,
    };

/// Returns the protected metadata paths below an existing writable root.
///
/// The first three paths are retained even when they do not exist so a child
/// cannot create a top-level metadata entry after the sandbox starts. Existing
/// nested metadata is discovered with symlink metadata; symbolic links are
/// never followed, and protected directories are not traversed. The local-agent
/// profile uses a bounded walk; when a nested subtree cannot be fully inspected
/// within the bound, that subtree is returned as a read-only fallback. If the
/// writable root itself cannot be inspected, the policy fails closed instead
/// of silently leaving a writable gap.
pub(crate) fn discover_protected_metadata_paths(root: &Path) -> Result<Vec<PathBuf>> {
    discover_protected_metadata_paths_with_limits(root, LOCAL_AGENT_PROTECTED_METADATA_SCAN_LIMITS)
}

pub(crate) fn discover_protected_metadata_paths_with_limits(
    root: &Path,
    limits: ProtectedMetadataScanLimits,
) -> Result<Vec<PathBuf>> {
    let root = std::fs::canonicalize(root)
        .with_context(|| format!("cannot resolve protected metadata root {}", root.display()))?;
    anyhow::ensure!(
        root.is_dir(),
        "protected metadata root is not a directory: {}",
        root.display()
    );

    let mut protected = PROTECTED_METADATA_NAMES
        .iter()
        .map(|name| root.join(name))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        protected.len() <= limits.max_paths,
        "protected metadata path count exceeds {}",
        limits.max_paths
    );
    let mut pending = vec![(root.clone(), 0usize)];
    let mut scanned_entries = 0usize;

    while let Some((directory, depth)) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                if directory == root {
                    return Err(error).with_context(|| {
                        format!(
                            "cannot enumerate protected metadata root directory {}",
                            directory.display()
                        )
                    });
                }
                add_read_only_fallback(&mut protected, &root, &directory, limits.max_paths)?;
                continue;
            }
        };
        for entry in entries {
            if scanned_entries >= limits.max_entries {
                if directory == root {
                    anyhow::bail!(
                        "protected metadata scan exceeds {} entries at writable root {}",
                        limits.max_entries,
                        root.display()
                    );
                }
                add_read_only_fallback(&mut protected, &root, &directory, limits.max_paths)?;
                break;
            }
            scanned_entries += 1;

            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if directory == root {
                        return Err(error).with_context(|| {
                            format!(
                                "cannot read an entry while walking protected metadata root {}",
                                directory.display()
                            )
                        });
                    }
                    add_read_only_fallback(&mut protected, &root, &directory, limits.max_paths)?;
                    break;
                }
            };
            let path = entry.path();
            anyhow::ensure!(
                path.starts_with(&root),
                "protected metadata scan escaped its root: {}",
                path.display()
            );
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    if directory == root {
                        return Err(error).with_context(|| {
                            format!(
                                "cannot inspect protected metadata scan entry {}",
                                path.display()
                            )
                        });
                    }
                    add_read_only_fallback(&mut protected, &root, &directory, limits.max_paths)?;
                    break;
                }
            };
            if is_protected_metadata_name(&entry.file_name()) {
                if protected.contains(&path) {
                    continue;
                }
                if protected.len() >= limits.max_paths {
                    if directory == root {
                        anyhow::bail!(
                            "protected metadata path count exceeds {} at writable root {}",
                            limits.max_paths,
                            root.display()
                        );
                    }
                    add_read_only_fallback(&mut protected, &root, &directory, limits.max_paths)?;
                    break;
                }
                protected.push(path);
                continue;
            }

            if metadata.file_type().is_dir() {
                let child_depth = depth
                    .checked_add(1)
                    .context("protected metadata scan depth overflow")?;
                if child_depth > limits.max_depth {
                    add_read_only_fallback(&mut protected, &root, &path, limits.max_paths)?;
                    continue;
                }
                pending.push((path, child_depth));
            }
        }
    }

    protected.sort();
    protected.dedup();
    Ok(protected)
}

fn add_read_only_fallback(
    protected: &mut Vec<PathBuf>,
    root: &Path,
    directory: &Path,
    max_paths: usize,
) -> Result<()> {
    anyhow::ensure!(
        directory != root,
        "protected metadata scan cannot safely bound writable root {}",
        root.display()
    );
    if protected.iter().any(|path| directory.starts_with(path)) {
        return Ok(());
    }
    protected.retain(|path| !path.starts_with(directory));
    anyhow::ensure!(
        protected.len() < max_paths,
        "protected metadata fallback path count exceeds {max_paths}"
    );
    protected.push(directory.to_owned());
    Ok(())
}

fn is_protected_metadata_name(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| PROTECTED_METADATA_NAMES.contains(&name))
}

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

#[derive(Clone, Debug)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

#[derive(Debug)]
struct LocalAgentSpawnError {
    source: std::io::Error,
}

impl LocalAgentSpawnError {
    fn new(source: std::io::Error) -> Self {
        Self { source }
    }
}

impl std::fmt::Display for LocalAgentSpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("failed to start bounded local-agent command")
    }
}

impl std::error::Error for LocalAgentSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub fn is_local_agent_spawn_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<LocalAgentSpawnError>().is_some()
}

/// A verified intermediate symlink that must be visible for a bounded
/// executable path graph to resolve inside the local-agent sandbox.
///
/// `link` is the lexical path that must exist in the sandbox and `target` is
/// its canonical host target. The parent constructs these values from the
/// fixed agent selected by the broker; the platform-specific sandbox backend
/// validates them again before exposing anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalAgentSymlink {
    pub link: PathBuf,
    pub target: PathBuf,
}

/// Canonical filesystem scope for a local-agent invocation.
pub struct LocalAgentScope<'a> {
    pub writable_roots: &'a [PathBuf],
    pub temporary_roots: &'a [PathBuf],
    pub read_only_paths: &'a [PathBuf],
    pub read_only_roots: &'a [PathBuf],
    pub read_only_symlinks: &'a [LocalAgentSymlink],
    /// Existing normal directories that are needed only to resolve a
    /// verified launcher path. Linux recreates these as empty directories;
    /// their host contents are never mounted.
    pub read_only_scaffold_directories: &'a [PathBuf],
    /// Verified regular files that are needed by a launcher but do not fit
    /// inside one of its read-only directory roots.
    pub read_only_files: &'a [PathBuf],
    pub hidden_roots: &'a [PathBuf],
}

/// Filesystem/network scope for the structured developer-tool broker
/// (Cargo / Vite+). Writes stay limited to the caller-selected workspace
/// (implicitly the cwd) plus narrowly scoped tool cache/state roots; the
/// operation class decides whether outbound network is enabled.
pub struct DeveloperToolScope<'a> {
    pub writable_roots: &'a [PathBuf],
    pub network_access: bool,
}

pub async fn run(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_with_metadata_roots(command, cwd, writable_roots, &[], None, stdin).await
}

/// Runs the structured local-agent broker profile.
///
/// Unlike the deliberately unrestricted host runners below, this profile
/// keeps the filesystem read-only by default and only grants writes to the
/// caller-selected workspace roots and the agent's private run state. Network
/// access is enabled because Codex/OpenCode need to reach their model service.
/// This is a fixed profile; public MCP callers cannot select a generic sandbox
/// escape or alter its filesystem/network policy.
pub async fn run_local_agent(
    command: &[String],
    cwd: &Path,
    scope: LocalAgentScope<'_>,
    stdin: Option<&[u8]>,
    environment: &HashMap<String, String>,
) -> Result<Output> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve local agent cwd {}", cwd.display()))?;
    validate_writable_scope(&cwd, scope.writable_roots)?;
    validate_local_agent_scope(
        scope.writable_roots,
        scope.temporary_roots,
        scope.read_only_paths,
        scope.read_only_roots,
        scope.read_only_symlinks,
        scope.read_only_scaffold_directories,
        scope.read_only_files,
        scope.hidden_roots,
    )?;

    #[cfg(target_os = "macos")]
    let spec = policy::SandboxSpec::local_agent(
        &cwd,
        scope.writable_roots,
        scope.temporary_roots,
        scope.read_only_paths,
        scope.read_only_roots,
        scope.read_only_symlinks,
        scope.read_only_scaffold_directories,
        scope.read_only_files,
        scope.hidden_roots,
    )?;

    #[cfg(target_os = "linux")]
    let mut process = linux::local_agent_command(command, &cwd, &scope)?;

    #[cfg(target_os = "macos")]
    let mut process = macos::command(&spec, command)?;

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let mut process = {
        anyhow::bail!(
            "bounded local-agent execution is currently implemented for Linux and macOS only"
        )
    };

    process
        .kill_on_drop(true)
        .current_dir(&cwd)
        .env_clear()
        .envs(environment)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = process.spawn().map_err(LocalAgentSpawnError::new)?;
    wait_with_limited_output(child, stdin).await
}

/// Runs the structured developer-tool profile for Cargo / Vite+ operations.
///
/// Writes are limited to the canonical cwd plus the caller-provided tool
/// cache/state roots; top-level protected metadata stays read-only. The
/// operation class decides whether outbound network is enabled. This is a
/// fixed profile; callers cannot select an arbitrary sandbox escape.
pub async fn run_developer_tool(
    command: &[String],
    cwd: &Path,
    scope: DeveloperToolScope<'_>,
    stdin: Option<&[u8]>,
    environment: &HashMap<String, String>,
) -> Result<Output> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve developer-tool cwd {}", cwd.display()))?;
    validate_writable_scope(&cwd, scope.writable_roots)?;

    #[cfg(target_os = "macos")]
    let spec =
        policy::SandboxSpec::developer_tool(&cwd, scope.writable_roots, scope.network_access)?;

    #[cfg(target_os = "linux")]
    let mut process =
        linux::developer_tool_command(command, &cwd, scope.writable_roots, scope.network_access)?;

    #[cfg(target_os = "macos")]
    let mut process = macos::command(&spec, command)?;

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let mut process = {
        anyhow::bail!(
            "bounded developer-tool execution is currently implemented for Linux and macOS only"
        )
    };

    process
        .kill_on_drop(true)
        .current_dir(&cwd)
        .env_clear()
        .envs(developer_tool_environment(environment))
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = process
        .spawn()
        .context("failed to start bounded developer-tool command")?;
    wait_with_limited_output(child, stdin).await
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
    run_with_metadata_roots(command, &cwd, writable_roots, &validated_roots, None, stdin).await
}

/// Runs the exact structured `git worktree add` command with the common
/// repository `worktrees` directory writable only for creating new metadata.
/// Existing sibling worktree metadata directories are re-masked read-only.
pub async fn run_git_worktree_add(
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
    let protected_worktree_roots = protected_git_worktree_metadata_roots(&validated_roots)?;
    run_with_metadata_roots(
        command,
        &cwd,
        writable_roots,
        &validated_roots,
        Some(&protected_worktree_roots),
        stdin,
    )
    .await
}

async fn run_with_metadata_roots(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    git_metadata_roots: &[PathBuf],
    protected_worktree_roots: Option<&[PathBuf]>,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    validate_writable_scope(&cwd, writable_roots)?;
    #[cfg(target_os = "macos")]
    let spec = if git_metadata_roots.is_empty() {
        policy::SandboxSpec::command(&cwd, writable_roots)?
    } else if let Some(protected_worktree_roots) = protected_worktree_roots {
        policy::SandboxSpec::git_worktree_add(
            &cwd,
            writable_roots,
            git_metadata_roots,
            protected_worktree_roots,
        )?
    } else {
        policy::SandboxSpec::git(&cwd, writable_roots, git_metadata_roots)?
    };

    #[cfg(target_os = "linux")]
    let mut process = if let Some(protected_worktree_roots) = protected_worktree_roots {
        linux::git_worktree_add_command(
            command,
            &cwd,
            writable_roots,
            git_metadata_roots,
            protected_worktree_roots,
        )?
    } else {
        linux::command(command, &cwd, writable_roots, git_metadata_roots)?
    };

    #[cfg(target_os = "macos")]
    let mut process = macos::command(&spec, command)?;

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let mut process =
        { anyhow::bail!("sandboxed execution is currently implemented for Linux and macOS only") };

    let command_cache = CommandCacheDir::create()?;
    let environment = safe_environment(command_cache.path())?;

    process
        .kill_on_drop(true)
        .current_dir(&cwd)
        .env_clear()
        .envs(environment)
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

fn protected_git_worktree_metadata_roots(git_metadata_roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let common = git_metadata_roots
        .first()
        .context("validated Git metadata roots are empty")?;
    anyhow::ensure!(
        !common.join("gitdir").is_file(),
        "first validated Git metadata root is not the common repository root"
    );
    let worktrees = common.join("worktrees");
    let metadata = match std::fs::symlink_metadata(&worktrees) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", worktrees.display()));
        }
    };
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Git worktrees metadata root must be a normal directory: {}",
        worktrees.display()
    );
    let canonical_worktrees = std::fs::canonicalize(&worktrees)?;
    let current_private = git_metadata_roots.get(1);
    let mut protected = Vec::new();
    for entry in std::fs::read_dir(&worktrees)? {
        let entry = entry?;
        let metadata = entry.file_type()?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.is_symlink(),
            "unexpected non-directory Git worktree metadata entry: {}",
            entry.path().display()
        );
        let path = std::fs::canonicalize(entry.path())?;
        anyhow::ensure!(
            path.parent() == Some(canonical_worktrees.as_path()),
            "Git worktree metadata entry escaped its validated parent: {}",
            path.display()
        );
        if current_private.is_some_and(|current| current == &path) {
            continue;
        }
        protected.push(path);
    }
    protected.sort();
    protected.dedup();
    Ok(protected)
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

#[allow(clippy::too_many_arguments)]
fn validate_local_agent_scope(
    writable_roots: &[PathBuf],
    temporary_roots: &[PathBuf],
    read_only_paths: &[PathBuf],
    read_only_roots: &[PathBuf],
    read_only_symlinks: &[LocalAgentSymlink],
    read_only_scaffold_directories: &[PathBuf],
    read_only_files: &[PathBuf],
    hidden_roots: &[PathBuf],
) -> Result<()> {
    for root in temporary_roots {
        let canonical = std::fs::canonicalize(root).with_context(|| {
            format!(
                "cannot resolve local agent temporary root {}",
                root.display()
            )
        })?;
        anyhow::ensure!(
            canonical.is_dir(),
            "local agent temporary root is not a directory: {}",
            root.display()
        );
        anyhow::ensure!(
            !is_protected_metadata_location(&canonical),
            "local agent temporary root must not be inside protected metadata: {}",
            canonical.display()
        );
    }
    for path in read_only_paths {
        let canonical = std::fs::canonicalize(path).with_context(|| {
            format!(
                "cannot resolve local agent read-only path {}",
                path.display()
            )
        })?;
        anyhow::ensure!(
            !is_protected_metadata_location(&canonical),
            "local agent read-only path must not be inside protected metadata: {}",
            canonical.display()
        );
    }
    for root in read_only_roots {
        let canonical = std::fs::canonicalize(root).with_context(|| {
            format!(
                "cannot resolve local agent read-only root {}",
                root.display()
            )
        })?;
        anyhow::ensure!(
            canonical.is_dir(),
            "local agent read-only root is not a directory: {}",
            root.display()
        );
        anyhow::ensure!(
            !is_protected_metadata_location(&canonical),
            "local agent read-only root must not be inside protected metadata: {}",
            canonical.display()
        );
    }
    for directory in read_only_scaffold_directories {
        anyhow::ensure!(
            is_absolute_clean_path(directory),
            "local agent scaffold directory is not a normalized absolute path: {}",
            directory.display()
        );
        anyhow::ensure!(
            !is_protected_metadata_location(directory),
            "local agent scaffold directory must not be inside protected metadata: {}",
            directory.display()
        );
        anyhow::ensure!(
            directory != Path::new("/"),
            "local agent scaffold directory must not be the filesystem root"
        );
        let metadata = std::fs::symlink_metadata(directory).with_context(|| {
            format!(
                "cannot inspect local agent scaffold directory {}",
                directory.display()
            )
        })?;
        anyhow::ensure!(
            metadata.file_type().is_dir(),
            "local agent scaffold path is not a directory: {}",
            directory.display()
        );
        anyhow::ensure!(
            std::fs::canonicalize(directory).with_context(|| {
                format!(
                    "cannot resolve local agent scaffold directory {}",
                    directory.display()
                )
            })? == *directory,
            "local agent scaffold directory is not canonical: {}",
            directory.display()
        );
    }
    for file in read_only_files {
        anyhow::ensure!(
            is_absolute_clean_path(file),
            "local agent read-only file is not a normalized absolute path: {}",
            file.display()
        );
        anyhow::ensure!(
            !is_protected_metadata_location(file),
            "local agent read-only file must not be inside protected metadata: {}",
            file.display()
        );
        let metadata = std::fs::symlink_metadata(file).with_context(|| {
            format!(
                "cannot inspect local agent read-only file {}",
                file.display()
            )
        })?;
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "local agent read-only path is not a regular file: {}",
            file.display()
        );
        anyhow::ensure!(
            std::fs::canonicalize(file).with_context(|| {
                format!(
                    "cannot resolve local agent read-only file {}",
                    file.display()
                )
            })? == *file,
            "local agent read-only file is not canonical: {}",
            file.display()
        );
        let parent = file
            .parent()
            .context("local agent read-only file has no parent")?;
        let parent_metadata = std::fs::symlink_metadata(parent).with_context(|| {
            format!(
                "cannot inspect local agent read-only file parent {}",
                parent.display()
            )
        })?;
        anyhow::ensure!(
            parent_metadata.file_type().is_dir(),
            "local agent read-only file parent is not a normal directory: {}",
            parent.display()
        );
        anyhow::ensure!(
            !is_protected_metadata_location(parent),
            "local agent read-only file parent must not be inside protected metadata: {}",
            parent.display()
        );
        anyhow::ensure!(
            !writable_roots
                .iter()
                .chain(temporary_roots.iter())
                .chain(read_only_roots.iter())
                .any(|root| file.starts_with(root)),
            "local agent read-only file overlaps another sandbox root: {}",
            file.display()
        );
    }
    for symlink in read_only_symlinks {
        anyhow::ensure!(
            is_absolute_clean_path(&symlink.link),
            "local agent read-only symlink link is not a normalized absolute path: {}",
            symlink.link.display()
        );
        anyhow::ensure!(
            !is_protected_metadata_location(&symlink.link),
            "local agent read-only symlink must not be inside protected metadata: {}",
            symlink.link.display()
        );
        anyhow::ensure!(
            is_absolute_clean_path(&symlink.target),
            "local agent read-only symlink target is not a normalized absolute path: {}",
            symlink.target.display()
        );
        anyhow::ensure!(
            !is_protected_metadata_location(&symlink.target),
            "local agent read-only symlink target must not be inside protected metadata: {}",
            symlink.target.display()
        );
        anyhow::ensure!(
            symlink.target != Path::new("/"),
            "local agent read-only symlink target must not be the filesystem root"
        );
        let metadata = std::fs::symlink_metadata(&symlink.link).with_context(|| {
            format!(
                "cannot inspect local agent read-only symlink {}",
                symlink.link.display()
            )
        })?;
        anyhow::ensure!(
            metadata.file_type().is_symlink(),
            "local agent read-only path is no longer a symlink: {}",
            symlink.link.display()
        );
        let canonical = std::fs::canonicalize(&symlink.link).with_context(|| {
            format!(
                "cannot resolve local agent read-only symlink {}",
                symlink.link.display()
            )
        })?;
        anyhow::ensure!(
            canonical == symlink.target,
            "local agent read-only symlink target changed: {}",
            symlink.link.display()
        );
    }
    for root in hidden_roots {
        let canonical = std::fs::canonicalize(root).with_context(|| {
            format!("cannot resolve local agent hidden root {}", root.display())
        })?;
        anyhow::ensure!(
            canonical.is_dir(),
            "local agent hidden root is not a directory: {}",
            root.display()
        );
        anyhow::ensure!(
            canonical != Path::new("/"),
            "local agent cannot hide the filesystem root"
        );
        anyhow::ensure!(
            !is_protected_metadata_location(&canonical),
            "local agent hidden root must not be inside protected metadata: {}",
            canonical.display()
        );
    }
    Ok(())
}

fn is_absolute_clean_path(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
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
pub(crate) fn trusted_service_account_bwrap() -> Result<PathBuf> {
    for candidate in [Path::new("/usr/bin/bwrap"), Path::new("/bin/bwrap")] {
        if let Some(path) = trusted_bwrap_candidate(candidate)? {
            return Ok(path);
        }
    }
    anyhow::bail!("Linux sandboxing requires a root-owned, non-writable /usr/bin/bwrap")
}

#[cfg(target_os = "linux")]
fn trusted_bwrap_candidate(candidate: &Path) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::MetadataExt;

    let Ok(path) = std::fs::canonicalize(candidate) else {
        return Ok(None);
    };
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("failed to inspect bubblewrap at {}", path.display()))?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
    {
        return Ok(None);
    }
    for ancestor in path.ancestors().skip(1) {
        let metadata = std::fs::metadata(ancestor).with_context(|| {
            format!("failed to inspect bubblewrap parent {}", ancestor.display())
        })?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Ok(None);
        }
    }
    Ok(Some(path))
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

/// Environment for a developer-tool child.
///
/// The caller-provided environment is already validated and filtered, so it is
/// preserved verbatim. The sandbox marker is then forced so a nested
/// Temote-aware tool always observes that it is running inside the bounded
/// developer-tool profile, matching ordinary sandboxed execution.
fn developer_tool_environment(environment: &HashMap<String, String>) -> HashMap<String, String> {
    let mut environment = environment.clone();
    environment.insert("TEMOTE_MCP_SANDBOX".to_owned(), "1".to_owned());
    environment
}

struct CommandCacheDir {
    path: PathBuf,
}

impl CommandCacheDir {
    fn create() -> Result<Self> {
        let temp_root = std::fs::canonicalize(std::env::temp_dir())
            .context("failed to resolve system temporary directory for sandbox cache")?;
        let path = temp_root.join(format!(
            "temote-mcp-command-cache-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).with_context(|| {
            format!(
                "failed to create private sandbox command cache {}",
                path.display()
            )
        })?;
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CommandCacheDir {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = make_private_cache_tree_removable(&self.path);
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn make_private_cache_tree_removable(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(());
    }

    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    std::fs::set_permissions(path, permissions)?;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_private_cache_tree_removable(&entry.path())?;
        }
    }
    Ok(())
}

fn safe_environment(cache_root: &Path) -> Result<HashMap<String, String>> {
    let mut environment = ["PATH", "LANG", "LC_ALL", "TERM", "TMPDIR", "HOME"]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| (name.to_owned(), value))
        })
        .collect::<HashMap<_, _>>();
    environment.insert("TEMOTE_MCP_SANDBOX".to_owned(), "1".to_owned());
    apply_standard_cache_environment(&mut environment, cache_root)?;
    Ok(environment)
}

fn standard_cache_environment_paths(cache_root: &Path) -> [(String, PathBuf); 2] {
    [
        ("XDG_CACHE_HOME".to_owned(), cache_root.join("xdg")),
        ("GOCACHE".to_owned(), cache_root.join("go-build")),
    ]
}

fn apply_standard_cache_environment(
    environment: &mut HashMap<String, String>,
    cache_root: &Path,
) -> Result<()> {
    anyhow::ensure!(
        cache_root.is_absolute(),
        "sandbox cache root must be absolute"
    );
    for (name, path) in standard_cache_environment_paths(cache_root) {
        std::fs::create_dir_all(&path).with_context(|| {
            format!(
                "failed to create sandbox cache directory {}",
                path.display()
            )
        })?;
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        environment.insert(name, path.to_string_lossy().into_owned());
    }
    apply_go_module_cache_environment(environment, cache_root)?;
    Ok(())
}

fn apply_go_module_cache_environment(
    environment: &mut HashMap<String, String>,
    cache_root: &Path,
) -> Result<()> {
    let Some(home) = environment.get("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    if !home.is_absolute() {
        return Ok(());
    }

    // Go's on-disk module download cache already uses the GOPROXY protocol
    // layout. Reuse that existing host cache as a read-only file proxy while
    // directing all module-cache writes/extraction into this command's private
    // temporary cache. This avoids granting write access to host-global
    // $HOME/go/pkg/mod and still lets network-restricted commands consume
    // dependencies that are already cached locally.
    let host_download_cache = home.join("go/pkg/mod/cache/download");
    let metadata = match std::fs::symlink_metadata(&host_download_cache) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect host Go module download cache {}",
                    host_download_cache.display()
                )
            });
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    let host_download_cache = std::fs::canonicalize(&host_download_cache).with_context(|| {
        format!(
            "failed to resolve host Go module download cache {}",
            host_download_cache.display()
        )
    })?;
    let Some(proxy) = go_file_proxy_url(&host_download_cache) else {
        return Ok(());
    };

    let private_module_cache = cache_root.join("go-mod");
    std::fs::create_dir_all(&private_module_cache).with_context(|| {
        format!(
            "failed to create private Go module cache {}",
            private_module_cache.display()
        )
    })?;
    #[cfg(unix)]
    std::fs::set_permissions(
        &private_module_cache,
        std::fs::Permissions::from_mode(0o700),
    )?;
    environment.insert(
        "GOMODCACHE".to_owned(),
        private_module_cache.to_string_lossy().into_owned(),
    );
    environment.insert("GOPROXY".to_owned(), proxy);
    Ok(())
}

fn go_file_proxy_url(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    let path = path.to_str()?;
    let mut url = String::with_capacity(path.len() + "file://".len());
    url.push_str("file://");
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            url.push(byte as char);
        } else {
            url.push('%');
            url.push(HEX[(byte >> 4) as usize] as char);
            url.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    Some(url)
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
        let cache = tempfile::tempdir().unwrap();
        let environment = safe_environment(cache.path()).unwrap();
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(
                environment.get("HOME").map(String::as_str),
                Some(home.as_str())
            );
        }
        assert_eq!(
            environment.get("TEMOTE_MCP_SANDBOX").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            environment.get("XDG_CACHE_HOME").map(PathBuf::from),
            Some(cache.path().join("xdg"))
        );
        assert_eq!(
            environment.get("GOCACHE").map(PathBuf::from),
            Some(cache.path().join("go-build"))
        );
        let default_go_proxy = environment
            .get("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("go/pkg/mod/cache/download"));
        if default_go_proxy.is_some_and(|proxy| {
            std::fs::symlink_metadata(proxy)
                .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
                .unwrap_or(false)
        }) {
            assert_eq!(
                environment.get("GOMODCACHE").map(PathBuf::from),
                Some(cache.path().join("go-mod"))
            );
            assert!(
                environment
                    .get("GOPROXY")
                    .is_some_and(|proxy| proxy.starts_with("file:///"))
            );
        } else {
            assert!(!environment.contains_key("GOMODCACHE"));
            assert!(!environment.contains_key("GOPROXY"));
        }
    }

    #[test]
    fn go_module_cache_uses_private_store_and_host_download_cache_as_file_proxy() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home with space");
        let host_download = home.join("go/pkg/mod/cache/download");
        let private = fixture.path().join("private-cache");
        std::fs::create_dir_all(&host_download).unwrap();
        std::fs::create_dir_all(&private).unwrap();
        let mut environment =
            HashMap::from([("HOME".to_owned(), home.to_string_lossy().into_owned())]);

        apply_go_module_cache_environment(&mut environment, &private).unwrap();

        assert_eq!(
            environment.get("GOMODCACHE").map(PathBuf::from),
            Some(private.join("go-mod"))
        );
        assert_eq!(
            environment.get("GOPROXY").map(String::as_str),
            Some(
                go_file_proxy_url(&std::fs::canonicalize(host_download).unwrap())
                    .unwrap()
                    .as_str()
            )
        );
        assert!(private.join("go-mod").is_dir());
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(private.join("go-mod"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn go_module_cache_remap_is_noop_without_a_host_download_cache() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let private = fixture.path().join("private-cache");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&private).unwrap();
        let mut environment =
            HashMap::from([("HOME".to_owned(), home.to_string_lossy().into_owned())]);

        apply_go_module_cache_environment(&mut environment, &private).unwrap();

        assert!(!environment.contains_key("GOMODCACHE"));
        assert!(!environment.contains_key("GOPROXY"));
    }

    #[test]
    fn command_cache_directory_is_private_and_removed_on_drop() {
        let cache = CommandCacheDir::create().unwrap();
        let path = cache.path().to_path_buf();
        assert!(path.is_absolute());
        assert!(path.is_dir());
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);

            let readonly_module = path.join("go-mod/example.com/module@v1.0.0");
            std::fs::create_dir_all(&readonly_module).unwrap();
            std::fs::write(
                readonly_module.join("go.mod"),
                b"module example.com/module\n",
            )
            .unwrap();
            std::fs::set_permissions(&readonly_module, std::fs::Permissions::from_mode(0o555))
                .unwrap();
        }
        drop(cache);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn command_cache_cleanup_does_not_follow_symlinks() {
        let outside = tempfile::tempdir().unwrap();
        let outside_path = outside.path().join("target");
        std::fs::create_dir(&outside_path).unwrap();
        std::fs::set_permissions(&outside_path, std::fs::Permissions::from_mode(0o500)).unwrap();

        let cache = CommandCacheDir::create().unwrap();
        let cache_path = cache.path().to_path_buf();
        std::os::unix::fs::symlink(&outside_path, cache.path().join("outside-link")).unwrap();
        drop(cache);

        assert!(!cache_path.exists());
        assert_eq!(
            std::fs::metadata(&outside_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o500
        );
        std::fs::set_permissions(&outside_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn local_agent_spawn_error_has_fixed_classification() {
        let error = anyhow::Error::new(LocalAgentSpawnError::new(
            std::io::Error::from_raw_os_error(libc::EPERM),
        ));
        assert!(is_local_agent_spawn_error(&error));
        assert_eq!(
            error.to_string(),
            "failed to start bounded local-agent command"
        );
        assert!(!is_local_agent_spawn_error(&anyhow::anyhow!(
            "sandbox setup failed"
        )));
    }

    #[test]
    fn generated_standard_cache_paths_stay_below_private_root() -> noprop::TestResult {
        test_support::run(0x4341_4348_4552_4f4f, 1024, |ctx| {
            let root = PathBuf::from(format!(
                "/tmp/temote-cache-{:016x}",
                noprop::sample_u64(ctx)
            ));
            for (name, path) in standard_cache_environment_paths(&root) {
                assert!(path.starts_with(&root), "{name} escaped private cache root");
                assert!(path.is_absolute(), "{name} cache path is not absolute");
            }
            let module_cache = root.join("go-mod");
            assert!(module_cache.starts_with(&root));
            assert!(module_cache.is_absolute());
            Ok(())
        })
    }

    #[test]
    fn generated_go_file_proxy_urls_escape_reserved_path_bytes() -> noprop::TestResult {
        test_support::run(0x474f_5052_4f58_5955, 1024, |ctx| {
            let path = PathBuf::from(format!(
                "/tmp/go proxy/%23-{:016x}",
                noprop::sample_u64(ctx)
            ));
            let url = go_file_proxy_url(&path).unwrap();
            assert!(url.starts_with("file:///tmp/"));
            assert!(!url.contains(' '));
            assert!(!url.contains('#'));
            assert!(url.contains("%20"));
            assert!(url.contains("%25"));
            Ok(())
        })
    }

    #[test]
    fn developer_tool_environment_forces_the_sandbox_marker() {
        let caller = HashMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), "/home/tester".to_owned()),
            // filtered_environment strips TEMOTE_MCP_* names, but force the
            // marker even if a caller supplies its own value.
            ("TEMOTE_MCP_SANDBOX".to_owned(), "caller".to_owned()),
        ]);
        let environment = developer_tool_environment(&caller);
        assert_eq!(
            environment.get("TEMOTE_MCP_SANDBOX").map(String::as_str),
            Some("1"),
            "developer-tool children must always carry the sandbox marker"
        );
        assert_eq!(
            environment.get("PATH").map(String::as_str),
            Some("/usr/bin:/bin"),
            "the filtered caller environment must be preserved"
        );
        assert_eq!(
            environment.get("HOME").map(String::as_str),
            Some("/home/tester")
        );
        assert_eq!(
            environment.len(),
            caller.len(),
            "the marker must not add or drop unrelated entries"
        );

        let mut without_marker = caller.clone();
        without_marker.remove("TEMOTE_MCP_SANDBOX");
        let environment = developer_tool_environment(&without_marker);
        assert_eq!(
            environment.get("TEMOTE_MCP_SANDBOX").map(String::as_str),
            Some("1")
        );
        assert_eq!(environment.len(), caller.len());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn untrusted_bwrap_candidate_is_rejected_even_when_executable() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        let fake = fixture.path().join("bwrap");
        std::fs::write(&fake, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(trusted_bwrap_candidate(&fake).unwrap().is_none());
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
    fn protected_metadata_walk_discovers_nested_entries_without_following_symlinks() -> Result<()> {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        let nested = workspace.join("nested");
        let deep = nested.join("deep");
        let ordinary = workspace.join("ordinary");
        std::fs::create_dir_all(workspace.join(".git/ignored"))?;
        std::fs::create_dir_all(nested.join(".git"))?;
        std::fs::create_dir_all(nested.join(".agents"))?;
        std::fs::create_dir_all(&deep)?;
        std::fs::write(deep.join(".codex"), b"metadata")?;
        std::fs::create_dir_all(&ordinary)?;
        std::fs::write(ordinary.join(".git"), b"gitdir: ../real-git")?;

        #[cfg(unix)]
        {
            let external = fixture.path().join("external");
            std::fs::create_dir_all(external.join(".codex"))?;
            std::os::unix::fs::symlink(&external, workspace.join("linked"))?;
        }

        let workspace = std::fs::canonicalize(workspace)?;
        let paths = discover_protected_metadata_paths(&workspace)?;
        for expected in [
            workspace.join(".git"),
            workspace.join(".agents"),
            workspace.join(".codex"),
            workspace.join("nested/.git"),
            workspace.join("nested/.agents"),
            workspace.join("nested/deep/.codex"),
            workspace.join("ordinary/.git"),
        ] {
            assert!(
                paths.contains(&expected),
                "missing protected path {expected:?}"
            );
        }
        assert!(
            !paths
                .iter()
                .any(|path| path.ends_with(".git/ignored/.codex")),
            "protected metadata directories must not be traversed"
        );
        #[cfg(unix)]
        assert!(
            !paths.iter().any(|path| path.ends_with("external/.codex")),
            "symbolic-link directories must not be followed"
        );
        Ok(())
    }

    #[test]
    fn protected_metadata_walk_falls_back_to_an_unscanned_nested_subtree() -> Result<()> {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        let fallback = workspace.join("large");
        std::fs::create_dir_all(&fallback)?;
        std::fs::create_dir_all(fallback.join("nested/.git"))?;
        std::fs::write(fallback.join("entry-0"), b"0")?;
        std::fs::write(fallback.join("entry-1"), b"1")?;

        let workspace = std::fs::canonicalize(workspace)?;
        let fallback = workspace.join("large");
        let paths = discover_protected_metadata_paths_with_limits(
            &workspace,
            ProtectedMetadataScanLimits {
                // Keep the root deterministic: it contains only the subtree
                // that must be replaced by a read-only fallback.
                max_entries: 1,
                max_depth: 64,
                max_paths: MAX_LOCAL_AGENT_PROTECTED_METADATA_PATHS,
            },
        )?;

        assert!(
            paths.contains(&fallback),
            "an unscanned nested subtree must become read-only"
        );
        assert!(
            !paths
                .iter()
                .any(|path| path.starts_with(&fallback) && path != &fallback),
            "fallback should replace narrower paths below the subtree"
        );
        Ok(())
    }

    #[test]
    fn protected_metadata_walk_falls_back_at_its_depth_bound() -> Result<()> {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let mut current = workspace.clone();
        for index in 0..=2 {
            current.push(format!("level-{index}"));
            std::fs::create_dir(&current)?;
        }

        let workspace = std::fs::canonicalize(workspace)?;
        let paths = discover_protected_metadata_paths_with_limits(
            &workspace,
            ProtectedMetadataScanLimits {
                max_entries: 64,
                max_depth: 1,
                max_paths: MAX_LOCAL_AGENT_PROTECTED_METADATA_PATHS,
            },
        )?;
        assert!(paths.contains(&workspace.join("level-0/level-1")));
        Ok(())
    }

    #[test]
    fn protected_metadata_walk_fails_closed_when_a_root_bound_cannot_be_represented() -> Result<()>
    {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        for index in 0..3 {
            std::fs::create_dir(workspace.join(format!("entry-{index}")))?;
        }

        let limits = ProtectedMetadataScanLimits {
            max_entries: 2,
            max_depth: 64,
            max_paths: MAX_LOCAL_AGENT_PROTECTED_METADATA_PATHS,
        };
        assert!(
            discover_protected_metadata_paths_with_limits(&workspace, limits).is_err(),
            "an over-budget writable root must fail closed"
        );
        assert!(
            discover_protected_metadata_paths_with_limits(
                &workspace,
                ProtectedMetadataScanLimits {
                    max_entries: 64,
                    max_depth: 64,
                    max_paths: 2,
                }
            )
            .is_err(),
            "a path budget that cannot retain top-level masks must fail closed"
        );
        Ok(())
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
    fn worktree_add_scope_protects_siblings_but_not_current_private_metadata() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        let common = repository.join(".git");
        let current = common.join("worktrees").join("current");
        let sibling = common.join("worktrees").join("sibling");
        let worktree = root.path().join("worktree");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", current.display()),
        )
        .unwrap();
        std::fs::write(current.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            current.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();

        let roots = git_metadata_roots(&worktree).unwrap();
        let protected = protected_git_worktree_metadata_roots(&roots).unwrap();

        assert_eq!(protected, vec![std::fs::canonicalize(&sibling).unwrap()]);
        assert!(!protected.contains(&std::fs::canonicalize(&current).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn worktree_add_scope_rejects_unexpected_worktree_metadata_entries() {
        let repository = tempfile::tempdir().unwrap();
        let git = repository.path().join(".git");
        let worktrees = git.join("worktrees");
        std::fs::create_dir_all(&worktrees).unwrap();
        std::fs::write(worktrees.join("unexpected-file"), b"metadata").unwrap();

        let common = std::fs::canonicalize(&git).unwrap();
        let error = protected_git_worktree_metadata_roots(&[common]).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unexpected non-directory Git worktree metadata entry")
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
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use uuid::Uuid;

    fn test_root() -> tempfile::TempDir {
        tempfile::tempdir_in("/var/tmp").expect("/var/tmp is required for Linux sandbox tests")
    }

    fn command(program: &str, args: &[&str]) -> Vec<String> {
        std::iter::once(program.to_owned())
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .collect()
    }

    fn unix_socket_address(
        name: &[u8],
        abstract_namespace: bool,
    ) -> Result<(libc::sockaddr_un, libc::socklen_t)> {
        anyhow::ensure!(!name.is_empty(), "Unix socket name must not be empty");
        let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let start = usize::from(abstract_namespace);
        anyhow::ensure!(
            name.len() + start < address.sun_path.len(),
            "Unix socket name is too long for the test address"
        );
        // SAFETY: `address` is zero-initialized, `sun_path` has enough room
        // for the requested bytes, and both source and destination do not
        // overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                address.sun_path.as_mut_ptr().cast::<u8>().add(start),
                name.len(),
            );
        }
        let length = std::mem::offset_of!(libc::sockaddr_un, sun_path)
            + start
            + name.len()
            + usize::from(!abstract_namespace);
        Ok((address, length as libc::socklen_t))
    }

    fn bind_abstract_listener(name: &[u8]) -> Result<OwnedFd> {
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        anyhow::ensure!(
            fd >= 0,
            "failed to create dummy abstract Unix listener: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `fd` is a newly-created, owned file descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let (address, length) = unix_socket_address(name, true)?;
        // SAFETY: `address` remains alive for the duration of the syscall and
        // `length` describes the initialized abstract socket address.
        let result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
                length,
            )
        };
        anyhow::ensure!(
            result == 0,
            "failed to bind dummy abstract Unix listener: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `fd` is a valid stream socket owned by this test.
        let result = unsafe { libc::listen(fd.as_raw_fd(), 1) };
        anyhow::ensure!(
            result == 0,
            "failed to listen on dummy abstract Unix socket: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `fd` is valid and owned by this test; changing its status
        // flags does not transfer ownership or create an alias.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        anyhow::ensure!(
            flags >= 0,
            "failed to inspect dummy abstract Unix listener flags: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `fd` is valid and the flags were obtained immediately above.
        let result =
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        anyhow::ensure!(
            result == 0,
            "failed to make dummy abstract Unix listener nonblocking: {}",
            std::io::Error::last_os_error()
        );
        Ok(fd)
    }

    fn allowed_stream_socketpair(socket_type: i32) -> Result<(OwnedFd, OwnedFd)> {
        let mut fds = [-1; 2];
        let result = unsafe { libc::socketpair(libc::AF_UNIX, socket_type, 0, fds.as_mut_ptr()) };
        anyhow::ensure!(
            result == 0,
            "required stream socketpair was denied: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: a successful socketpair call initialized two distinct,
        // owned file descriptors in `fds`.
        let first = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: see the ownership argument for `first`; `fds[1]` is the
        // other descriptor returned by the same successful call.
        let second = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        Ok((first, second))
    }

    fn assert_socket_denied(socket_type: i32) -> Result<()> {
        let fd = unsafe { libc::socket(libc::AF_UNIX, socket_type, 0) };
        anyhow::ensure!(
            fd == -1,
            "AF_UNIX socket unexpectedly succeeded for type {socket_type:#x}"
        );
        let error = std::io::Error::last_os_error();
        anyhow::ensure!(
            error.raw_os_error() == Some(libc::EPERM),
            "AF_UNIX socket failed with an unexpected error: {error}"
        );
        Ok(())
    }

    fn assert_socketpair_denied(domain: i32, socket_type: i32, protocol: i32) -> Result<()> {
        let mut fds = [-1; 2];
        let result = unsafe { libc::socketpair(domain, socket_type, protocol, fds.as_mut_ptr()) };
        anyhow::ensure!(
            result == -1,
            "socketpair unexpectedly succeeded for domain {domain} type {socket_type:#x} protocol {protocol}"
        );
        let error = std::io::Error::last_os_error();
        anyhow::ensure!(
            error.raw_os_error() == Some(libc::EPERM),
            "socketpair failed with an unexpected error: {error}"
        );
        Ok(())
    }

    fn assert_socketpair_cannot_connect(
        address: &libc::sockaddr_un,
        length: libc::socklen_t,
        label: &str,
    ) -> Result<()> {
        let (first, _second) = allowed_stream_socketpair(
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
        )?;
        // SAFETY: `address` is initialized by `unix_socket_address` and lives
        // through this syscall; `first` is a valid connected stream socket.
        let result = unsafe {
            libc::connect(
                first.as_raw_fd(),
                (address as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
                length,
            )
        };
        anyhow::ensure!(
            result == -1,
            "allowed stream socketpair connected to {label}"
        );
        let error = std::io::Error::last_os_error();
        anyhow::ensure!(
            error.raw_os_error() == Some(libc::EISCONN),
            "connecting an already-connected stream socketpair to {label} returned an unexpected error: {error}"
        );
        Ok(())
    }

    fn assert_no_path_listener_connection(listener: &UnixListener) -> Result<()> {
        match listener.accept() {
            Ok(_) => anyhow::bail!("sandbox unexpectedly reached the pathname Unix socket"),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn assert_no_abstract_listener_connection(listener: &OwnedFd) -> Result<()> {
        // SAFETY: `listener` is a valid nonblocking listening socket and null
        // address arguments intentionally discard any accepted peer address.
        let fd = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            )
        };
        anyhow::ensure!(
            fd == -1,
            "sandbox unexpectedly reached the abstract Unix socket"
        );
        let error = std::io::Error::last_os_error();
        anyhow::ensure!(
            error.kind() == std::io::ErrorKind::WouldBlock,
            "abstract Unix listener returned an unexpected error: {error}"
        );
        Ok(())
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
    async fn linux_structured_worktree_add_can_create_without_sibling_metadata_write() -> Result<()>
    {
        let root = test_root();
        let repository = root.path().join("repository");
        let sibling = root.path().join("sibling");
        let destination = repository.join(".wt").join("review");
        std::fs::create_dir_all(&repository)?;
        std::fs::create_dir_all(repository.join(".wt"))?;
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
                "sibling",
                sibling.to_str().unwrap(),
            ],
        )?;

        let git_roots = git_metadata_roots(&repository)?;
        let protected = protected_git_worktree_metadata_roots(&git_roots)?;
        assert_eq!(protected.len(), 1);
        let sibling_metadata = protected[0].clone();
        let sibling_marker = sibling_metadata.join("temote-protected-marker");

        let denied = run_git_worktree_add(
            &command("/usr/bin/touch", &[sibling_marker.to_str().unwrap()]),
            &repository,
            std::slice::from_ref(&root.path().to_path_buf()),
            &git_roots,
            None,
        )
        .await?;
        assert_ne!(denied.status, 0);
        assert!(!sibling_marker.exists());

        let add = vec![
            "/usr/bin/git".to_owned(),
            "-c".to_owned(),
            "core.hooksPath=/dev/null".to_owned(),
            "worktree".to_owned(),
            "add".to_owned(),
            "-b".to_owned(),
            "review".to_owned(),
            destination.to_string_lossy().into_owned(),
            "HEAD".to_owned(),
        ];
        let output = run_git_worktree_add(
            &add,
            &repository,
            std::slice::from_ref(&root.path().to_path_buf()),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert_eq!(
            std::process::Command::new("/usr/bin/git")
                .args(["branch", "--show-current"])
                .current_dir(&destination)
                .output()?
                .stdout,
            b"review\n"
        );
        assert!(sibling_metadata.is_dir());
        assert!(!sibling_marker.exists());
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

    #[tokio::test]
    async fn linux_local_agent_seccomp_allows_runtime_stream_pair_only() -> Result<()> {
        const ROLE: &str = "TEMOTE_TEST_LOCAL_AGENT_SOCKETPAIR_ROLE";
        const PATH_SOCKET: &str = "TEMOTE_TEST_LOCAL_AGENT_PATH_SOCKET";
        const ABSTRACT_SOCKET: &str = "TEMOTE_TEST_LOCAL_AGENT_ABSTRACT_SOCKET";
        const TEST_NAME: &str =
            "sandbox::linux_tests::linux_local_agent_seccomp_allows_runtime_stream_pair_only";
        const STREAM_TYPE: i32 = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;

        if std::env::var(ROLE).as_deref() == Ok("fixture") {
            let pathname = PathBuf::from(
                std::env::var(PATH_SOCKET).context("pathname Unix socket path is missing")?,
            );
            let abstract_name =
                std::env::var(ABSTRACT_SOCKET).context("abstract Unix socket name is missing")?;

            let (_blocking_first, _blocking_second) =
                allowed_stream_socketpair(libc::SOCK_STREAM | libc::SOCK_CLOEXEC)?;
            let (_nonblocking_first, _nonblocking_second) = allowed_stream_socketpair(STREAM_TYPE)?;
            assert_socket_denied(STREAM_TYPE)?;
            assert_socket_denied(libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)?;
            assert_socketpair_denied(
                libc::AF_UNIX,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )?;
            assert_socketpair_denied(libc::AF_UNIX, libc::SOCK_STREAM, 0)?;
            assert_socketpair_denied(libc::AF_UNIX, STREAM_TYPE, 1)?;
            assert_socketpair_denied(libc::AF_INET, STREAM_TYPE, 0)?;

            if UnixStream::connect(&pathname).is_ok() {
                anyhow::bail!("pathname Unix socket creation unexpectedly succeeded");
            }

            let (pathname_address, pathname_length) =
                unix_socket_address(pathname.as_os_str().as_bytes(), false)?;
            assert_socketpair_cannot_connect(
                &pathname_address,
                pathname_length,
                "the pathname Unix socket",
            )?;
            let (abstract_address, abstract_length) =
                unix_socket_address(abstract_name.as_bytes(), true)?;
            assert_socketpair_cannot_connect(
                &abstract_address,
                abstract_length,
                "the abstract Unix socket",
            )?;
            return Ok(());
        }

        let root = test_root();
        let workspace = root.path().join("workspace");
        let state = root.path().join("state");
        let state_tmp = state.join("tmp");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&state_tmp)?;

        // Keep both dummy receivers outside the hidden policy root. If a
        // broader socket rule accidentally permits a path-based connection,
        // these listeners observe it without touching a host service.
        let listener_root = test_root();
        let pathname = listener_root.path().join("host-only.sock");
        let pathname_listener = UnixListener::bind(&pathname)?;
        pathname_listener.set_nonblocking(true)?;
        let abstract_name = format!("temote-test-{}", Uuid::new_v4());
        let abstract_listener = bind_abstract_listener(abstract_name.as_bytes())?;

        let current_exe = std::env::current_exe()?;
        let current_exe = current_exe
            .to_str()
            .context("current test executable path is not UTF-8")?;
        let child_command = command(current_exe, &["--exact", TEST_NAME, "--nocapture"]);
        let environment = HashMap::from([
            (ROLE.to_owned(), "fixture".to_owned()),
            (
                PATH_SOCKET.to_owned(),
                pathname.to_string_lossy().into_owned(),
            ),
            (ABSTRACT_SOCKET.to_owned(), abstract_name),
            ("HOME".to_owned(), state.to_string_lossy().into_owned()),
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            (
                "TMPDIR".to_owned(),
                state_tmp.to_string_lossy().into_owned(),
            ),
        ]);
        let hidden_root = root.path().to_path_buf();
        let output = run_local_agent(
            &child_command,
            &workspace,
            LocalAgentScope {
                writable_roots: std::slice::from_ref(&state),
                temporary_roots: std::slice::from_ref(&state_tmp),
                read_only_paths: &[],
                read_only_roots: std::slice::from_ref(&workspace),
                read_only_symlinks: &[],
                read_only_scaffold_directories: &[],
                read_only_files: &[],
                hidden_roots: std::slice::from_ref(&hidden_root),
            },
            None,
            &environment,
        )
        .await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert_no_path_listener_connection(&pathname_listener)?;
        assert_no_abstract_listener_connection(&abstract_listener)?;
        Ok(())
    }

    #[tokio::test]
    async fn linux_local_agent_profile_bounds_workspace_writes() -> Result<()> {
        let root = test_root();
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        let state = root.path().join("state");
        let state_tmp = state.join("tmp");
        let git = workspace.join(".git");
        let agents = workspace.join(".agents");
        let codex = workspace.join(".codex");
        let nested_git = workspace.join("nested/.git");
        let nested_agents = workspace.join("nested/.agents");
        let nested_codex = workspace.join("nested/deep/.codex");
        let ordinary_git = workspace.join("ordinary/.git");
        std::fs::create_dir_all(&git)?;
        std::fs::create_dir_all(&agents)?;
        std::fs::create_dir_all(&codex)?;
        std::fs::create_dir_all(&nested_git)?;
        std::fs::create_dir_all(&nested_agents)?;
        std::fs::create_dir_all(nested_codex.parent().context("nested .codex parent")?)?;
        std::fs::create_dir_all(ordinary_git.parent().context("ordinary .git parent")?)?;
        std::fs::create_dir_all(&outside)?;
        std::fs::create_dir_all(&state_tmp)?;
        std::fs::write(nested_git.join("existing"), b"protected")?;
        std::fs::write(nested_agents.join("existing"), b"protected")?;
        std::fs::write(&nested_codex, b"protected")?;
        std::fs::write(&ordinary_git, b"protected")?;
        let outside_secret = outside.join("secret");
        std::fs::write(&outside_secret, b"not visible to the agent")?;

        let state_file = state.join("state-file");
        let state_file_argument = state_file.to_str().context("state path is not UTF-8")?;
        let hidden_root = root.path().to_path_buf();
        let write_script = "set -eu; printf allowed > allowed; if printf outside > ../outside/denied; then exit 11; fi; if printf git > .git/denied; then exit 12; fi; if printf agents > .agents/denied; then exit 13; fi; if printf codex > .codex/denied; then exit 14; fi; if printf nested-git > nested/.git/denied; then exit 15; fi; if printf nested-agents > nested/.agents/denied; then exit 16; fi; if printf nested-codex > nested/deep/.codex; then exit 17; fi; if printf ordinary-git > ordinary/.git; then exit 18; fi; printf state > \"$1\"; test -f allowed; test ! -e ../outside/denied; test ! -e .git/denied; test ! -e .agents/denied; test ! -e .codex/denied; test ! -e nested/.git/denied; test ! -e nested/.agents/denied; test \"$(cat nested/deep/.codex)\" = protected; test \"$(cat ordinary/.git)\" = protected; test -f \"$1\"";
        let write_command = command(
            "/bin/sh",
            &["-c", write_script, "local-agent", state_file_argument],
        );
        let environment = HashMap::from([
            ("HOME".to_owned(), state.to_string_lossy().into_owned()),
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            (
                "TMPDIR".to_owned(),
                state_tmp.to_string_lossy().into_owned(),
            ),
        ]);
        let write = run_local_agent(
            &write_command,
            &workspace,
            LocalAgentScope {
                writable_roots: &[workspace.clone(), state.clone()],
                temporary_roots: std::slice::from_ref(&state_tmp),
                read_only_paths: &[],
                read_only_roots: &[],
                read_only_symlinks: &[],
                read_only_scaffold_directories: &[],
                read_only_files: &[],
                hidden_roots: std::slice::from_ref(&hidden_root),
            },
            None,
            &environment,
        )
        .await?;
        assert_eq!(write.status, 0, "{}", write.stderr);
        assert_eq!(std::fs::read(nested_git.join("existing"))?, b"protected");
        assert_eq!(std::fs::read(nested_agents.join("existing"))?, b"protected");
        assert_eq!(std::fs::read(&nested_codex)?, b"protected");
        assert_eq!(std::fs::read(&ordinary_git)?, b"protected");

        let outside_secret_argument = outside_secret
            .to_str()
            .context("outside secret path is not UTF-8")?;
        let read_outside_command = command(
            "/bin/sh",
            &[
                "-c",
                "if cat \"$1\" >/dev/null 2>&1; then exit 31; else exit 0; fi",
                "local-agent",
                outside_secret_argument,
            ],
        );
        let read_outside = run_local_agent(
            &read_outside_command,
            &workspace,
            LocalAgentScope {
                writable_roots: std::slice::from_ref(&state),
                temporary_roots: std::slice::from_ref(&state_tmp),
                read_only_paths: &[],
                read_only_roots: std::slice::from_ref(&workspace),
                read_only_symlinks: &[],
                read_only_scaffold_directories: &[],
                read_only_files: &[],
                hidden_roots: std::slice::from_ref(&hidden_root),
            },
            None,
            &environment,
        )
        .await?;
        assert_eq!(
            read_outside.status, 0,
            "outside read was unexpectedly visible: stdout={:?} stderr={:?}",
            read_outside.stdout, read_outside.stderr
        );

        std::fs::remove_file(workspace.join("allowed"))?;
        std::fs::remove_file(&state_file)?;
        let read_only_script = "set -eu; if printf denied > allowed; then exit 21; fi; printf state > \"$1\"; test ! -e allowed; test -f \"$1\"";
        let read_command = command(
            "/bin/sh",
            &["-c", read_only_script, "local-agent", state_file_argument],
        );
        let read_only = run_local_agent(
            &read_command,
            &workspace,
            LocalAgentScope {
                writable_roots: std::slice::from_ref(&state),
                temporary_roots: std::slice::from_ref(&state_tmp),
                read_only_paths: &[],
                read_only_roots: std::slice::from_ref(&workspace),
                read_only_symlinks: &[],
                read_only_scaffold_directories: &[],
                read_only_files: &[],
                hidden_roots: std::slice::from_ref(&hidden_root),
            },
            None,
            &environment,
        )
        .await?;
        assert_eq!(read_only.status, 0, "{}", read_only.stderr);

        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let accepted = std::thread::spawn(move || {
            for _ in 0..400 {
                match listener.accept() {
                    Ok(_) => return true,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => return false,
                }
            }
            false
        });
        let network_script = format!(
            "exec 3<>/dev/tcp/127.0.0.1/{}; printf connected >&3",
            address.port()
        );
        let network_command = command("/bin/bash", &["-c", &network_script]);
        let network = run_local_agent(
            &network_command,
            &workspace,
            LocalAgentScope {
                writable_roots: std::slice::from_ref(&state),
                temporary_roots: std::slice::from_ref(&state_tmp),
                read_only_paths: &[],
                read_only_roots: std::slice::from_ref(&workspace),
                read_only_symlinks: &[],
                read_only_scaffold_directories: &[],
                read_only_files: &[],
                hidden_roots: std::slice::from_ref(&hidden_root),
            },
            None,
            &environment,
        )
        .await?;
        assert_eq!(network.status, 0, "{}", network.stderr);
        assert!(accepted.join().unwrap());
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
    async fn seatbelt_structured_worktree_add_can_create_without_sibling_metadata_write()
    -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
        {
            return Ok(());
        }
        let root = test_directory();
        let repository = root.join("repository");
        let sibling = root.join("sibling");
        let destination = repository.join(".wt").join("review");
        std::fs::create_dir_all(repository.join(".wt"))?;

        let git = |args: &[&str]| -> Result<()> {
            let output = std::process::Command::new("/usr/bin/git")
                .args(args)
                .current_dir(&repository)
                .output()?;
            anyhow::ensure!(
                output.status.success(),
                "host git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(())
        };
        git(&["init", "-q"])?;
        std::fs::write(repository.join("base.txt"), b"base\n")?;
        git(&["add", "--", "base.txt"])?;
        git(&[
            "-c",
            "user.name=temote-mcp test",
            "-c",
            "user.email=temote-mcp@example.invalid",
            "commit",
            "-q",
            "-m",
            "base",
        ])?;
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "sibling",
            sibling.to_str().unwrap(),
        ])?;

        let git_roots = git_metadata_roots(&repository)?;
        let protected = protected_git_worktree_metadata_roots(&git_roots)?;
        assert_eq!(protected.len(), 1);
        let sibling_metadata = protected[0].clone();
        let sibling_marker = sibling_metadata.join("temote-protected-marker");
        let denied = run_git_worktree_add(
            &[
                "/usr/bin/touch".to_owned(),
                sibling_marker.to_string_lossy().into_owned(),
            ],
            &repository,
            std::slice::from_ref(&root),
            &git_roots,
            None,
        )
        .await?;
        assert_ne!(denied.status, 0);
        assert!(!sibling_marker.exists());

        let add = vec![
            "/usr/bin/git".to_owned(),
            "-c".to_owned(),
            "core.hooksPath=/dev/null".to_owned(),
            "worktree".to_owned(),
            "add".to_owned(),
            "-b".to_owned(),
            "review".to_owned(),
            destination.to_string_lossy().into_owned(),
            "HEAD".to_owned(),
        ];
        let output = run_git_worktree_add(
            &add,
            &repository,
            std::slice::from_ref(&root),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        let branch = std::process::Command::new("/usr/bin/git")
            .args(["branch", "--show-current"])
            .current_dir(&destination)
            .output()?;
        assert!(branch.status.success());
        assert_eq!(branch.stdout, b"review\n");
        assert!(sibling_metadata.is_dir());
        assert!(!sibling_marker.exists());

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
