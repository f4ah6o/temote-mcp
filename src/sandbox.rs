use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};

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

/// Network policy for ordinary sandboxed commands (`execute` / `start_command`).
///
/// This is the single typed decision shared by both tools. It is deliberately
/// not a caller-selectable policy: the session permission mode selects it, and
/// no MCP input can widen it. `Restricted` denies outbound network in the
/// sandbox; `Development` reuses the existing network-enabled sandbox profile
/// so ordinary development commands can reach localhost, LAN, and Internet
/// endpoints while filesystem/path containment stays in force.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandNetworkPolicy {
    Restricted,
    Development,
}

impl CommandNetworkPolicy {
    pub const fn development_enabled(self) -> bool {
        matches!(self, Self::Development)
    }
}

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
    /// When set, `run_local_agent` re-derives the repository identity of the
    /// canonical cwd immediately before spawning the child and fails closed
    /// unless it equals this validated expected identity.
    pub expected_repository: Option<&'a WorkspaceRepositoryIdentity>,
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
    run_with_network_policy(
        command,
        cwd,
        writable_roots,
        CommandNetworkPolicy::Restricted,
        stdin,
    )
    .await
}

/// Runs an ordinary sandboxed command with an explicit network policy.
///
/// Callers pass the decision produced by the session permission mode; the
/// filesystem/path containment and protected-metadata handling are identical
/// for both network policies.
pub async fn run_with_network_policy(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    network: CommandNetworkPolicy,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_with_metadata_roots(command, cwd, writable_roots, &[], None, network, stdin, &[]).await
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
    // The sandbox command and its policy are fully constructed above. If this
    // launch is bound to a validated managed worktree, re-derive the canonical
    // repository identity of the canonical cwd at the last possible point and
    // refuse to spawn when the filesystem no longer presents that identity.
    if let Some(expected) = scope.expected_repository {
        let observed = WorkspaceRepositoryIdentity::for_workspace(&cwd).context(
            "validated managed worktree identity is no longer a supported Git worktree root",
        )?;
        anyhow::ensure!(
            observed == *expected,
            "local agent workspace managed worktree identity changed before spawn: {}",
            cwd.display()
        );
    }
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

/// No-follow filesystem primitives for host-backed Git staging.
///
/// Every staging operation is anchored to an already-held directory
/// descriptor and uses descriptor-relative calls, so a pathname swap cannot
/// redirect a write or a lock removal to an unrelated directory. Symlinks,
/// special files and unexpected entry types fail closed instead of being
/// followed.
#[cfg(target_os = "linux")]
mod staging_fs {
    use super::*;
    use std::ffi::CString;
    use std::fs::File;
    use std::io::{Read as _, Write as _};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    pub(super) struct RegularEntry {
        pub(super) bytes: Vec<u8>,
        pub(super) mode: u32,
    }

    fn cstring(value: &str, label: &str) -> Result<CString> {
        CString::new(value).with_context(|| format!("{label} contains a NUL byte"))
    }

    fn path_cstring(path: &Path, label: &str) -> Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("{label} contains a NUL byte: {}", path.display()))
    }

    pub(super) fn stat_is_directory(stat: &libc::stat) -> bool {
        stat.st_mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub(super) fn stat_is_regular_file(stat: &libc::stat) -> bool {
        stat.st_mode & libc::S_IFMT == libc::S_IFREG
    }

    pub(super) fn file_identity(file: &File) -> Result<(u64, u64)> {
        let metadata = file
            .metadata()
            .context("cannot inspect an open staging directory")?;
        Ok((metadata.dev(), metadata.ino()))
    }

    fn stat_descriptor(
        parent: libc::c_int,
        name: &CString,
        label: &str,
    ) -> Result<Option<libc::stat>> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `parent` is an open directory descriptor owned by the
        // caller, `name` is a valid NUL-terminated name, and `stat` points to
        // writable memory of the exact type the kernel fills in.
        let result = unsafe {
            libc::fstatat(
                parent,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            // SAFETY: a successful `fstatat` initialized the whole structure.
            return Ok(Some(unsafe { stat.assume_init() }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        Err(error).with_context(|| format!("cannot inspect {label}"))
    }

    pub(super) fn stat_entry_at(
        parent: &File,
        name: &str,
        label: &str,
    ) -> Result<Option<libc::stat>> {
        let name = cstring(name, label)?;
        stat_descriptor(parent.as_raw_fd(), &name, label)
    }

    pub(super) fn stat_path_no_follow(path: &Path, label: &str) -> Result<Option<libc::stat>> {
        let name = path_cstring(path, label)?;
        stat_descriptor(
            libc::AT_FDCWD,
            &name,
            &format!("{label} {}", path.display()),
        )
    }

    pub(super) fn open_directory_no_follow(path: &Path, label: &str) -> Result<File> {
        let name = path_cstring(path, label)?;
        // SAFETY: `name` is a valid NUL-terminated path and the returned
        // descriptor is immediately wrapped in an owning `File`.
        let descriptor = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot open {label} {}", path.display()));
        }
        // SAFETY: `descriptor` is a fresh, owned descriptor from `open`.
        let file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file
            .metadata()
            .with_context(|| format!("cannot inspect {label} {}", path.display()))?;
        anyhow::ensure!(
            metadata.is_dir(),
            "{label} is not a directory: {}",
            path.display()
        );
        Ok(file)
    }

    pub(super) fn open_directory_at(
        parent: &File,
        name: &str,
        label: &str,
    ) -> Result<Option<File>> {
        let c_name = cstring(name, label)?;
        // SAFETY: `parent` is an open directory descriptor owned by the
        // caller, `name` is a valid NUL-terminated entry name, and the
        // returned descriptor is immediately wrapped in an owning `File`.
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error).with_context(|| format!("cannot open {label} {name:?}"));
        }
        // SAFETY: `descriptor` is a fresh, owned descriptor from `openat`.
        Ok(Some(unsafe { File::from_raw_fd(descriptor) }))
    }

    pub(super) fn create_private_directory(path: &Path) -> Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path).with_context(|| {
            format!(
                "cannot create private Git staging directory {}",
                path.display()
            )
        })?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "cannot protect private Git staging directory {}",
                path.display()
            )
        })
    }

    pub(super) fn create_directory_at(
        parent: &File,
        name: &str,
        mode: u32,
        label: &str,
    ) -> Result<()> {
        let c_name = cstring(name, label)?;
        // SAFETY: `parent` is an open directory descriptor owned by the
        // caller and `name` is a valid NUL-terminated entry name.
        let created =
            unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), mode as libc::mode_t) };
        if created != 0 {
            let error = std::io::Error::last_os_error();
            anyhow::ensure!(
                error.kind() == std::io::ErrorKind::AlreadyExists,
                "cannot create {label}: {error}"
            );
        }
        let directory = open_directory_at(parent, name, label)?
            .with_context(|| format!("{label} disappeared after creation"))?;
        drop(directory);
        Ok(())
    }

    pub(super) fn open_lock_file(parent: &File, name: &str) -> std::io::Result<File> {
        let Ok(name) = cstring(name, "Git lock name") else {
            return Err(std::io::Error::other("Git lock name contains a NUL byte"));
        };
        // SAFETY: `parent` is an open directory descriptor owned by the
        // caller, `name` is a valid NUL-terminated entry name, and the
        // returned descriptor is immediately wrapped in an owning `File`.
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o644,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `descriptor` is a fresh, owned descriptor from `openat`.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    pub(super) fn read_regular_entry_at(
        parent: &File,
        name: &str,
        label: &str,
    ) -> Result<Option<RegularEntry>> {
        let name = cstring(name, label)?;
        // SAFETY: `parent` is an open directory descriptor owned by the
        // caller, `name` is a valid NUL-terminated entry name, and the
        // returned descriptor is immediately wrapped in an owning `File`.
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error).with_context(|| format!("cannot open {label}"));
        }
        // SAFETY: `descriptor` is a fresh, owned descriptor from `openat`.
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file
            .metadata()
            .with_context(|| format!("cannot inspect {label}"))?;
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "{label} is not a regular file"
        );
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {label}"))?;
        Ok(Some(RegularEntry {
            bytes,
            mode: metadata.mode() & 0o7777,
        }))
    }

    fn set_file_mode(file: &File, mode: u32, label: &str) -> Result<()> {
        // SAFETY: `file` is an open descriptor owned by the caller.
        let result = unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot set permissions for {label}"));
        }
        Ok(())
    }

    pub(super) fn write_new_regular_entry_at(
        parent: &File,
        name: &str,
        bytes: &[u8],
        mode: u32,
        label: &str,
    ) -> Result<()> {
        let name = cstring(name, label)?;
        // SAFETY: `parent` is an open directory descriptor owned by the
        // caller, `name` is a valid NUL-terminated entry name, and the
        // returned descriptor is immediately wrapped in an owning `File`.
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode as libc::mode_t,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot create {label}"));
        }
        // SAFETY: `descriptor` is a fresh, owned descriptor from `openat`.
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        file.write_all(bytes)
            .with_context(|| format!("cannot write {label}"))?;
        file.flush()
            .with_context(|| format!("cannot flush {label}"))?;
        set_file_mode(&file, mode, label)
    }

    pub(super) fn replace_regular_entry_atomic_at(
        parent: &File,
        name: &str,
        bytes: &[u8],
        mode: u32,
        label: &str,
        inject_failure: bool,
    ) -> Result<()> {
        let temporary = format!(".temote-git-sync-{}", uuid::Uuid::new_v4());
        let temporary_name = cstring(&temporary, "staged Git temporary name")?;
        let target_name = cstring(name, label)?;
        // SAFETY: `parent` is an open directory descriptor owned by the
        // caller, both names are valid NUL-terminated entry names, and the
        // returned descriptor is immediately wrapped in an owning `File`.
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                temporary_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot create a temporary file for {label}"));
        }
        // SAFETY: `descriptor` is a fresh, owned descriptor from `openat`.
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let result = (|| -> Result<()> {
            file.write_all(bytes)
                .with_context(|| format!("cannot write a temporary file for {label}"))?;
            file.flush()
                .with_context(|| format!("cannot flush a temporary file for {label}"))?;
            set_file_mode(&file, mode, label)?;
            if inject_failure {
                anyhow::bail!("injected staged Git apply failure for {label}");
            }
            // SAFETY: both descriptors are open directories owned by the
            // caller and both names are valid NUL-terminated entry names in
            // the target directory; `renameat` replaces atomically.
            let renamed = unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    temporary_name.as_ptr(),
                    parent.as_raw_fd(),
                    target_name.as_ptr(),
                )
            };
            if renamed != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("cannot publish {label}"));
            }
            Ok(())
        })();
        if result.is_err() {
            // SAFETY: the temporary descriptor directory is still open and
            // the name is a valid NUL-terminated entry name.
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), temporary_name.as_ptr(), 0);
            }
        }
        result
    }

    pub(super) fn unlink_entry_if_identity(
        parent: &File,
        name: &str,
        device: u64,
        inode: u64,
        label: &str,
    ) -> bool {
        let Ok(Some(stat)) = stat_entry_at(parent, name, label) else {
            return false;
        };
        if !stat_is_regular_file(&stat) || stat.st_dev != device || stat.st_ino != inode {
            return false;
        }
        let Ok(name) = cstring(name, label) else {
            return false;
        };
        // SAFETY: `parent` is an open directory descriptor owned by the
        // caller and `name` is a valid NUL-terminated entry name whose
        // identity was just verified.
        unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) == 0 }
    }

    fn read_directory_names(directory: &File) -> Vec<(CString, bool)> {
        let mut names = Vec::new();
        // SAFETY: `directory` is an open directory descriptor owned by the
        // caller; `fcntl` duplicates it with close-on-exec.
        let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            return names;
        }
        // SAFETY: `duplicate` is a fresh descriptor whose ownership is
        // transferred to the directory stream.
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            // SAFETY: `duplicate` was not consumed and is still owned here.
            unsafe {
                libc::close(duplicate);
            }
            return names;
        }
        loop {
            // SAFETY: `stream` is a valid directory stream until `closedir`.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            // SAFETY: `readdir` returned a non-null entry with a
            // NUL-terminated name valid until the next `readdir`.
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            let Ok(name) = name.to_str() else {
                continue;
            };
            if name == "." || name == ".." {
                continue;
            }
            let Ok(c_name) = cstring(name, "private staging entry") else {
                continue;
            };
            let is_directory =
                match stat_descriptor(directory.as_raw_fd(), &c_name, "private staging entry") {
                    Ok(Some(stat)) => stat_is_directory(&stat),
                    _ => false,
                };
            names.push((c_name, is_directory));
        }
        // SAFETY: `stream` is a valid directory stream and is not used after.
        unsafe {
            libc::closedir(stream);
        }
        names
    }

    fn remove_tree_contents(directory: &File) {
        for (name, is_directory) in read_directory_names(directory) {
            if is_directory {
                // SAFETY: `directory` is an open descriptor and `name` is a
                // valid NUL-terminated entry name; the child descriptor is
                // immediately wrapped in an owning `File`.
                let descriptor = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if descriptor >= 0 {
                    // SAFETY: `descriptor` is a fresh, owned descriptor.
                    let child = unsafe { File::from_raw_fd(descriptor) };
                    remove_tree_contents(&child);
                }
                // SAFETY: `directory` is open and `name` is a valid entry
                // name; a failure is ignored because cleanup is best effort.
                unsafe {
                    libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
                }
            } else {
                // SAFETY: see above; a non-directory entry, including a
                // symbolic link, is unlinked without being followed.
                unsafe {
                    libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0);
                }
            }
        }
    }

    /// Removes the private staging tree and the staging directory itself.
    ///
    /// Contents are removed through the held descriptor without following
    /// symlinks. The directory is only unlinked when its pathname still
    /// refers to the held directory, so a replaced path is left untouched.
    pub(super) fn remove_private_staging_directory(path: &Path, directory: &File) -> bool {
        remove_tree_contents(directory);
        let Ok(Some(stat)) = stat_path_no_follow(path, "private Git staging directory") else {
            return false;
        };
        let Ok((device, inode)) = file_identity(directory) else {
            return false;
        };
        if !stat_is_directory(&stat) || stat.st_dev != device || stat.st_ino != inode {
            return false;
        }
        let Ok(path) = path_cstring(path, "private Git staging directory") else {
            return false;
        };
        // SAFETY: the descriptor-relative identity was just verified; the
        // path is valid and NUL-terminated.
        unsafe { libc::unlinkat(libc::AT_FDCWD, path.as_ptr(), libc::AT_REMOVEDIR) == 0 }
    }
}

/// Host-backed staging state for a primary checkout's per-worktree files.
///
/// The sandbox exposes the repository metadata root read-only, so the primary
/// checkout's per-worktree state (`HEAD`, `index`, `COMMIT_EDITMSG`,
/// `ORIG_HEAD` and the HEAD reflog) cannot be written in place without also
/// exposing the protected entries to creation. Git is therefore run with
/// `GIT_DIR` pointing at this private staging directory and `GIT_COMMON_DIR`
/// pointing at the real read-only-protected metadata root: shared state
/// (refs, objects, logs, config) is written directly to the host, while the
/// per-worktree files are staged and atomically applied back after the command
/// finishes.
///
/// The repository's own locks are acquired before the host snapshot is read,
/// and every host-side read, write and lock removal is anchored to the
/// descriptor of the verified metadata directory that was opened during
/// preparation. Pathnames are only used to re-verify that the verified
/// directory is still in place; they are never re-resolved to obtain write or
/// lock-removal authority.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct GitWorktreeStaging {
    directory: PathBuf,
    directory_file: std::fs::File,
    common_dir: PathBuf,
    common_dir_file: std::fs::File,
    worktree_root: PathBuf,
    locks: Vec<OwnedGitLock>,
    baseline: Vec<Option<Vec<u8>>>,
    reflog_directory_present: bool,
    #[cfg(test)]
    fail_before_rename: Option<&'static str>,
}

/// One repository lock owned by a staging run.
///
/// The open descriptor pins the inode for the lifetime of the run, and
/// release only unlinks the pathname while it still refers to that exact
/// regular file. An existing lock of another worker, or a lock replaced after
/// acquisition, is therefore never removed.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct OwnedGitLock {
    name: &'static str,
    _file: std::fs::File,
    device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
impl OwnedGitLock {
    fn acquire(common_dir: &std::fs::File, common_path: &Path, name: &'static str) -> Result<Self> {
        let path = common_path.join(name);
        let file = match staging_fs::open_lock_file(common_dir, name) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                anyhow::bail!(
                    "another Git operation is in progress: {} exists",
                    path.display()
                );
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot lock Git worktree state {}", path.display()));
            }
        };
        let (device, inode) = staging_fs::file_identity(&file)
            .with_context(|| format!("cannot inspect Git lock {}", path.display()))?;
        Ok(Self {
            name,
            _file: file,
            device,
            inode,
        })
    }

    fn release(&self, common_dir: &std::fs::File) {
        staging_fs::unlink_entry_if_identity(
            common_dir,
            self.name,
            self.device,
            self.inode,
            "owned Git lock",
        );
    }
}

#[cfg(target_os = "linux")]
struct StagedGitEntry {
    /// Path relative to the metadata root, used in diagnostics.
    path: &'static str,
    /// Final component below its parent directory.
    name: &'static str,
    /// Whether the entry lives under the common `logs` directory.
    in_logs: bool,
}

#[cfg(target_os = "linux")]
const STAGED_GIT_ENTRIES: &[StagedGitEntry] = &[
    StagedGitEntry {
        path: "HEAD",
        name: "HEAD",
        in_logs: false,
    },
    StagedGitEntry {
        path: "index",
        name: "index",
        in_logs: false,
    },
    StagedGitEntry {
        path: "ORIG_HEAD",
        name: "ORIG_HEAD",
        in_logs: false,
    },
    StagedGitEntry {
        path: "COMMIT_EDITMSG",
        name: "COMMIT_EDITMSG",
        in_logs: false,
    },
    StagedGitEntry {
        path: "logs/HEAD",
        name: "HEAD",
        in_logs: true,
    },
];

#[cfg(target_os = "linux")]
impl GitWorktreeStaging {
    /// Prepares staging only for a validated primary checkout. Linked
    /// worktrees keep their private metadata directory writable and are never
    /// staged.
    ///
    /// The verified common directory is opened first and every later step is
    /// bound to that descriptor. Repository locks are acquired before the
    /// host snapshot is read, so the baseline cannot be stale with respect to
    /// the lock-protected metadata. Any failure drops the partially-built
    /// value: only locks this run actually acquired are released and only the
    /// private staging directory is removed.
    fn prepare(cwd: &Path, metadata_roots: &[PathBuf]) -> Result<Option<Self>> {
        if metadata_roots.len() != 1 {
            return Ok(None);
        }
        let metadata_root = &metadata_roots[0];
        let metadata = std::fs::symlink_metadata(metadata_root).with_context(|| {
            format!(
                "cannot inspect Git metadata root {}",
                metadata_root.display()
            )
        })?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "symbolic-link Git metadata roots cannot be staged: {}",
            metadata_root.display()
        );
        anyhow::ensure!(
            metadata.is_dir(),
            "Git metadata root is not a directory: {}",
            metadata_root.display()
        );
        let common_dir = std::fs::canonicalize(metadata_root).with_context(|| {
            format!(
                "cannot resolve Git metadata root {}",
                metadata_root.display()
            )
        })?;
        let worktree_root = git_worktree_root(cwd)?;
        anyhow::ensure!(
            git_primary_checkout(cwd)? == worktree_root,
            "staged Git state requires a primary checkout"
        );
        let common_dir_file =
            staging_fs::open_directory_no_follow(&common_dir, "Git metadata directory")?;
        let base = std::fs::canonicalize(std::env::temp_dir())
            .context("cannot resolve the system temporary directory for Git staging")?;
        let directory = base.join(format!("temote-git-stage-{}", uuid::Uuid::new_v4()));
        staging_fs::create_private_directory(&directory)?;
        let directory_file =
            match staging_fs::open_directory_no_follow(&directory, "private Git staging directory")
            {
                Ok(file) => file,
                Err(error) => {
                    let _ = std::fs::remove_dir(&directory);
                    return Err(error);
                }
            };
        let mut staging = Self {
            directory,
            directory_file,
            common_dir,
            common_dir_file,
            worktree_root,
            locks: Vec::new(),
            baseline: Vec::new(),
            reflog_directory_present: false,
            #[cfg(test)]
            fail_before_rename: None,
        };
        staging.verify_metadata_authority()?;
        staging.acquire_locks()?;
        staging.capture_and_seed()?;
        Ok(Some(staging))
    }

    /// Re-verifies that the stored pathname still refers to the exact
    /// directory entity that was opened during preparation. A replaced,
    /// symlinked or removed metadata directory fails closed.
    fn verify_metadata_authority(&self) -> Result<()> {
        let (device, inode) = staging_fs::file_identity(&self.common_dir_file)?;
        match staging_fs::stat_path_no_follow(&self.common_dir, "Git metadata directory")? {
            Some(stat)
                if staging_fs::stat_is_directory(&stat)
                    && stat.st_dev == device
                    && stat.st_ino == inode =>
            {
                Ok(())
            }
            _ => anyhow::bail!(
                "Git metadata directory was replaced after staging was prepared: {}",
                self.common_dir.display()
            ),
        }
    }

    fn acquire_locks(&mut self) -> Result<()> {
        for name in ["index.lock", "HEAD.lock"] {
            let lock = OwnedGitLock::acquire(&self.common_dir_file, &self.common_dir, name)?;
            self.locks.push(lock);
        }
        Ok(())
    }

    /// Reads the lock-time host snapshot and seeds the private staging
    /// directory from it. The common `logs` directory is validated with a
    /// no-follow open: a symlinked or special entry fails closed instead of
    /// directing the later apply outside the metadata root.
    fn capture_and_seed(&mut self) -> Result<()> {
        let common_logs =
            staging_fs::open_directory_at(&self.common_dir_file, "logs", "Git reflog directory")?;
        self.reflog_directory_present = common_logs.is_some();
        staging_fs::create_directory_at(
            &self.directory_file,
            "logs",
            0o700,
            "private staging reflog directory",
        )?;
        let staging_logs = staging_fs::open_directory_at(
            &self.directory_file,
            "logs",
            "private staging reflog directory",
        )?
        .context("private staging reflog directory disappeared after creation")?;
        staging_fs::write_new_regular_entry_at(
            &self.directory_file,
            "commondir",
            format!("{}\n", self.common_dir.display()).as_bytes(),
            0o600,
            "staged Git common directory pointer",
        )?;
        for entry in STAGED_GIT_ENTRIES {
            let contents = if entry.in_logs {
                match &common_logs {
                    Some(common_logs) => staging_fs::read_regular_entry_at(
                        common_logs,
                        entry.name,
                        &format!("Git worktree state {}", entry.path),
                    )?,
                    None => None,
                }
            } else {
                staging_fs::read_regular_entry_at(
                    &self.common_dir_file,
                    entry.name,
                    &format!("Git worktree state {}", entry.path),
                )?
            };
            self.baseline
                .push(contents.as_ref().map(|entry| entry.bytes.clone()));
            if let Some(contents) = contents {
                let target = if entry.in_logs {
                    &staging_logs
                } else {
                    &self.directory_file
                };
                staging_fs::write_new_regular_entry_at(
                    target,
                    entry.name,
                    &contents.bytes,
                    contents.mode,
                    &format!("staged Git worktree state {}", entry.path),
                )?;
            }
        }
        Ok(())
    }

    fn directory_path(&self) -> &Path {
        &self.directory
    }

    fn extra_environment(&self) -> Vec<(String, String)> {
        let mut environment = vec![
            ("GIT_DIR".to_owned(), self.directory.display().to_string()),
            (
                "GIT_COMMON_DIR".to_owned(),
                self.common_dir.display().to_string(),
            ),
            (
                "GIT_WORK_TREE".to_owned(),
                self.worktree_root.display().to_string(),
            ),
        ];
        if !self.reflog_directory_present {
            // The common reflog directory did not exist when the locks were
            // acquired and cannot be created inside the read-only metadata
            // root; keep reflogs disabled exactly like a repository that has
            // no reflog directory yet.
            environment.extend([
                ("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()),
                (
                    "GIT_CONFIG_KEY_0".to_owned(),
                    "core.logAllRefUpdates".to_owned(),
                ),
                ("GIT_CONFIG_VALUE_0".to_owned(), "false".to_owned()),
            ]);
        }
        environment
    }

    fn open_staging_parent(&self, in_logs: bool) -> Result<std::fs::File> {
        if in_logs {
            staging_fs::open_directory_at(
                &self.directory_file,
                "logs",
                "private staging reflog directory",
            )?
            .context("private staging reflog directory is missing")
        } else {
            self.directory_file
                .try_clone()
                .context("cannot duplicate the private Git staging directory handle")
        }
    }

    /// Applies exactly the staged entries the command changed relative to the
    /// lock-time snapshot.
    ///
    /// The metadata authority, target parents, target entries and staged
    /// files are all validated before the first mutation. Each mutation is an
    /// exclusive temporary file renamed descriptor-relative over the target,
    /// preserving the staged file permissions. An entry the command did not
    /// change is never restored to the old snapshot, and a command that
    /// removed staged state fails closed instead of deleting host state.
    ///
    /// This is not a transaction: entries are atomic individually, and shared
    /// refs or objects the command already wrote to the common directory are
    /// not rolled back.
    fn apply(&self) -> Result<()> {
        anyhow::ensure!(
            self.baseline.len() == STAGED_GIT_ENTRIES.len(),
            "staged Git baseline is incomplete"
        );
        self.verify_metadata_authority()?;
        let mut planned = Vec::new();
        for (index, entry) in STAGED_GIT_ENTRIES.iter().enumerate() {
            let staged_parent = self.open_staging_parent(entry.in_logs)?;
            let staged = staging_fs::read_regular_entry_at(
                &staged_parent,
                entry.name,
                &format!("staged Git state {}", entry.path),
            )?;
            let Some(staged) = staged else {
                if self.baseline[index].is_some() {
                    anyhow::bail!(
                        "the command removed staged Git state {}; refusing to delete metadata state",
                        entry.path
                    );
                }
                continue;
            };
            if self.baseline[index].as_deref() == Some(staged.bytes.as_slice()) {
                continue;
            }
            let mut create_reflog_directory = false;
            let target_parent = if entry.in_logs {
                match staging_fs::open_directory_at(
                    &self.common_dir_file,
                    "logs",
                    "Git reflog directory",
                )? {
                    Some(directory) => directory,
                    None => {
                        create_reflog_directory = true;
                        self.common_dir_file
                            .try_clone()
                            .context("cannot duplicate the Git metadata directory handle")?
                    }
                }
            } else {
                self.common_dir_file
                    .try_clone()
                    .context("cannot duplicate the Git metadata directory handle")?
            };
            if !create_reflog_directory {
                let target_label = format!("Git state {}", entry.path);
                match staging_fs::stat_entry_at(&target_parent, entry.name, &target_label)? {
                    Some(stat) if staging_fs::stat_is_regular_file(&stat) => {}
                    Some(_) => anyhow::bail!("refusing to replace non-regular {target_label}"),
                    None => {}
                }
            }
            planned.push(StagedApplyEntry {
                index,
                bytes: staged.bytes,
                mode: staged.mode,
                create_reflog_directory,
            });
        }
        if planned.is_empty() {
            return Ok(());
        }
        if planned.iter().any(|entry| entry.create_reflog_directory) {
            staging_fs::create_directory_at(
                &self.common_dir_file,
                "logs",
                0o777,
                "Git reflog directory",
            )?;
        }
        for (position, entry) in planned.iter().enumerate() {
            let definition = &STAGED_GIT_ENTRIES[entry.index];
            let target_parent = if definition.in_logs {
                staging_fs::open_directory_at(
                    &self.common_dir_file,
                    "logs",
                    "Git reflog directory",
                )?
                .context("Git reflog directory is missing")?
            } else {
                self.common_dir_file
                    .try_clone()
                    .context("cannot duplicate the Git metadata directory handle")?
            };
            #[cfg(test)]
            let inject_failure = self.fail_before_rename == Some(definition.path);
            #[cfg(not(test))]
            let inject_failure = false;
            staging_fs::replace_regular_entry_atomic_at(
                &target_parent,
                definition.name,
                &entry.bytes,
                entry.mode,
                &format!("Git state {}", definition.path),
                inject_failure,
            )
            .map_err(|error| {
                let applied = planned[..position]
                    .iter()
                    .map(|entry| STAGED_GIT_ENTRIES[entry.index].path)
                    .collect::<Vec<_>>();
                let applied = if applied.is_empty() {
                    "none".to_owned()
                } else {
                    applied.join(", ")
                };
                error.context(format!(
                    "failed to apply staged Git state to {} (already applied: {applied})",
                    definition.path
                ))
            })?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
struct StagedApplyEntry {
    index: usize,
    bytes: Vec<u8>,
    mode: u32,
    create_reflog_directory: bool,
}

#[cfg(target_os = "linux")]
impl Drop for GitWorktreeStaging {
    fn drop(&mut self) {
        for lock in &self.locks {
            lock.release(&self.common_dir_file);
        }
        staging_fs::remove_private_staging_directory(&self.directory, &self.directory_file);
    }
}

/// Metadata authorization for one validated Git command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitMetadataScope {
    /// Every metadata root validated for the command `cwd` must be contained by
    /// the command `cwd` or one of the writable roots.
    Contained,
    /// The caller pinned and re-validated this exact repository identity before
    /// the request, so the metadata roots validated for the command `cwd` may
    /// lie below the primary checkout of a linked worktree.
    PinnedWorktree,
}

/// Runs a narrowly validated Git operation with write access to the repository
/// metadata needed by `git add` and `git commit`. Ordinary sandboxed commands
/// continue to keep `.git` read-only.
///
/// Every metadata root validated for `cwd` must be contained by the command
/// `cwd` or one of `writable_roots`. A linked worktree, whose metadata lives
/// below the primary checkout, must use
/// [`run_git_with_pinned_worktree_metadata`] instead.
pub async fn run_git(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    provided_git_metadata_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_git_with_metadata_scope(
        command,
        cwd,
        writable_roots,
        provided_git_metadata_roots,
        GitMetadataScope::Contained,
        stdin,
    )
    .await
}

/// Runs one narrowly validated Git mutation for a linked worktree whose
/// repository identity the caller pinned before the request.
///
/// The local agent Git broker captures the canonical selected workspace and its
/// validated `git_worktree_root` / `git_metadata_roots` identity at broker
/// start and re-validates that identity for every request, so the private
/// worktree metadata and the common repository directory below the primary
/// checkout are authorized here without widening any writable workspace scope.
/// The sandbox still derives the metadata roots from `cwd` itself and rejects
/// any mismatch, so no caller can authorize an arbitrary path, and the
/// repository metadata policy (`config`/`hooks`/`refs` protection) is
/// unchanged.
pub async fn run_git_with_pinned_worktree_metadata(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    provided_git_metadata_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_git_with_metadata_scope(
        command,
        cwd,
        writable_roots,
        provided_git_metadata_roots,
        GitMetadataScope::PinnedWorktree,
        stdin,
    )
    .await
}

async fn run_git_with_metadata_scope(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    provided_git_metadata_roots: &[PathBuf],
    scope: GitMetadataScope,
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
    verify_git_metadata_scope(scope, &cwd, writable_roots, &validated_roots)?;
    run_git_with_metadata_roots_platform(command, &cwd, writable_roots, &validated_roots, stdin)
        .await
}

/// Platform-specific Git command execution for one validated metadata scope.
///
/// A primary checkout keeps its per-worktree files in the metadata root that
/// the sandbox exposes read-only. On Linux those files are staged in a private
/// host-backed directory so Git's lock-and-rename protocol works, while the
/// protected (and missing) entries stay untouched.
#[cfg(target_os = "linux")]
async fn run_git_with_metadata_roots_platform(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    validated_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    let staging = GitWorktreeStaging::prepare(cwd, validated_roots)?;
    let mut staged_roots = writable_roots.to_vec();
    let mut environment = Vec::new();
    if let Some(staging) = &staging {
        staged_roots.push(staging.directory_path().to_path_buf());
        environment = staging.extra_environment();
    }
    let output = run_with_metadata_roots(
        command,
        cwd,
        &staged_roots,
        validated_roots,
        None,
        CommandNetworkPolicy::Restricted,
        stdin,
        &environment,
    )
    .await;
    finalize_staged_git_state(staging.as_ref(), &output)?;
    output
}

/// Persists the staged per-worktree state only when the command produced a
/// result.
///
/// A failed sandbox setup or spawn means no child ran, so the lock-time
/// snapshot must not be applied and the original error is propagated
/// unchanged. A completed command applies exactly the entries the child
/// changed, including on a non-zero exit status: Git can make legitimate
/// partial updates before reporting failure. Shared state the child already
/// wrote directly to the common directory is not rolled back; this is not a
/// transaction.
#[cfg(target_os = "linux")]
fn finalize_staged_git_state(
    staging: Option<&GitWorktreeStaging>,
    result: &Result<Output>,
) -> Result<()> {
    if result.is_err() {
        return Ok(());
    }
    if let Some(staging) = staging {
        staging
            .apply()
            .context("failed to persist staged Git worktree state")?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn run_git_with_metadata_roots_platform(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    validated_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_with_metadata_roots(
        command,
        cwd,
        writable_roots,
        validated_roots,
        None,
        CommandNetworkPolicy::Restricted,
        stdin,
        &[],
    )
    .await
}

/// Applies the metadata containment rule for one validated command scope.
///
/// `validated_roots` always comes from [`git_metadata_roots`] for the canonical
/// command `cwd`, so the pinned scope can only ever authorize that exact
/// identity: the caller cannot inject a root, and the repository metadata
/// policy still protects `config`, `hooks` and remote/tag refs.
fn verify_git_metadata_scope(
    scope: GitMetadataScope,
    cwd: &Path,
    writable_roots: &[PathBuf],
    validated_roots: &[PathBuf],
) -> Result<()> {
    if scope == GitMetadataScope::PinnedWorktree {
        return Ok(());
    }
    let mut permitted_roots = vec![cwd.to_path_buf()];
    permitted_roots.extend(
        writable_roots
            .iter()
            .map(|path| {
                std::fs::canonicalize(path)
                    .with_context(|| format!("cannot resolve writable root {}", path.display()))
            })
            .collect::<Result<Vec<_>>>()?,
    );
    for git_root in validated_roots {
        anyhow::ensure!(
            permitted_roots
                .iter()
                .any(|permitted| git_root.starts_with(permitted)),
            "Git metadata root is outside the permitted session roots: {}",
            git_root.display()
        );
    }
    Ok(())
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
    // The common metadata root is read-only in the sandbox and only existing
    // directories can be re-exposed as writable. Creating the `worktrees`
    // namespace is the command's own first-use effect, so prepare the empty
    // directory host-side and keep the command itself sandboxed.
    if let Some(common) = validated_roots.first() {
        let worktrees = common.join("worktrees");
        if !worktrees.exists() {
            std::fs::create_dir(&worktrees).with_context(|| {
                format!(
                    "cannot prepare the Git worktrees namespace {}",
                    worktrees.display()
                )
            })?;
        }
    }
    let mut environment = Vec::new();
    if let Some(common) = validated_roots.first()
        && !common.join("logs").is_dir()
    {
        // A missing common reflog directory cannot be created inside the
        // read-only metadata root; keep reflogs disabled for this command.
        environment.extend([
            ("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()),
            (
                "GIT_CONFIG_KEY_0".to_owned(),
                "core.logAllRefUpdates".to_owned(),
            ),
            ("GIT_CONFIG_VALUE_0".to_owned(), "false".to_owned()),
        ]);
    }
    run_with_metadata_roots(
        command,
        &cwd,
        writable_roots,
        &validated_roots,
        Some(&protected_worktree_roots),
        CommandNetworkPolicy::Restricted,
        stdin,
        &environment,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_with_metadata_roots(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    git_metadata_roots: &[PathBuf],
    protected_worktree_roots: Option<&[PathBuf]>,
    network: CommandNetworkPolicy,
    stdin: Option<&[u8]>,
    extra_environment: &[(String, String)],
) -> Result<Output> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    validate_writable_scope(&cwd, writable_roots)?;
    #[cfg(target_os = "macos")]
    let spec = if git_metadata_roots.is_empty() {
        policy::SandboxSpec::command(&cwd, writable_roots, network.development_enabled())?
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
        linux::command(command, &cwd, writable_roots, git_metadata_roots, network)?
    };

    #[cfg(target_os = "macos")]
    let mut process = macos::command(&spec, command)?;

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let mut process =
        { anyhow::bail!("sandboxed execution is currently implemented for Linux and macOS only") };

    let command_cache = CommandCacheDir::create()?;
    let mut environment = safe_environment(command_cache.path())?;
    environment.extend(extra_environment.iter().cloned());

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

struct GitMetadataPaths {
    worktree_root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    dot_git_is_directory: bool,
}

/// Resolves the worktree's private Git directory and its common repository
/// directory. The latter is needed for linked worktrees, whose `.git` file
/// points below the common repository metadata directory.
pub fn git_metadata_roots(cwd: &Path) -> Result<Vec<PathBuf>> {
    let metadata = resolve_git_metadata_paths(cwd)?;
    let mut roots = vec![metadata.git_dir, metadata.common_dir];
    roots.sort();
    roots.dedup();
    Ok(roots)
}

/// Canonical common Git directory for the repository that contains `cwd`.
///
/// This is the repository identity anchor: two checkouts are the same
/// repository only when they resolve to the same canonical common directory.
pub fn git_common_dir(cwd: &Path) -> Result<PathBuf> {
    Ok(resolve_git_metadata_paths(cwd)?.common_dir)
}

/// Canonical primary checkout (main worktree) for the repository that contains
/// `cwd`.
///
/// Only Git's standard layouts are supported: either `cwd` itself is inside the
/// primary checkout, or a linked worktree's common directory is the primary
/// checkout's `.git` directory. Anything else fails closed.
pub fn git_primary_checkout(cwd: &Path) -> Result<PathBuf> {
    let metadata = resolve_git_metadata_paths(cwd)?;
    if metadata.dot_git_is_directory {
        return Ok(metadata.worktree_root);
    }
    anyhow::ensure!(
        metadata.common_dir.file_name() == Some(std::ffi::OsStr::new(".git")),
        "unsupported Git common directory layout: {}",
        metadata.common_dir.display()
    );
    let primary = metadata
        .common_dir
        .parent()
        .context("Git common directory has no parent")?;
    anyhow::ensure!(
        primary.is_dir(),
        "Git primary checkout is not a directory: {}",
        primary.display()
    );
    Ok(primary.to_path_buf())
}

/// Current branch of the worktree that contains `cwd`.
///
/// Reads the worktree's own bounded `HEAD` control file, so a linked worktree
/// reports its own branch, never the primary checkout's. A detached HEAD or any
/// unsupported ref shape yields `None`.
pub fn git_current_branch(cwd: &Path) -> Result<Option<String>> {
    const MAX_BRANCH_BYTES: usize = 255;
    let metadata = resolve_git_metadata_paths(cwd)?;
    let Some(head) = read_git_control_file(&metadata.git_dir.join("HEAD"), "Git HEAD")? else {
        return Ok(None);
    };
    let Some(branch) = head.trim().strip_prefix("ref: refs/heads/") else {
        return Ok(None);
    };
    anyhow::ensure!(
        !branch.is_empty()
            && branch.len() <= MAX_BRANCH_BYTES
            && !branch.chars().any(char::is_control),
        "Git HEAD contains an unsupported branch name"
    );
    Ok(Some(branch.to_owned()))
}

/// Canonical repository identity of one Git worktree root.
///
/// The managed-worktree authority resolution captures this value after it has
/// validated a workspace and carries it to the Git broker and the local-agent
/// sandbox launch, so those later stages compare against the exact identity
/// that was validated instead of adopting a freshly observed one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRepositoryIdentity {
    pub worktree_root: PathBuf,
    pub metadata_roots: Vec<PathBuf>,
    pub common_dir: PathBuf,
    pub primary_checkout: PathBuf,
}

impl WorkspaceRepositoryIdentity {
    /// Resolves the repository identity only when `workspace` is itself a
    /// canonical Git worktree root.
    ///
    /// A subdirectory, a symlinked or swapped path, a nested repository and
    /// every unsupported Git layout fail closed instead of yielding a partial
    /// identity.
    pub fn for_workspace(workspace: &Path) -> Result<Self> {
        let canonical = std::fs::canonicalize(workspace).with_context(|| {
            format!(
                "cannot resolve Git worktree workspace {}",
                workspace.display()
            )
        })?;
        anyhow::ensure!(
            canonical.is_dir(),
            "Git worktree workspace is not a directory: {}",
            canonical.display()
        );
        let worktree_root = git_worktree_root(&canonical)?;
        anyhow::ensure!(
            worktree_root == canonical,
            "Git workspace is not a worktree root: {}",
            canonical.display()
        );
        Ok(Self {
            worktree_root,
            metadata_roots: git_metadata_roots(&canonical)?,
            common_dir: git_common_dir(&canonical)?,
            primary_checkout: git_primary_checkout(&canonical)?,
        })
    }

    /// `true` for a linked worktree whose private metadata and common
    /// repository directory live below the primary checkout.
    pub fn linked_worktree(&self) -> bool {
        self.primary_checkout != self.worktree_root
    }
}

fn resolve_git_metadata_paths(cwd: &Path) -> Result<GitMetadataPaths> {
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

    Ok(GitMetadataPaths {
        worktree_root,
        git_dir,
        common_dir,
        dot_git_is_directory,
    })
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

/// A Git worktree and its metadata directories pinned before a structured
/// mutation is approved.
///
/// This is intentionally narrower than [`run_unrestricted_with_env`].  It is
/// only used by exact structured branch-delete operations and their bounded
/// inspections.  On Unix, the child receives descriptor-backed `cwd`,
/// `GIT_DIR`, `GIT_COMMON_DIR`, and `GIT_WORK_TREE` paths, so replacing the
/// validated pathname after approval cannot redirect the Git process to
/// another repository.
pub struct PinnedGitRepository {
    #[cfg(not(unix))]
    identity: WorkspaceRepositoryIdentity,
    #[cfg(unix)]
    worktree: std::fs::File,
    #[cfg(unix)]
    git_dir: std::fs::File,
    #[cfg(unix)]
    common_dir: std::fs::File,
    #[cfg(not(unix))]
    root: PathBuf,
}

/// Pins one canonical Git worktree root and its private/common metadata.
///
/// The descriptor is opened before approval and remains owned by the caller
/// until the mutation finishes.  `WorkspaceRepositoryIdentity` is resolved
/// through the opened worktree descriptor on Linux/macOS rather than adopting
/// a later pathname resolution.
pub fn pin_git_repository(root: &Path) -> Result<PinnedGitRepository> {
    #[cfg(unix)]
    {
        let worktree = open_pinned_directory(root, "Git worktree")?;
        let identity = WorkspaceRepositoryIdentity::for_workspace(&fd_path(worktree.as_raw_fd())?)?;
        anyhow::ensure!(
            identity.worktree_root == std::fs::canonicalize(root)?,
            "Git worktree path changed while it was being pinned"
        );

        let current = std::fs::metadata(root)
            .with_context(|| format!("cannot inspect Git worktree {}", root.display()))?;
        anyhow::ensure!(
            same_file_identity(&worktree.metadata()?, &current),
            "Git worktree path changed while it was being pinned"
        );

        let git_dir_path = identity
            .metadata_roots
            .iter()
            .find(|path| *path != &identity.common_dir)
            .cloned()
            .unwrap_or_else(|| identity.common_dir.clone());
        let git_dir = open_pinned_directory(&git_dir_path, "Git private metadata")?;
        let common_dir = if git_dir_path == identity.common_dir {
            let common_dir = git_dir
                .try_clone()
                .context("cannot duplicate pinned Git common metadata descriptor")?;
            make_fd_inheritable(&common_dir)?;
            common_dir
        } else {
            open_pinned_directory(&identity.common_dir, "Git common metadata")?
        };

        // Re-check the identity after every metadata descriptor is open.  The
        // child will use these descriptors, not the paths, after this point.
        let observed = WorkspaceRepositoryIdentity::for_workspace(&fd_path(worktree.as_raw_fd())?)?;
        anyhow::ensure!(
            observed == identity,
            "Git repository identity changed while metadata was being pinned"
        );

        Ok(PinnedGitRepository {
            worktree,
            git_dir,
            common_dir,
        })
    }

    #[cfg(not(unix))]
    {
        let root = std::fs::canonicalize(root)
            .with_context(|| format!("cannot resolve Git worktree {}", root.display()))?;
        let identity = WorkspaceRepositoryIdentity::for_workspace(&root)?;
        Ok(PinnedGitRepository { identity, root })
    }
}

/// Executes one already-classified structured Git command through the pinned
/// repository.  The pinned context is borrowed so one structured operation can
/// inspect and mutate the same descriptor-backed repository across approval.
/// The function is crate-private and does not provide a public arbitrary
/// unsandboxed command surface.
pub async fn run_pinned_git_command(
    pinned: &PinnedGitRepository,
    command: &[String],
    stdin: Option<&[u8]>,
    environment: &HashMap<String, String>,
    remove_environment: &[&str],
) -> Result<Output> {
    run_pinned_git_command_inner(
        pinned,
        command,
        stdin,
        environment,
        remove_environment,
        |_| Ok(()),
    )
    .await
}

async fn run_pinned_git_command_inner<F>(
    pinned: &PinnedGitRepository,
    command: &[String],
    stdin: Option<&[u8]>,
    environment: &HashMap<String, String>,
    remove_environment: &[&str],
    on_spawn: F,
) -> Result<Output>
where
    F: FnOnce(u32) -> Result<()>,
{
    anyhow::ensure!(!command.is_empty(), "command must not be empty");

    #[cfg(unix)]
    let (worktree_fd, git_dir_fd, common_dir_fd) = (
        pinned.worktree.as_raw_fd(),
        pinned.git_dir.as_raw_fd(),
        pinned.common_dir.as_raw_fd(),
    );

    #[cfg(unix)]
    let mut process = {
        let mut process = tokio::process::Command::new(&command[0]);
        process.args(&command[1..]);
        // The descriptor-backed fchdir below is the authority.  Do not set a
        // pathname cwd that could be swapped between command construction and
        // the child pre-exec hook.
        unsafe {
            process.pre_exec(move || {
                // SAFETY: the descriptor is held by `pinned` until the child
                // has completed, and fchdir does not allocate or touch Rust
                // synchronization primitives in the forked child.
                if libc::fchdir(worktree_fd) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        process
    };

    #[cfg(not(unix))]
    let mut process = {
        anyhow::ensure!(
            WorkspaceRepositoryIdentity::for_workspace(&pinned.root)? == pinned.identity,
            "Git repository identity changed before structured mutation"
        );
        tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .current_dir(&pinned.root)
    };

    // Do not let inherited GIT_* variables redirect this exact operation to a
    // different index, object database, config, or repository.  The caller's
    // environment is applied only after the inherited variables are removed;
    // the pinned values are written last and cannot be overridden by it.
    for (name, _) in
        std::env::vars_os().filter(|(name, _)| name.to_string_lossy().starts_with("GIT_"))
    {
        process.env_remove(name);
    }
    for name in remove_environment {
        process.env_remove(name);
    }
    process.envs(environment);

    #[cfg(unix)]
    {
        process.env("GIT_DIR", fd_path(git_dir_fd)?);
        process.env("GIT_COMMON_DIR", fd_path(common_dir_fd)?);
        process.env("GIT_WORK_TREE", fd_path(worktree_fd)?);
        process.env("GIT_CONFIG_NOSYSTEM", "1");
        process.env("GIT_TERMINAL_PROMPT", "0");
    }

    process
        .kill_on_drop(true)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = process
        .spawn()
        .context("failed to start pinned structured Git command")?;
    let pid = child
        .id()
        .context("pinned structured Git child PID is unavailable after spawn")?;
    if let Err(error) = on_spawn(pid) {
        let mut child = child;
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(error);
    }
    wait_with_limited_output(child, stdin).await
}

#[cfg(unix)]
fn open_pinned_directory(path: &Path, label: &str) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .with_context(|| format!("cannot open pinned {label} {}", path.display()))?;
    make_fd_inheritable(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn make_fd_inheritable(file: &std::fs::File) -> Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: fd belongs to the live file descriptor held by `file`.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    anyhow::ensure!(flags >= 0, "cannot inspect pinned Git descriptor flags");
    // SAFETY: fd belongs to the live file descriptor held by `file`; clearing
    // close-on-exec is required for the child to resolve /proc/self/fd paths.
    anyhow::ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == 0,
        "cannot make pinned Git descriptor inheritable"
    );
    Ok(())
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(unix)]
fn fd_path(fd: RawFd) -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    let path = PathBuf::from(format!("/proc/self/fd/{fd}"));
    #[cfg(target_os = "macos")]
    let path = PathBuf::from(format!("/dev/fd/{fd}"));
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let path = {
        let _ = fd;
        anyhow::bail!("descriptor-backed Git execution is unsupported on this Unix host")
    };
    Ok(path)
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
    apply_codex_private_state_environment(&mut environment, cache_root)?;
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

/// Direct Codex CLI probes create PATH-alias startup state under
/// `$CODEX_HOME/tmp` before doing any real work. Ordinary sandboxed commands
/// see the host `$HOME/.codex` read-only, so that write fails as
/// `Read-only file system` and Codex prints a warning on every successful run.
///
/// Redirect only the mutable startup state into this command's private cache
/// and expose the existing credential/config files as read-only symlinks. The
/// host Codex home is never made writable, credential bytes are never copied,
/// and the launcher shape (Vite+ managed or standalone) is irrelevant because
/// Codex resolves the same `CODEX_HOME` contract.
fn apply_codex_private_state_environment(
    environment: &mut HashMap<String, String>,
    cache_root: &Path,
) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (environment, cache_root);
        Ok(())
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let Some(home) = environment.get("HOME").map(PathBuf::from) else {
            return Ok(());
        };
        if !home.is_absolute() {
            return Ok(());
        }
        let source = home.join(".codex");
        let metadata = match std::fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect host Codex home {}", source.display())
                });
            }
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Ok(());
        }

        let state = cache_root.join("codex-home");
        std::fs::create_dir_all(state.join("tmp"))
            .with_context(|| format!("failed to create private Codex state {}", state.display()))?;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(state.join("tmp"), std::fs::Permissions::from_mode(0o700))?;
        for name in ["auth.json", "config.toml"] {
            let target = source.join(name);
            let Ok(metadata) = std::fs::symlink_metadata(&target) else {
                continue;
            };
            if !metadata.file_type().is_file() {
                continue;
            }
            let link = state.join(name);
            if std::fs::symlink_metadata(&link).is_ok() {
                continue;
            }
            std::os::unix::fs::symlink(&target, &link).with_context(|| {
                format!(
                    "failed to link read-only Codex file {} into {}",
                    target.display(),
                    link.display()
                )
            })?;
        }
        environment.insert(
            "CODEX_HOME".to_owned(),
            state.to_string_lossy().into_owned(),
        );
        Ok(())
    }
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
    #[cfg(unix)]
    fn command_environment_redirects_codex_startup_state_for_both_launcher_shapes() {
        for vite_plus_managed in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let home = fixture.path().join("home with space");
            let host_codex = home.join(".codex");
            std::fs::create_dir_all(host_codex.join("tmp")).unwrap();
            std::fs::write(host_codex.join("auth.json"), b"fixture-auth").unwrap();
            std::fs::write(host_codex.join("config.toml"), b"fixture-config").unwrap();
            if vite_plus_managed {
                let launcher = home.join(".vite-plus/bin/codex");
                std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
                std::fs::write(&launcher, b"#!/bin/sh\nexec vp codex \"$@\"\n").unwrap();
            }
            let cache = fixture.path().join("command-cache");
            let mut environment =
                HashMap::from([("HOME".to_owned(), home.to_string_lossy().into_owned())]);

            apply_codex_private_state_environment(&mut environment, &cache).unwrap();

            let state = environment
                .get("CODEX_HOME")
                .map(PathBuf::from)
                .expect("Codex startup state must be redirected");
            assert!(state.starts_with(&cache));
            assert!(!state.starts_with(&home));

            // The attempted mutable startup path is `$CODEX_HOME/tmp/arg0` for
            // both launcher shapes; only the private state accepts that write.
            let alias = state.join("tmp/arg0/codex");
            std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
            std::fs::write(&alias, b"alias").unwrap();
            assert_eq!(std::fs::read(&alias).unwrap(), b"alias");
            assert!(!host_codex.join("tmp/arg0").exists());

            // Credential/config bytes stay in the read-only host home.
            assert_eq!(
                std::fs::read_link(state.join("auth.json")).unwrap(),
                host_codex.join("auth.json")
            );
            assert_eq!(
                std::fs::read(state.join("config.toml")).unwrap(),
                b"fixture-config"
            );
            let host_text = std::fs::read(host_codex.join("auth.json")).unwrap();
            assert_eq!(host_text, b"fixture-auth");

            assert!(
                !environment
                    .values()
                    .any(|value| value.contains(".vite-plus")),
                "Vite+ host state must not be exposed as writable Codex state"
            );
        }
    }

    #[test]
    fn command_environment_leaves_codex_home_untouched_without_host_state() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("empty-home");
        std::fs::create_dir(&home).unwrap();
        let cache = fixture.path().join("command-cache");
        let mut environment =
            HashMap::from([("HOME".to_owned(), home.to_string_lossy().into_owned())]);

        apply_codex_private_state_environment(&mut environment, &cache).unwrap();

        assert!(!environment.contains_key("CODEX_HOME"));
        assert!(!cache.exists());
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
    fn resolves_primary_checkout_and_common_dir_for_standard_layouts() {
        let repository = tempfile::tempdir().unwrap();
        std::fs::create_dir(repository.path().join(".git")).unwrap();
        let canonical_repository = std::fs::canonicalize(repository.path()).unwrap();
        assert_eq!(
            git_primary_checkout(&canonical_repository).unwrap(),
            canonical_repository
        );
        assert_eq!(
            git_common_dir(&canonical_repository).unwrap(),
            canonical_repository.join(".git")
        );

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

        assert_eq!(
            git_common_dir(&worktree).unwrap(),
            std::fs::canonicalize(&common).unwrap()
        );
        assert_eq!(
            git_primary_checkout(&worktree).unwrap(),
            std::fs::canonicalize(&repository).unwrap()
        );
    }

    #[test]
    fn current_branch_reads_the_worktree_own_head() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        let common = repository.join(".git");
        let private = common.join("worktrees").join("feature");
        let worktree = root.path().join("worktree");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(common.join("HEAD"), "ref: refs/heads/main\n").unwrap();
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

        std::fs::write(
            private.join("HEAD"),
            "ref: refs/heads/feature/nested-name\n",
        )
        .unwrap();
        assert_eq!(
            git_current_branch(&worktree).unwrap().as_deref(),
            Some("feature/nested-name")
        );
        assert_eq!(
            git_current_branch(&repository).unwrap().as_deref(),
            Some("main")
        );

        std::fs::write(private.join("HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(git_current_branch(&worktree).unwrap(), None);

        std::fs::write(private.join("HEAD"), "ref: refs/heads/bad\nname\n").unwrap();
        assert!(git_current_branch(&worktree).is_err());
    }

    #[test]
    fn primary_checkout_fails_closed_on_unsupported_common_layouts() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        let common = repository.join("metadata");
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

        assert_eq!(
            git_common_dir(&worktree).unwrap(),
            std::fs::canonicalize(&common).unwrap()
        );
        assert!(git_primary_checkout(&worktree).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn git_metadata_paths_reject_symlinked_dot_git() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, repository.join(".git")).unwrap();

        assert!(git_metadata_roots(&repository).is_err());
        assert!(git_common_dir(&repository).is_err());
        assert!(git_primary_checkout(&repository).is_err());
    }

    #[test]
    fn linked_worktree_metadata_requires_the_pinned_scope() {
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

        let canonical_worktree = std::fs::canonicalize(&worktree).unwrap();
        let validated = git_metadata_roots(&canonical_worktree).unwrap();
        let writable = [canonical_worktree.clone()];

        // The contained scope used by ordinary structured Git tools rejects a
        // linked worktree whose metadata is below the primary checkout.
        let error = verify_git_metadata_scope(
            GitMetadataScope::Contained,
            &canonical_worktree,
            &writable,
            &validated,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the permitted session roots"),
            "{error:#}"
        );

        // The pinned scope used by the broker authorizes exactly the validated
        // metadata roots; the async entry point still requires the caller's
        // roots to equal `git_metadata_roots(cwd)`, so no other path can be
        // authorized.
        verify_git_metadata_scope(
            GitMetadataScope::PinnedWorktree,
            &canonical_worktree,
            &writable,
            &validated,
        )
        .unwrap();

        // A primary checkout stays contained and keeps using the contained
        // scope.
        let canonical_repository = std::fs::canonicalize(&repository).unwrap();
        let primary_roots = git_metadata_roots(&canonical_repository).unwrap();
        verify_git_metadata_scope(
            GitMetadataScope::Contained,
            &canonical_repository,
            std::slice::from_ref(&canonical_repository),
            &primary_roots,
        )
        .unwrap();
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

    fn host_git_stdout(cwd: &Path, args: &[&str]) -> Result<String> {
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
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
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
        assert_eq!(host_git_stdout(&workspace, &["status", "--porcelain"])?, "");
        assert_eq!(
            host_git_stdout(&workspace, &["log", "-1", "--format=%s"])?,
            "linux sandbox acceptance"
        );

        // Staging applies the primary checkout's per-worktree state back to
        // the host: a branch switch must persist its HEAD and index.
        let switched = run_git(
            &command("/usr/bin/git", &["switch", "-c", "staged-branch"]),
            &workspace,
            std::slice::from_ref(&writable_root),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(switched.status, 0, "{}", switched.stderr);
        assert_eq!(
            host_git_stdout(&workspace, &["branch", "--show-current"])?,
            "staged-branch"
        );
        assert_eq!(host_git_stdout(&workspace, &["status", "--porcelain"])?, "");
        assert!(
            std::fs::read_to_string(workspace.join(".git").join("HEAD"))?
                .contains("refs/heads/staged-branch")
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

    #[derive(Debug)]
    enum HostEntry {
        Missing,
        RegularFile(Vec<u8>),
        Symlink,
        Directory,
        Other,
    }

    /// Inspects a host path without collapsing non-NotFound I/O errors into an
    /// empty value. Only an explicit NotFound becomes [`HostEntry::Missing`].
    fn host_entry(path: &Path) -> Result<HostEntry> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_file() {
                    Ok(HostEntry::RegularFile(std::fs::read(path)?))
                } else if file_type.is_symlink() {
                    Ok(HostEntry::Symlink)
                } else if file_type.is_dir() {
                    Ok(HostEntry::Directory)
                } else {
                    Ok(HostEntry::Other)
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HostEntry::Missing),
            Err(error) => {
                Err(error).with_context(|| format!("cannot inspect host path {}", path.display()))
            }
        }
    }

    fn init_metadata_repository(repository: &Path) -> Result<()> {
        std::fs::create_dir_all(repository)?;
        host_git(repository, &["init", "-q"])?;
        std::fs::write(repository.join("base.txt"), b"base\n")?;
        host_git(repository, &["add", "--", "base.txt"])?;
        host_git(
            repository,
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
        Ok(())
    }

    fn add_metadata_worktree(repository: &Path, worktree: &Path, branch: &str) -> Result<()> {
        host_git(
            repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                worktree
                    .to_str()
                    .context("worktree path is not valid UTF-8")?,
            ],
        )
    }

    #[tokio::test]
    async fn linux_missing_protected_metadata_stays_missing_in_a_primary_checkout() -> Result<()> {
        let root = test_root();
        let repository = root.path().join("repository");
        init_metadata_repository(&repository)?;
        let missing = repository.join(".git").join("shallow");
        let packed_refs = repository.join(".git").join("packed-refs");
        assert!(matches!(host_entry(&missing)?, HostEntry::Missing));
        assert!(matches!(host_entry(&packed_refs)?, HostEntry::Missing));
        assert_eq!(
            host_git_stdout(&repository, &["rev-parse", "--is-shallow-repository"])?,
            "false"
        );

        let git_roots = git_metadata_roots(&repository)?;
        let output = run_git(
            &command(
                "/usr/bin/sh",
                &[
                    "-c",
                    "if test -e .git/shallow; then echo shallow-exists; else echo shallow-absent; fi; \
                     if test -e .git/packed-refs; then echo packed-refs-exists; else echo packed-refs-absent; fi; \
                     printf 'shallow-before='; git rev-parse --is-shallow-repository; \
                     if printf corrupted > .git/shallow 2>/dev/null; then echo write-shallow-ok; \
                     else echo write-shallow-denied; fi; \
                     if printf corrupted > .git/packed-refs 2>/dev/null; then echo write-packed-refs-ok; \
                     else echo write-packed-refs-denied; fi; \
                     printf 'shallow-after='; git rev-parse --is-shallow-repository",
                ],
            ),
            &repository,
            std::slice::from_ref(&repository),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert_eq!(
            output.stdout,
            "shallow-absent\npacked-refs-absent\nshallow-before=false\n\
             write-shallow-denied\nwrite-packed-refs-denied\nshallow-after=false\n"
        );
        // The host path stayed exactly missing: no empty placeholder and no
        // mount-point artifact.
        assert!(matches!(host_entry(&missing)?, HostEntry::Missing));
        assert!(matches!(host_entry(&packed_refs)?, HostEntry::Missing));
        assert_eq!(
            host_git_stdout(&repository, &["rev-parse", "--is-shallow-repository"])?,
            "false"
        );

        std::fs::remove_dir_all(root.path())?;
        Ok(())
    }

    #[tokio::test]
    async fn linux_missing_protected_metadata_stays_missing_in_a_linked_worktree() -> Result<()> {
        let root = test_root();
        let repository = root.path().join("repository");
        let linked = root.path().join("linked");
        init_metadata_repository(&repository)?;
        add_metadata_worktree(&repository, &linked, "feature")?;
        let missing = repository.join(".git").join("shallow");
        assert!(matches!(host_entry(&missing)?, HostEntry::Missing));
        assert_eq!(
            host_git_stdout(&repository, &["rev-parse", "--is-shallow-repository"])?,
            "false"
        );

        let linked = std::fs::canonicalize(&linked)?;
        let git_roots = git_metadata_roots(&linked)?;
        let script = format!(
            "if test -e {0}; then echo shallow-exists; else echo shallow-absent; fi; \
             printf 'shallow-before='; git rev-parse --is-shallow-repository; \
             if printf corrupted > {0} 2>/dev/null; then echo write-shallow-ok; \
             else echo write-shallow-denied; fi; \
             printf 'shallow-after='; git rev-parse --is-shallow-repository",
            missing.display()
        );
        let output = run_git_with_pinned_worktree_metadata(
            &command("/usr/bin/sh", &["-c", &script]),
            &linked,
            std::slice::from_ref(&linked),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert_eq!(
            output.stdout,
            "shallow-absent\nshallow-before=false\nwrite-shallow-denied\nshallow-after=false\n"
        );
        assert!(matches!(host_entry(&missing)?, HostEntry::Missing));
        assert_eq!(
            host_git_stdout(&repository, &["rev-parse", "--is-shallow-repository"])?,
            "false"
        );

        std::fs::remove_dir_all(root.path())?;
        Ok(())
    }

    /// Even a read-only sandbox invocation must not materialize a mount-point
    /// artifact for a missing protected path.
    #[tokio::test]
    async fn linux_missing_protected_metadata_leaves_no_artifact_after_read_only_use() -> Result<()>
    {
        let root = test_root();
        let repository = root.path().join("repository");
        init_metadata_repository(&repository)?;
        let missing = repository.join(".git").join("shallow");
        assert!(matches!(host_entry(&missing)?, HostEntry::Missing));

        let git_roots = git_metadata_roots(&repository)?;
        let output = run_git(
            &command(
                "/usr/bin/sh",
                &[
                    "-c",
                    "printf 'inside='; git rev-parse --is-shallow-repository",
                ],
            ),
            &repository,
            std::slice::from_ref(&repository),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert_eq!(output.stdout, "inside=false\n");

        assert!(matches!(host_entry(&missing)?, HostEntry::Missing));

        std::fs::remove_dir_all(root.path())?;
        Ok(())
    }

    /// An existing shallow repository keeps its shallow state and shallow file
    /// inside the sandbox, and the protected file stays immutable.
    #[tokio::test]
    async fn linux_existing_shallow_repository_keeps_its_shallow_state() -> Result<()> {
        let root = test_root();
        let source = root.path().join("source");
        init_metadata_repository(&source)?;
        let shallow_repository = root.path().join("shallow-repository");
        let clone = std::process::Command::new("/usr/bin/git")
            .args([
                "clone",
                "--depth",
                "1",
                "--quiet",
                &format!("file://{}", source.display()),
                shallow_repository
                    .to_str()
                    .context("shallow path is not UTF-8")?,
            ])
            .current_dir(root.path())
            .output()?;
        anyhow::ensure!(
            clone.status.success(),
            "shallow clone failed: {}",
            String::from_utf8_lossy(&clone.stderr)
        );
        let shallow_file = shallow_repository.join(".git").join("shallow");
        let sentinel = std::fs::read(&shallow_file)?;
        assert!(!sentinel.is_empty());
        assert_eq!(
            host_git_stdout(
                &shallow_repository,
                &["rev-parse", "--is-shallow-repository"]
            )?,
            "true"
        );

        let git_roots = git_metadata_roots(&shallow_repository)?;
        let output = run_git(
            &command(
                "/usr/bin/sh",
                &[
                    "-c",
                    "printf 'inside='; git rev-parse --is-shallow-repository; \
                     if printf corrupted > .git/shallow 2>/dev/null; then echo write-ok; \
                     else echo write-denied; fi",
                ],
            ),
            &shallow_repository,
            std::slice::from_ref(&shallow_repository),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert_eq!(output.stdout, "inside=true\nwrite-denied\n");
        match host_entry(&shallow_file)? {
            HostEntry::RegularFile(content) => assert_eq!(content, sentinel),
            other => panic!("shallow file has an unexpected host type: {other:?}"),
        }

        std::fs::remove_dir_all(root.path())?;
        Ok(())
    }

    #[tokio::test]
    async fn linux_existing_metadata_sentinel_is_readable_and_immutable_in_a_primary_checkout()
    -> Result<()> {
        let root = test_root();
        let repository = root.path().join("repository");
        init_metadata_repository(&repository)?;
        let sentinel = "0000000000000000000000000000000000000000 refs/heads/sentinel\n";
        let sentinel_path = repository.join(".git").join("packed-refs");
        std::fs::write(&sentinel_path, sentinel)?;
        assert!(matches!(
            host_entry(&sentinel_path)?,
            HostEntry::RegularFile(_)
        ));

        let git_roots = git_metadata_roots(&repository)?;
        let output = run_git(
            &command(
                "/usr/bin/sh",
                &[
                    "-c",
                    "cat .git/packed-refs; \
                     if printf corrupted > .git/packed-refs 2>/dev/null; then printf 'write-ok\\n'; \
                     else printf 'write-denied\\n'; fi; \
                     cat .git/packed-refs",
                ],
            ),
            &repository,
            std::slice::from_ref(&repository),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert_eq!(output.stdout, format!("{sentinel}write-denied\n{sentinel}"));
        match host_entry(&sentinel_path)? {
            HostEntry::RegularFile(content) => {
                assert_eq!(String::from_utf8(content)?, sentinel);
            }
            other => panic!("unexpected sentinel host type: {other:?}"),
        }

        std::fs::remove_dir_all(root.path())?;
        Ok(())
    }

    #[tokio::test]
    async fn linux_existing_metadata_sentinel_is_readable_and_immutable_in_a_linked_worktree()
    -> Result<()> {
        let root = test_root();
        let repository = root.path().join("repository");
        let linked = root.path().join("linked");
        init_metadata_repository(&repository)?;
        add_metadata_worktree(&repository, &linked, "feature")?;
        let sentinel = "0000000000000000000000000000000000000000 refs/heads/sentinel\n";
        let sentinel_path = repository.join(".git").join("packed-refs");
        std::fs::write(&sentinel_path, sentinel)?;

        let linked = std::fs::canonicalize(&linked)?;
        let git_roots = git_metadata_roots(&linked)?;
        let script = format!(
            "cat {0}; \
             if printf corrupted > {0} 2>/dev/null; then printf 'write-ok\\n'; \
             else printf 'write-denied\\n'; fi; \
             cat {0}",
            sentinel_path.display()
        );
        let output = run_git_with_pinned_worktree_metadata(
            &command("/usr/bin/sh", &["-c", &script]),
            &linked,
            std::slice::from_ref(&linked),
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert_eq!(output.stdout, format!("{sentinel}write-denied\n{sentinel}"));
        match host_entry(&sentinel_path)? {
            HostEntry::RegularFile(content) => {
                assert_eq!(String::from_utf8(content)?, sentinel);
            }
            other => panic!("unexpected sentinel host type: {other:?}"),
        }

        std::fs::remove_dir_all(root.path())?;
        Ok(())
    }

    #[tokio::test]
    async fn linux_linked_worktree_pinned_metadata_scope_serves_git_mutations() -> Result<()> {
        let root = test_root();
        let repository = root.path().join("repository");
        let sibling = root.path().join("sibling");
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
        host_git(&repository, &["branch", "-M", "main"])?;
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
        std::fs::write(repository.join("primary-untracked.txt"), b"keep\n")?;
        let primary_before = (
            host_git_stdout(&repository, &["rev-parse", "HEAD"])?,
            host_git_stdout(&repository, &["status", "--porcelain"])?,
        );
        let sibling_before = (
            host_git_stdout(&sibling, &["branch", "--show-current"])?,
            host_git_stdout(&sibling, &["rev-parse", "HEAD"])?,
        );

        let worktree = std::fs::canonicalize(&worktree)?;
        let git_roots = git_metadata_roots(&worktree)?;
        assert_eq!(git_roots.len(), 2);
        let writable = [worktree.clone()];

        // The contained scope used by ordinary structured Git tools still
        // fails closed for this linked worktree.
        let denied = run_git(
            &command("/usr/bin/git", &["switch", "-c", "agent/denied"]),
            &worktree,
            &writable,
            &git_roots,
            None,
        )
        .await;
        let error = denied.expect_err("the contained scope must reject linked metadata");
        assert!(
            error
                .to_string()
                .contains("outside the permitted session roots"),
            "{error:#}"
        );

        // The pinned scope used by the broker serves switch/add/commit with
        // only the linked worktree writable.
        let switch = run_git_with_pinned_worktree_metadata(
            &command("/usr/bin/git", &["switch", "-c", "agent/pinned"]),
            &worktree,
            &writable,
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(switch.status, 0, "{}", switch.stderr);
        std::fs::write(worktree.join("agent.txt"), b"agent\n")?;
        let add = run_git_with_pinned_worktree_metadata(
            &command("/usr/bin/git", &["add", "--", "agent.txt"]),
            &worktree,
            &writable,
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(add.status, 0, "{}", add.stderr);
        let commit = run_git_with_pinned_worktree_metadata(
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
                    "linked worktree pinned metadata",
                ],
            ),
            &worktree,
            &writable,
            &git_roots,
            None,
        )
        .await?;
        assert_eq!(commit.status, 0, "{}", commit.stderr);

        assert_eq!(
            host_git_stdout(&worktree, &["branch", "--show-current"])?,
            "agent/pinned"
        );
        assert_eq!(
            host_git_stdout(&worktree, &["show", "HEAD:agent.txt"])?,
            "agent"
        );
        assert_eq!(
            (
                host_git_stdout(&repository, &["rev-parse", "HEAD"])?,
                host_git_stdout(&repository, &["status", "--porcelain"])?,
            ),
            primary_before
        );
        assert_eq!(
            (
                host_git_stdout(&sibling, &["branch", "--show-current"])?,
                host_git_stdout(&sibling, &["rev-parse", "HEAD"])?,
            ),
            sibling_before
        );
        assert_eq!(
            host_git_stdout(&repository, &["branch", "--show-current"])?,
            "main"
        );

        // The metadata policy still protects sensitive common-directory paths
        // through the pinned scope.
        let protected = git_roots[0].join("config");
        let denied = run_git_with_pinned_worktree_metadata(
            &command("/usr/bin/touch", &[protected.to_str().unwrap()]),
            &worktree,
            &writable,
            &git_roots,
            None,
        )
        .await?;
        assert_ne!(denied.status, 0);

        std::fs::remove_dir_all(root.path())?;
        Ok(())
    }

    #[tokio::test]
    async fn linux_local_agent_scope_rejects_a_swapped_worktree_identity_before_spawn() -> Result<()>
    {
        let root = test_root();
        let primary = root.path().join("primary");
        let linked = root.path().join("linked");
        let other = root.path().join("other");
        let other_linked = root.path().join("other-linked");
        for repository in [&primary, &other] {
            std::fs::create_dir_all(repository)?;
            host_git(repository, &["init", "-q"])?;
            std::fs::write(repository.join("base.txt"), b"base\n")?;
            host_git(repository, &["add", "--", "base.txt"])?;
            host_git(
                repository,
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
        }
        host_git(
            &primary,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ],
        )?;
        host_git(
            &other,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "other-branch",
                other_linked.to_str().unwrap(),
            ],
        )?;

        let linked = std::fs::canonicalize(&linked)?;
        let expected = WorkspaceRepositoryIdentity::for_workspace(&linked)?;
        assert!(expected.linked_worktree());

        // Swap the validated target's `.git` pointer to the other
        // repository's structurally valid private metadata.
        let other_private =
            std::fs::canonicalize(other.join(".git").join("worktrees").join("other-linked"))?;
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", other_private.display()),
        )?;
        std::fs::write(
            other_private.join("gitdir"),
            format!("{}\n", linked.join(".git").display()),
        )?;

        let command = command("/bin/sh", &["-c", "pwd > ran-in.txt"]);
        let temporary = root.path().to_path_buf();
        let error = run_local_agent(
            &command,
            &linked,
            LocalAgentScope {
                writable_roots: std::slice::from_ref(&linked),
                temporary_roots: std::slice::from_ref(&temporary),
                read_only_paths: &[],
                read_only_roots: &[],
                read_only_symlinks: &[],
                read_only_scaffold_directories: &[],
                read_only_files: &[],
                hidden_roots: &[],
                expected_repository: Some(&expected),
            },
            None,
            &HashMap::new(),
        )
        .await
        .expect_err("a swapped worktree identity must fail before spawn");
        assert!(
            error.to_string().contains("managed worktree identity"),
            "{error:#}"
        );
        assert!(!linked.join("ran-in.txt").exists());

        std::fs::remove_dir_all(root.path())?;
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
    async fn linux_structured_worktree_add_creates_the_namespace_on_first_use() -> Result<()> {
        let root = test_root();
        let repository = root.path().join("repository");
        let destination = repository.join(".wt").join("first");
        std::fs::create_dir_all(destination.parent().unwrap())?;
        init_metadata_repository(&repository)?;
        assert!(!repository.join(".git").join("worktrees").exists());

        let git_roots = git_metadata_roots(&repository)?;
        let add = vec![
            "/usr/bin/git".to_owned(),
            "-c".to_owned(),
            "core.hooksPath=/dev/null".to_owned(),
            "worktree".to_owned(),
            "add".to_owned(),
            "-b".to_owned(),
            "first".to_owned(),
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
        assert!(repository.join(".git").join("worktrees").is_dir());
        assert!(destination.join(".git").is_file());
        assert_eq!(
            std::process::Command::new("/usr/bin/git")
                .args(["branch", "--show-current"])
                .current_dir(&destination)
                .output()?
                .stdout,
            b"first\n"
        );

        std::fs::remove_dir_all(root.path())?;
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
    async fn linux_ordinary_command_network_policy_controls_host_loopback() -> Result<()> {
        let root = test_root();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let accept = tokio::spawn(async move {
            for _ in 0..2 {
                let _ = listener.accept().await;
            }
        });
        let connect = command(
            "/bin/bash",
            &["-c", &format!("exec 3<>/dev/tcp/127.0.0.1/{port}")],
        );

        let development = run_with_network_policy(
            &connect,
            &workspace,
            &[],
            CommandNetworkPolicy::Development,
            None,
        )
        .await?;
        assert_eq!(
            development.status, 0,
            "development profile must reach host loopback: {}",
            development.stderr
        );

        let restricted = run_with_network_policy(
            &connect,
            &workspace,
            &[],
            CommandNetworkPolicy::Restricted,
            None,
        )
        .await?;
        assert_ne!(
            restricted.status, 0,
            "restricted profile must not reach host loopback"
        );

        accept.abort();
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
                expected_repository: None,
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
                expected_repository: None,
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
                expected_repository: None,
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
                expected_repository: None,
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
                expected_repository: None,
            },
            None,
            &environment,
        )
        .await?;
        assert_eq!(network.status, 0, "{}", network.stderr);
        assert!(accepted.join().unwrap());
        Ok(())
    }

    #[tokio::test]
    async fn linux_local_agent_git_shim_executes_from_state_and_uses_the_private_queue()
    -> Result<()> {
        let root = test_root();
        let workspace = root.path().join("workspace");
        let state = root.path().join("state");
        let state_tmp = state.join("tmp");
        let state_bin = state.join("bin");
        let host_bin = root.path().join("host-bin");
        let broker = state.join("git-broker");
        let requests = broker.join("requests");
        let responses = root.path().join("responses");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&state_tmp)?;
        std::fs::create_dir_all(&state_bin)?;
        std::fs::create_dir_all(&host_bin)?;
        std::fs::create_dir_all(&requests)?;
        std::fs::create_dir_all(&responses)?;

        // The production shim is a symlink from the private state bin directory
        // to the host binary, which the policy re-exposes as a read-only file.
        // The response queue is re-exposed read-only: the shim can read a
        // broker response, but the sandboxed process cannot forge, replace, or
        // remove one.
        let target = host_bin.join("git-target");
        std::fs::write(
            &target,
            "#!/bin/sh\nset -eu\nprintf '{\"schema\":1}' > \"$TEMOTE_MCP_GIT_BROKER_DIR/requests/probe.json\"\nif /bin/echo forged > \"$TEMOTE_MCP_GIT_BROKER_RESPONSES_DIR/forged.json\" 2>/dev/null; then echo allowed > \"$TMPDIR/forgery-report.txt\"; else echo denied > \"$TMPDIR/forgery-report.txt\"; fi\nif /bin/rm \"$TEMOTE_MCP_GIT_BROKER_RESPONSES_DIR/probe.json\" 2>/dev/null; then echo removed > \"$TMPDIR/unlink-report.txt\"; else echo kept > \"$TMPDIR/unlink-report.txt\"; fi\ncat \"$TEMOTE_MCP_GIT_BROKER_RESPONSES_DIR/probe.json\"\n",
        )?;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))?;
        let target = std::fs::canonicalize(&target)?;
        std::os::unix::fs::symlink(&target, state_bin.join("git"))?;
        std::fs::write(responses.join("probe.json"), b"{\"ok\":true}")?;

        let environment = HashMap::from([
            ("HOME".to_owned(), state.to_string_lossy().into_owned()),
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            (
                "TMPDIR".to_owned(),
                state_tmp.to_string_lossy().into_owned(),
            ),
            (
                "TEMOTE_MCP_GIT_BROKER_DIR".to_owned(),
                broker.to_string_lossy().into_owned(),
            ),
            (
                "TEMOTE_MCP_GIT_BROKER_RESPONSES_DIR".to_owned(),
                responses.to_string_lossy().into_owned(),
            ),
        ]);
        let hidden_root = root.path().to_path_buf();
        let shim = state_bin.join("git");
        let shim_argument = shim.to_str().context("shim path is not UTF-8")?;
        let shim_command = command(shim_argument, &[]);
        let output = run_local_agent(
            &shim_command,
            &workspace,
            LocalAgentScope {
                writable_roots: &[workspace.clone(), state.clone()],
                temporary_roots: std::slice::from_ref(&state_tmp),
                read_only_paths: &[],
                read_only_roots: std::slice::from_ref(&responses),
                read_only_symlinks: &[],
                read_only_scaffold_directories: &[],
                read_only_files: std::slice::from_ref(&target),
                hidden_roots: std::slice::from_ref(&hidden_root),
                expected_repository: None,
            },
            None,
            &environment,
        )
        .await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert_eq!(output.stdout, "{\"ok\":true}");
        assert_eq!(
            std::fs::read_to_string(requests.join("probe.json"))?,
            "{\"schema\":1}"
        );
        assert_eq!(
            std::fs::read_to_string(state_tmp.join("forgery-report.txt"))?.trim(),
            "denied"
        );
        assert_eq!(
            std::fs::read_to_string(state_tmp.join("unlink-report.txt"))?.trim(),
            "kept"
        );
        assert_eq!(
            std::fs::read_to_string(responses.join("probe.json"))?,
            "{\"ok\":true}"
        );
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

#[cfg(all(test, target_os = "linux"))]
mod phase24_staging_tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    fn disposable_repository(base: &Path, name: &str) -> PathBuf {
        let repository = base.join(name);
        std::fs::create_dir(&repository).unwrap();
        let output = std::process::Command::new("/usr/bin/git")
            .args(["init", "--quiet"])
            .current_dir(&repository)
            .output()
            .expect("host git is required for the staging tests");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        repository
    }

    fn prepare_staging(repository: &Path) -> GitWorktreeStaging {
        let common = repository.join(".git");
        GitWorktreeStaging::prepare(repository, std::slice::from_ref(&common))
            .expect("staging preparation must succeed")
            .expect("a primary checkout must be staged")
    }

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    fn make_fifo(path: &Path) {
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `name` is a valid NUL-terminated path and the file is
        // created inside the disposable fixture.
        let result = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    }

    fn temporary_sync_files(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".temote-git-sync-"))
            })
            .collect()
    }

    #[test]
    fn phase24_staging_head_lock_collision_preserves_other_lock_and_releases_own_index_lock() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        std::fs::write(common.join("HEAD.lock"), b"other-worker-lock").unwrap();
        let error = GitWorktreeStaging::prepare(&repository, std::slice::from_ref(&common))
            .expect_err("preparation must fail while another HEAD lock exists");
        assert!(
            error
                .to_string()
                .contains("another Git operation is in progress"),
            "{error}"
        );
        assert_eq!(read(&common.join("HEAD.lock")), b"other-worker-lock");
        assert!(
            !common.join("index.lock").exists(),
            "preparation failure leaked its own index.lock"
        );
    }

    #[test]
    fn phase24_staging_index_lock_collision_preserves_other_lock() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        std::fs::write(common.join("index.lock"), b"other-worker-lock").unwrap();
        let error = GitWorktreeStaging::prepare(&repository, std::slice::from_ref(&common))
            .expect_err("preparation must fail while another index lock exists");
        assert!(
            error
                .to_string()
                .contains("another Git operation is in progress"),
            "{error}"
        );
        assert_eq!(read(&common.join("index.lock")), b"other-worker-lock");
        assert!(!common.join("HEAD.lock").exists());
    }

    #[test]
    fn phase24_staging_lock_is_acquired_before_snapshot_is_read() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        std::fs::write(common.join("HEAD.lock"), b"other-worker-lock").unwrap();
        // A symlinked entry would make the snapshot fail if it ran first.
        std::os::unix::fs::symlink(fixture.path().join("outside-index"), common.join("index"))
            .unwrap();
        let error = GitWorktreeStaging::prepare(&repository, std::slice::from_ref(&common))
            .expect_err("preparation must fail while another HEAD lock exists");
        assert!(
            error
                .to_string()
                .contains("another Git operation is in progress"),
            "the snapshot was read before the locks were acquired: {error}"
        );
        assert_eq!(read(&common.join("HEAD.lock")), b"other-worker-lock");
        assert!(!common.join("index.lock").exists());
    }

    #[test]
    fn phase24_staging_snapshot_failure_releases_owned_locks() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        make_fifo(&common.join("index"));
        let error = GitWorktreeStaging::prepare(&repository, std::slice::from_ref(&common))
            .expect_err("a special-file snapshot entry must fail closed");
        assert!(error.to_string().contains("not a regular file"), "{error}");
        assert!(
            !common.join("index.lock").exists(),
            "the owned index lock was not released"
        );
        assert!(
            !common.join("HEAD.lock").exists(),
            "the owned HEAD lock was not released"
        );
    }

    #[test]
    fn phase24_staging_drop_removes_private_staging_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let staged = prepare_staging(&repository);
        let directory = staged.directory_path().to_owned();
        assert!(directory.is_dir());
        drop(staged);
        assert!(!directory.exists(), "staging directory remains after Drop");
        assert!(!common.join("index.lock").exists());
        assert!(!common.join("HEAD.lock").exists());
    }

    #[test]
    fn phase24_staging_drop_does_not_remove_a_replaced_staging_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let staged = prepare_staging(&repository);
        let directory = staged.directory_path().to_owned();
        let moved = fixture.path().join("moved-staging");
        std::fs::rename(&directory, &moved).unwrap();
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("sentinel"), b"unrelated").unwrap();
        drop(staged);
        assert_eq!(
            read(&directory.join("sentinel")),
            b"unrelated",
            "Drop removed an unrelated directory"
        );
        assert!(moved.is_dir());
        assert!(!moved.join("HEAD").exists());
        assert!(!moved.join("logs").exists());
        std::fs::remove_dir_all(&directory).unwrap();
        std::fs::remove_dir_all(&moved).unwrap();
    }

    #[test]
    fn phase24_staging_rejects_symlinked_metadata_root() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let link = fixture.path().join("git-link");
        std::os::unix::fs::symlink(repository.join(".git"), &link).unwrap();
        let error = GitWorktreeStaging::prepare(&repository, std::slice::from_ref(&link))
            .expect_err("a symlinked metadata root must fail closed");
        assert!(error.to_string().contains("symbolic-link"), "{error}");
    }

    #[test]
    fn phase24_staging_apply_rejects_retargeted_common_directory_and_preserves_unrelated_repository()
     {
        let fixture = tempfile::tempdir().unwrap();
        let repository_a = disposable_repository(fixture.path(), "repository-a");
        let repository_b = disposable_repository(fixture.path(), "repository-b");
        let common_a = repository_a.join(".git");
        let common_b = repository_b.join(".git");
        let unrelated_before = read(&common_b.join("HEAD"));
        let staged = prepare_staging(&repository_a);
        std::fs::write(
            staged.directory_path().join("HEAD"),
            b"ref: refs/heads/phase24-probe\n",
        )
        .unwrap();
        std::fs::write(common_b.join("index.lock"), b"other-repository-lock").unwrap();
        std::fs::rename(&common_a, repository_a.join("saved-git")).unwrap();
        std::os::unix::fs::symlink(&common_b, &common_a).unwrap();
        let error = staged
            .apply()
            .expect_err("apply must reject a retargeted common directory");
        assert!(
            error
                .to_string()
                .contains("replaced after staging was prepared"),
            "{error}"
        );
        let directory = staged.directory_path().to_owned();
        drop(staged);
        assert_eq!(read(&common_b.join("HEAD")), unrelated_before);
        assert_eq!(
            read(&common_b.join("index.lock")),
            b"other-repository-lock",
            "Drop removed an unrelated repository lock"
        );
        assert_eq!(
            read(&common_b.join("HEAD")),
            unrelated_before,
            "apply mutated an unrelated repository"
        );
        assert!(
            !repository_a.join("saved-git").join("index.lock").exists(),
            "the owned lock was not released in the verified metadata directory"
        );
        assert!(!repository_a.join("saved-git").join("HEAD.lock").exists());
        assert!(!directory.exists());
    }

    #[test]
    fn phase24_staging_apply_rejects_symlinked_reflog_parent() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let outside = fixture.path().join("outside-metadata");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("HEAD"), b"sentinel").unwrap();
        let staged = prepare_staging(&repository);
        std::fs::write(staged.directory_path().join("logs/HEAD"), b"staged-reflog").unwrap();
        std::os::unix::fs::symlink(&outside, common.join("logs")).unwrap();
        let error = staged
            .apply()
            .expect_err("a symlinked reflog parent must fail closed");
        assert!(!format!("{error:#}").is_empty());
        assert!(
            std::fs::symlink_metadata(common.join("logs"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(read(&outside.join("HEAD")), b"sentinel");
    }

    #[test]
    fn phase24_staging_apply_rejects_symlinked_staged_entry() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let before = read(&common.join("HEAD"));
        let staged = prepare_staging(&repository);
        let outside = fixture.path().join("outside-staged-head");
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::remove_file(staged.directory_path().join("HEAD")).unwrap();
        std::os::unix::fs::symlink(&outside, staged.directory_path().join("HEAD")).unwrap();
        let error = staged
            .apply()
            .expect_err("a symlinked staged entry must fail closed");
        assert!(!format!("{error:#}").is_empty());
        assert_eq!(read(&common.join("HEAD")), before);
    }

    #[test]
    fn phase24_staging_apply_rejects_symlinked_target_entry() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let staged = prepare_staging(&repository);
        std::fs::write(
            staged.directory_path().join("HEAD"),
            b"ref: refs/heads/phase24-symlink\n",
        )
        .unwrap();
        let outside = fixture.path().join("outside-target-head");
        std::fs::write(&outside, b"sentinel").unwrap();
        std::fs::remove_file(common.join("HEAD")).unwrap();
        std::os::unix::fs::symlink(&outside, common.join("HEAD")).unwrap();
        let error = staged
            .apply()
            .expect_err("a symlinked target entry must fail closed");
        assert!(error.to_string().contains("non-regular"), "{error}");
        assert_eq!(read(&outside), b"sentinel");
    }

    #[test]
    fn phase24_staging_apply_rejects_special_target_entry() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let staged = prepare_staging(&repository);
        std::fs::write(staged.directory_path().join("index"), b"staged-index").unwrap();
        make_fifo(&common.join("index"));
        let error = staged
            .apply()
            .expect_err("a special-file target must fail closed");
        assert!(error.to_string().contains("non-regular"), "{error}");
        assert!(
            std::fs::symlink_metadata(common.join("index"))
                .unwrap()
                .file_type()
                .is_fifo()
        );
        assert!(!common.join(".temote-git-sync-probe").exists());
    }

    #[test]
    fn phase24_staging_apply_preserves_state_the_command_did_not_change() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let staged = prepare_staging(&repository);
        let drift = b"ref: refs/heads/host-drift\n";
        std::fs::write(common.join("HEAD"), drift).unwrap();
        std::fs::write(staged.directory_path().join("index"), b"staged-index").unwrap();
        staged.apply().unwrap();
        assert_eq!(
            read(&common.join("HEAD")),
            drift,
            "apply restored state the command did not change"
        );
        assert_eq!(read(&common.join("index")), b"staged-index");
    }

    #[test]
    fn phase24_staging_apply_writes_changed_state_and_preserves_permissions() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let staged = prepare_staging(&repository);
        let staged_head = staged.directory_path().join("HEAD");
        std::fs::write(&staged_head, b"ref: refs/heads/phase24-applied\n").unwrap();
        std::fs::set_permissions(&staged_head, std::fs::Permissions::from_mode(0o640)).unwrap();
        staged.apply().unwrap();
        assert_eq!(
            read(&common.join("HEAD")),
            b"ref: refs/heads/phase24-applied\n"
        );
        let mode = std::fs::symlink_metadata(common.join("HEAD"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o640, "apply did not preserve the staged permissions");
    }

    #[test]
    fn phase24_staging_apply_applies_changed_reflog_into_new_logs_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        assert!(!common.join("logs").exists());
        let staged = prepare_staging(&repository);
        std::fs::write(staged.directory_path().join("logs/HEAD"), b"phase24-reflog").unwrap();
        staged.apply().unwrap();
        assert!(common.join("logs").is_dir());
        assert_eq!(read(&common.join("logs/HEAD")), b"phase24-reflog");
    }

    #[test]
    fn phase24_staging_apply_rejects_symlinked_staged_reflog_parent() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let staged = prepare_staging(&repository);
        std::fs::write(staged.directory_path().join("logs/HEAD"), b"staged-reflog").unwrap();
        let outside = fixture.path().join("outside-reflog");
        std::fs::create_dir(&outside).unwrap();
        std::fs::remove_dir_all(staged.directory_path().join("logs")).unwrap();
        std::os::unix::fs::symlink(&outside, staged.directory_path().join("logs")).unwrap();
        let error = staged
            .apply()
            .expect_err("a symlinked staging parent must fail closed");
        assert!(!format!("{error:#}").is_empty());
        assert!(!common.join("logs").exists());
    }

    #[test]
    fn phase24_staging_no_apply_when_the_command_has_no_result() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let before = read(&common.join("HEAD"));
        let staged = prepare_staging(&repository);
        std::fs::write(
            staged.directory_path().join("HEAD"),
            b"ref: refs/heads/phase24-pending\n",
        )
        .unwrap();
        let failure: Result<Output> = Err(anyhow::anyhow!("sandbox setup failed"));
        finalize_staged_git_state(Some(&staged), &failure).unwrap();
        assert_eq!(
            read(&common.join("HEAD")),
            before,
            "a stale snapshot was applied after a command without a result"
        );
    }

    #[test]
    fn phase24_staging_apply_runs_for_completed_command_even_on_nonzero_exit() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let staged = prepare_staging(&repository);
        std::fs::write(
            staged.directory_path().join("HEAD"),
            b"ref: refs/heads/phase24-partial\n",
        )
        .unwrap();
        let completed = Ok(Output {
            status: 1,
            stdout: String::new(),
            stderr: "partial update".to_owned(),
            truncated: false,
        });
        finalize_staged_git_state(Some(&staged), &completed).unwrap();
        assert_eq!(
            read(&common.join("HEAD")),
            b"ref: refs/heads/phase24-partial\n",
            "a legitimate partial update of a failed command was discarded"
        );
    }

    #[test]
    fn phase24_staging_owned_lock_replacement_is_not_removed() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let staged = prepare_staging(&repository);
        assert!(common.join("index.lock").is_file());
        assert!(common.join("HEAD.lock").is_file());
        for name in ["index.lock", "HEAD.lock"] {
            std::fs::remove_file(common.join(name)).unwrap();
            std::fs::write(common.join(name), b"other-worker-replacement").unwrap();
        }
        drop(staged);
        assert_eq!(
            read(&common.join("index.lock")),
            b"other-worker-replacement",
            "Drop removed a lock that was replaced after acquisition"
        );
        assert_eq!(read(&common.join("HEAD.lock")), b"other-worker-replacement");
    }

    #[test]
    fn phase24_staging_apply_failure_cleans_temporary_files_and_reports_already_applied() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = disposable_repository(fixture.path(), "repository");
        let common = repository.join(".git");
        let mut staged = prepare_staging(&repository);
        std::fs::write(
            staged.directory_path().join("HEAD"),
            b"ref: refs/heads/phase24-partial-apply\n",
        )
        .unwrap();
        std::fs::write(staged.directory_path().join("index"), b"phase24-index").unwrap();
        staged.fail_before_rename = Some("index");
        let error = staged
            .apply()
            .expect_err("the injected apply failure must surface as an error");
        let message = format!("{error:#}");
        assert!(
            message.contains("already applied: HEAD"),
            "apply failure did not report the applied state: {message}"
        );
        assert_eq!(
            read(&common.join("HEAD")),
            b"ref: refs/heads/phase24-partial-apply\n"
        );
        assert!(!common.join("index").exists());
        assert!(
            temporary_sync_files(&common).is_empty(),
            "apply failure left temporary files behind"
        );
    }
}
