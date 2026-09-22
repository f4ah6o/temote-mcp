use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const MANAGED_WORKTREE_ROOT_NAME: &str = "worktrees";
pub(crate) const MANAGED_SRC_ROOT_NAME: &str = "src";
pub(crate) const MAX_MANAGED_TASK_BYTES: usize = 64;
pub(crate) const MAX_MANAGED_REPOSITORY_BYTES: usize = 255;
const WORKTREE_RESERVATION_DIRECTORY_NAME: &str = "worktree-reservations";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorktreeReservationMode {
    Shared,
    Exclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReservationNamespace {
    Worktree,
    Repository,
}

/// A cross-process, per-worktree lifecycle reservation.
///
/// The lock file lives in Temote's trusted per-user state, never below a
/// caller-selected checkout.  The file is deliberately retained after the
/// guard is dropped: its stable hash-derived name gives every process the same
/// lock identity while the advisory `flock` is released by RAII.
pub(crate) struct WorktreeReservation {
    file: File,
    identity: PathBuf,
    namespace: ReservationNamespace,
}

impl std::fmt::Debug for WorktreeReservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorktreeReservation")
            .field("identity", &self.identity)
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

impl WorktreeReservation {
    pub(crate) fn identity(&self) -> &Path {
        &self.identity
    }
}

impl Drop for WorktreeReservation {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // The file remains in the trusted state directory so another
            // process cannot race creation of a different lock identity.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// A repository-scoped lifecycle gate.  It deliberately has a distinct type
/// and lock namespace from per-worktree ownership reservations so a primary
/// checkout cannot alias its repository gate with its target lock.
pub(crate) struct RepositoryReservation {
    reservation: WorktreeReservation,
}

impl std::fmt::Debug for RepositoryReservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositoryReservation")
            .field("identity", &self.reservation.identity)
            .finish_non_exhaustive()
    }
}

/// A sorted set of per-worktree reservations.  Keeping all guards in one RAII
/// value makes it difficult for a caller to accidentally release one lock
/// before its mutation and post-verification have completed.
pub(crate) struct WorktreeReservations {
    reservations: Vec<WorktreeReservation>,
}

impl std::fmt::Debug for WorktreeReservations {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorktreeReservations")
            .field("identities", &self.identities())
            .finish()
    }
}

impl WorktreeReservations {
    pub(crate) fn identities(&self) -> Vec<&Path> {
        self.reservations
            .iter()
            .map(WorktreeReservation::identity)
            .collect()
    }
}

/// A short-lived lifecycle admission for one resolved cwd.  The reservation
/// is held while the caller revalidates the cwd/worktree identity and makes
/// its active ownership visible; callers should then release it before the
/// long-running operation begins.
pub(crate) struct WorktreeAdmission {
    pub(crate) cwd: PathBuf,
    pub(crate) worktree_root: Option<PathBuf>,
    pub(crate) repository_reservation: Option<RepositoryReservation>,
    pub(crate) reservation: Option<WorktreeReservation>,
}

impl std::fmt::Debug for WorktreeAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorktreeAdmission")
            .field("cwd", &self.cwd)
            .field("worktree_root", &self.worktree_root)
            .field("repository_reservation", &self.repository_reservation)
            .field("reservation", &self.reservation)
            .finish()
    }
}

fn reservation_directory() -> Result<PathBuf> {
    let state = crate::config::state_dir()?;
    if let Some(parent) = state.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "cannot create Temote state directory parent {}",
                parent.display()
            )
        })?;
    }
    match std::fs::create_dir(&state) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("cannot create Temote state directory {}", state.display())
            });
        }
    }
    let state_metadata = std::fs::symlink_metadata(&state)
        .with_context(|| format!("cannot inspect Temote state directory {}", state.display()))?;
    anyhow::ensure!(
        state_metadata.is_dir() && !state_metadata.file_type().is_symlink(),
        "Temote state directory must be a normal directory: {}",
        state.display()
    );
    anyhow::ensure!(
        std::fs::canonicalize(&state).with_context(|| format!(
            "cannot resolve Temote state directory {}",
            state.display()
        ))? == state,
        "Temote state directory must be canonical and not a swapped path: {}",
        state.display()
    );
    #[cfg(unix)]
    anyhow::ensure!(
        state_metadata.uid() == unsafe { libc::geteuid() },
        "Temote state directory is not owned by the current user: {}",
        state.display()
    );
    let directory = state.join(WORKTREE_RESERVATION_DIRECTORY_NAME);
    ensure_trusted_owner_directory(&directory, "worktree reservation directory", true)?;
    Ok(directory)
}

fn ensure_trusted_owner_directory(path: &Path, label: &str, create: bool) -> Result<()> {
    let mut created = false;
    if create {
        if let Some(parent) = path.parent() {
            // The state root is selected by Temote's platform authority, not
            // by a request.  Create it only when absent, then validate every
            // component we use before placing a lock below it.
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {label} parent {}", parent.display()))?;
        }
        match std::fs::create_dir(path) {
            Ok(()) => created = true,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot create {label} {}", path.display()));
            }
        }
    }
    #[cfg(unix)]
    if created {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot protect {label} {}", path.display()))?;
    }
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect {label} {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} must be a normal directory: {}",
        path.display()
    );
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {label} {}", path.display()))?;
    anyhow::ensure!(
        canonical == path,
        "{label} must be canonical and not a swapped path: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        let owner = unsafe { libc::geteuid() };
        anyhow::ensure!(
            metadata.uid() == owner,
            "{label} is not owned by the current user: {}",
            path.display()
        );
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o700 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("cannot protect {label} {}", path.display()))?;
        }
        let protected = std::fs::symlink_metadata(path)
            .with_context(|| format!("cannot recheck protected {label} {}", path.display()))?;
        let protected_mode = protected.permissions().mode() & 0o777;
        anyhow::ensure!(
            protected.is_dir()
                && !protected.file_type().is_symlink()
                && protected.uid() == owner
                && protected_mode & 0o077 == 0,
            "{label} must be owner-only (mode {protected_mode:04o}): {}",
            path.display()
        );
    }
    Ok(())
}

fn reservation_identity(path: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "worktree reservation identity must be absolute: {}",
        path.display()
    );
    // Existing worktrees must use the canonical Git root.  A stale prune
    // entry may no longer exist; in that case the broker-derived absolute
    // entry itself is the stable identity.  Parent-directory traversal is
    // rejected so a malformed broker entry cannot select an unrelated lock.
    match std::fs::canonicalize(path) {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::ensure!(
                path.components()
                    .all(|component| !matches!(component, Component::ParentDir)),
                "stale worktree reservation identity contains parent traversal: {}",
                path.display()
            );
            Ok(path.to_path_buf())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "cannot resolve worktree reservation identity {}",
                path.display()
            )
        }),
    }
}

pub(crate) fn worktree_reservation_identity(path: &Path) -> Result<PathBuf> {
    reservation_identity(path)
}

fn reservation_path(directory: &Path, namespace: ReservationNamespace, identity: &Path) -> PathBuf {
    let namespace_tag = match namespace {
        ReservationNamespace::Worktree => b"temote-worktree-reservation-v1\0" as &[u8],
        ReservationNamespace::Repository => b"temote-repository-gate-v1\0",
    };
    let mut hasher = Sha256::new();
    hasher.update(namespace_tag);
    #[cfg(unix)]
    hasher.update(identity.as_os_str().as_bytes());
    #[cfg(not(unix))]
    hasher.update(identity.as_os_str().to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let digest = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    directory.join(format!("{digest}.lock"))
}

fn open_reservation_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .with_context(|| format!("cannot open worktree reservation {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect worktree reservation {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "worktree reservation must be a regular file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "worktree reservation is not owned by the current user: {}",
            path.display()
        );
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "worktree reservation must be owner-only (mode {mode:04o}): {}",
            path.display()
        );
        if mode != 0o600 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| {
                    format!("cannot protect worktree reservation {}", path.display())
                })?;
        }
    }
    Ok(file)
}

fn acquire_worktree_reservation_inner(
    path: &Path,
    namespace: ReservationNamespace,
    mode: WorktreeReservationMode,
    nonblocking: bool,
) -> Result<WorktreeReservation> {
    let identity = reservation_identity(path)?;
    let directory = reservation_directory()?;
    let lock_path = reservation_path(&directory, namespace, &identity);
    let file = open_reservation_file(&lock_path)?;
    #[cfg(unix)]
    {
        let lock_type = match mode {
            WorktreeReservationMode::Shared => libc::LOCK_SH,
            WorktreeReservationMode::Exclusive => libc::LOCK_EX,
        };
        let operation = lock_type | if nonblocking { libc::LOCK_NB } else { 0 };
        let locked = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if nonblocking {
            anyhow::ensure!(
                locked == 0,
                "another Temote operation owns this Git worktree reservation: {}",
                identity.display()
            );
        } else {
            anyhow::ensure!(
                locked == 0,
                "failed to acquire Git worktree reservation: {}",
                identity.display()
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        let _ = nonblocking;
        anyhow::bail!("Git worktree lifecycle reservations require a platform file-lock primitive");
    }
    Ok(WorktreeReservation {
        file,
        identity,
        namespace,
    })
}

/// Acquires one reservation, waiting for another Temote operation to release
/// it.  Callers must retain the returned guard across revalidation, mutation,
/// and post-verification.
#[cfg(test)]
pub(crate) fn acquire_worktree_reservation(path: &Path) -> Result<WorktreeReservation> {
    acquire_worktree_reservation_inner(
        path,
        ReservationNamespace::Worktree,
        WorktreeReservationMode::Exclusive,
        false,
    )
}

/// Attempts one reservation without waiting.  This is useful for deterministic
/// admission checks and callers that prefer to fail closed immediately.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn try_acquire_worktree_reservation(path: &Path) -> Result<WorktreeReservation> {
    acquire_worktree_reservation_inner(
        path,
        ReservationNamespace::Worktree,
        WorktreeReservationMode::Exclusive,
        true,
    )
}

/// Acquires a shared reservation for an admitted session or job.  Multiple
/// operations may hold this guard concurrently, while an exclusive cleanup
/// reservation cannot be acquired until every active operation releases it.
pub(crate) fn acquire_shared_worktree_reservation(path: &Path) -> Result<WorktreeReservation> {
    acquire_worktree_reservation_inner(
        path,
        ReservationNamespace::Worktree,
        WorktreeReservationMode::Shared,
        false,
    )
}

pub(crate) async fn acquire_shared_worktree_reservation_async(
    path: &Path,
) -> Result<WorktreeReservation> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || acquire_shared_worktree_reservation(&path))
        .await
        .context("shared worktree reservation worker failed")?
}

fn acquire_repository_reservation_inner(
    path: &Path,
    mode: WorktreeReservationMode,
    nonblocking: bool,
) -> Result<RepositoryReservation> {
    Ok(RepositoryReservation {
        reservation: acquire_worktree_reservation_inner(
            path,
            ReservationNamespace::Repository,
            mode,
            nonblocking,
        )?,
    })
}

pub(crate) async fn acquire_shared_repository_reservation_async(
    path: &Path,
) -> Result<RepositoryReservation> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        acquire_repository_reservation_inner(&path, WorktreeReservationMode::Shared, false)
    })
    .await
    .context("shared repository gate worker failed")?
}

pub(crate) async fn try_acquire_repository_reservation_async(
    path: &Path,
) -> Result<RepositoryReservation> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        acquire_repository_reservation_inner(&path, WorktreeReservationMode::Exclusive, true)
    })
    .await
    .context("exclusive repository gate worker failed")?
}

/// Attempts an exclusive cleanup reservation without waiting for an active
/// session/job in another process.  Cleanup must fail closed at this point
/// instead of waiting indefinitely for an unknown owner.
pub(crate) async fn try_acquire_worktree_reservation_async(
    path: &Path,
) -> Result<WorktreeReservation> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        acquire_worktree_reservation_inner(
            &path,
            ReservationNamespace::Worktree,
            WorktreeReservationMode::Exclusive,
            true,
        )
    })
    .await
    .context("exclusive worktree reservation worker failed")?
}

fn reservation_identities(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut identities = paths
        .iter()
        .map(|path| reservation_identity(path))
        .collect::<Result<Vec<_>>>()?;
    identities.sort();
    identities.dedup();
    Ok(identities)
}

fn acquire_worktree_reservations_inner(
    paths: &[PathBuf],
    mode: WorktreeReservationMode,
    nonblocking: bool,
) -> Result<WorktreeReservations> {
    let identities = reservation_identities(paths)?;
    let directory = reservation_directory()?;
    let mut reservations = Vec::with_capacity(identities.len());
    for identity in identities {
        let lock_path = reservation_path(&directory, ReservationNamespace::Worktree, &identity);
        match open_reservation_file(&lock_path).and_then(|file| {
            #[cfg(unix)]
            {
                let lock_type = match mode {
                    WorktreeReservationMode::Shared => libc::LOCK_SH,
                    WorktreeReservationMode::Exclusive => libc::LOCK_EX,
                };
                let operation = lock_type | if nonblocking { libc::LOCK_NB } else { 0 };
                let locked = unsafe { libc::flock(file.as_raw_fd(), operation) } == 0;
                if nonblocking {
                    anyhow::ensure!(
                        locked,
                        "another Temote operation owns this Git worktree reservation: {}",
                        identity.display()
                    );
                } else {
                    anyhow::ensure!(
                        locked,
                        "failed to acquire Git worktree reservation: {}",
                        identity.display()
                    );
                }
            }
            #[cfg(not(unix))]
            {
                let _ = mode;
                let _ = nonblocking;
                anyhow::bail!(
                    "Git worktree lifecycle reservations require a platform file-lock primitive"
                );
            }
            Ok(WorktreeReservation {
                file,
                identity,
                namespace: ReservationNamespace::Worktree,
            })
        }) {
            Ok(reservation) => reservations.push(reservation),
            Err(error) => return Err(error),
        }
    }
    Ok(WorktreeReservations { reservations })
}

#[cfg(test)]
pub(crate) fn acquire_worktree_reservations(paths: &[PathBuf]) -> Result<WorktreeReservations> {
    acquire_worktree_reservations_inner(paths, WorktreeReservationMode::Exclusive, false)
}

/// Attempts to acquire every exclusive reservation in stable order.  Any
/// partial acquisition is dropped on error so all previously acquired locks
/// are released before the failure is returned.
pub(crate) fn try_acquire_worktree_reservations(paths: &[PathBuf]) -> Result<WorktreeReservations> {
    acquire_worktree_reservations_inner(paths, WorktreeReservationMode::Exclusive, true)
}

pub(crate) async fn try_acquire_worktree_reservations_async(
    paths: &[PathBuf],
) -> Result<WorktreeReservations> {
    let paths = paths.to_vec();
    tokio::task::spawn_blocking(move || try_acquire_worktree_reservations(&paths))
        .await
        .context("exclusive worktree reservations worker failed")?
}

fn path_has_git_metadata(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path.join(".git")) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("cannot inspect Git metadata below {}", path.display())),
    }
}

fn worktree_root_for_admission(
    cwd: &Path,
    trusted_boundary: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let mut has_git_metadata = false;
    for ancestor in cwd.ancestors() {
        if path_has_git_metadata(ancestor)? {
            has_git_metadata = true;
            break;
        }
        if trusted_boundary.is_some_and(|boundary| ancestor == boundary) {
            break;
        }
    }
    if has_git_metadata {
        let worktree_root = crate::sandbox::git_worktree_root(cwd)?;
        // The lightweight root probe only finds a `.git` component. Resolve
        // the full metadata graph before treating it as a valid Git root so a
        // malformed ambient `.git` cannot downgrade admission to an unsafe
        // no-lock path.
        crate::sandbox::git_common_dir(cwd)?;
        Ok(Some(worktree_root))
    } else {
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AdmissionDirectoryIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    length: u64,
    #[cfg(not(unix))]
    readonly: bool,
}

fn admission_directory_identity(path: &Path) -> Result<AdmissionDirectoryIdentity> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect admitted worktree root {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "admitted worktree root must be a normal directory: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        Ok(AdmissionDirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(AdmissionDirectoryIdentity {
            length: metadata.len(),
            readonly: metadata.permissions().readonly(),
        })
    }
}

fn managed_target_and_repository_for_admission(
    cwd: &Path,
    trusted_src_root: Option<&Path>,
) -> Result<Option<(ManagedRepository, PathBuf)>> {
    let Some(trusted_src_root) = trusted_src_root else {
        return Ok(None);
    };
    let src_metadata = std::fs::symlink_metadata(trusted_src_root).with_context(|| {
        format!(
            "cannot inspect trusted configured src root {}",
            trusted_src_root.display()
        )
    })?;
    anyhow::ensure!(
        src_metadata.is_dir() && !src_metadata.file_type().is_symlink(),
        "trusted configured src root must be a normal directory: {}",
        trusted_src_root.display()
    );
    let canonical_src_root = std::fs::canonicalize(trusted_src_root).with_context(|| {
        format!(
            "cannot resolve trusted configured src root {}",
            trusted_src_root.display()
        )
    })?;
    anyhow::ensure!(
        canonical_src_root == trusted_src_root,
        "trusted configured src root changed: {}",
        trusted_src_root.display()
    );

    let managed_namespace = trusted_src_root.join(MANAGED_WORKTREE_ROOT_NAME);
    let namespace_metadata = match std::fs::symlink_metadata(&managed_namespace) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot inspect trusted managed worktree namespace {}",
                    managed_namespace.display()
                )
            });
        }
    };
    anyhow::ensure!(
        namespace_metadata.is_dir() && !namespace_metadata.file_type().is_symlink(),
        "trusted managed worktree namespace must be a normal directory: {}",
        managed_namespace.display()
    );
    anyhow::ensure!(
        std::fs::canonicalize(&managed_namespace).with_context(|| {
            format!(
                "cannot resolve trusted managed worktree namespace {}",
                managed_namespace.display()
            )
        })? == managed_namespace,
        "trusted managed worktree namespace changed: {}",
        managed_namespace.display()
    );

    let Some(relative) = cwd.strip_prefix(&managed_namespace).ok() else {
        return Ok(None);
    };
    let mut components = relative.components();
    let Some(repository_component) = components.next() else {
        return Ok(None);
    };
    let Some(task_component) = components.next() else {
        return Ok(None);
    };
    let repository_name = repository_component
        .as_os_str()
        .to_str()
        .context("managed repository component must be valid UTF-8")?;
    let task = task_component
        .as_os_str()
        .to_str()
        .context("managed task component must be valid UTF-8")?;
    let repository =
        ManagedRepository::resolve(&trusted_src_root.join(repository_name), trusted_src_root)?;
    ensure_requested_repository(repository_name, repository.repository_name())?;
    validate_task_name(task)?;
    let target = repository.target(task)?;
    anyhow::ensure!(
        cwd == target || cwd.starts_with(&target),
        "cwd is not contained by the trusted managed worktree target: {}",
        cwd.display()
    );
    let target_metadata = std::fs::symlink_metadata(&target).with_context(|| {
        format!(
            "cannot inspect trusted managed worktree target {}",
            target.display()
        )
    })?;
    anyhow::ensure!(
        target_metadata.is_dir() && !target_metadata.file_type().is_symlink(),
        "trusted managed worktree target must be a normal directory: {}",
        target.display()
    );
    let canonical_target = std::fs::canonicalize(&target).with_context(|| {
        format!(
            "cannot resolve trusted managed worktree target {}",
            target.display()
        )
    })?;
    anyhow::ensure!(
        canonical_target == target,
        "trusted managed worktree target changed: {}",
        target.display()
    );
    Ok(Some((repository, canonical_target)))
}

fn admission_identities(
    cwd: &Path,
    trusted_src_root: Option<&Path>,
) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    if let Some((repository, managed_target)) =
        managed_target_and_repository_for_admission(cwd, trusted_src_root)?
    {
        if path_has_git_metadata(&managed_target)? {
            let worktree_root = worktree_root_for_admission(cwd, trusted_src_root)?;
            anyhow::ensure!(
                worktree_root.as_deref() == Some(managed_target.as_path()),
                "Git worktree root does not match the trusted managed target: {}",
                managed_target.display()
            );
            let primary_checkout = crate::sandbox::git_primary_checkout(cwd)?;
            anyhow::ensure!(
                primary_checkout == repository.primary_checkout(),
                "Git repository does not match the trusted managed repository: {}",
                managed_target.display()
            );
            return Ok((worktree_root, Some(primary_checkout)));
        }
        // A managed target may be in the middle of cleanup and have lost its
        // `.git` metadata while its directory remains. The trusted shape is
        // sufficient to reserve its exact target root, without granting any
        // filesystem authority.
        return Ok((
            Some(managed_target),
            Some(repository.primary_checkout().to_path_buf()),
        ));
    }
    if let Some(worktree_root) = worktree_root_for_admission(cwd, trusted_src_root)? {
        let primary_checkout = crate::sandbox::git_primary_checkout(cwd)?;
        return Ok((Some(worktree_root), Some(primary_checkout)));
    }
    Ok((None, None))
}

/// Resolve a cwd, reserve its Git worktree when applicable, and revalidate
/// both identities while that reservation is held.  This is the common
/// admission primitive for sessions and spawned jobs; a caller must keep the
/// returned reservation until its ownership record is visible.
pub(crate) async fn acquire_worktree_admission(
    cwd: &Path,
    trusted_src_root: Option<&Path>,
) -> Result<WorktreeAdmission> {
    let cwd = crate::config::canonical_directory(cwd)
        .with_context(|| format!("cannot resolve worktree admission cwd {}", cwd.display()))?;
    let (initial_worktree_root, initial_repository_root) =
        admission_identities(&cwd, trusted_src_root)?;
    let initial_directory_identity = initial_worktree_root
        .as_deref()
        .map(admission_directory_identity)
        .transpose()?;
    // Acquire the repository gate before the per-worktree ownership lock so a
    // prune that owns the repository exclusively cannot observe an admission
    // half-way through its final observation.
    let repository_reservation = match &initial_repository_root {
        Some(repository_root) => Some(
            acquire_shared_repository_reservation_async(repository_root)
                .await
                .with_context(|| {
                    format!(
                        "cannot admit an operation for Git repository {}",
                        repository_root.display()
                    )
                })?,
        ),
        None => None,
    };
    let reservation = match &initial_worktree_root {
        Some(worktree_root) => Some(
            acquire_shared_worktree_reservation_async(worktree_root)
                .await
                .with_context(|| {
                    format!(
                        "cannot admit an operation for Git worktree {}",
                        worktree_root.display()
                    )
                })?,
        ),
        None => None,
    };

    let observed_cwd = crate::config::canonical_directory(&cwd)
        .with_context(|| format!("worktree admission cwd changed: {}", cwd.display()))?;
    anyhow::ensure!(
        observed_cwd == cwd,
        "worktree admission cwd changed: {}",
        cwd.display()
    );
    let (observed_worktree_root, observed_repository_root) =
        admission_identities(&observed_cwd, trusted_src_root)?;
    match (&initial_worktree_root, &observed_worktree_root) {
        (Some(expected), Some(observed)) => anyhow::ensure!(
            observed == expected,
            "Git worktree changed during admission: {}",
            cwd.display()
        ),
        (Some(_), None) => anyhow::bail!(
            "Git worktree disappeared during admission: {}",
            cwd.display()
        ),
        (None, Some(_)) => {
            anyhow::bail!("Git worktree appeared during admission: {}", cwd.display())
        }
        (None, None) => {}
    }
    anyhow::ensure!(
        observed_repository_root == initial_repository_root,
        "Git repository changed during admission: {}",
        cwd.display()
    );
    if let (Some(expected), Some(observed)) = (
        &initial_directory_identity,
        observed_worktree_root.as_deref(),
    ) {
        anyhow::ensure!(
            *expected == admission_directory_identity(observed)?,
            "admitted worktree root was removed or swapped during admission: {}",
            observed.display()
        );
    }

    Ok(WorktreeAdmission {
        cwd: observed_cwd,
        worktree_root: initial_worktree_root,
        repository_reservation,
        reservation,
    })
}

/// Classification for one registered Git worktree of the selected repository.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorktreeClassification {
    Primary,
    Managed,
    Legacy,
}

impl WorktreeClassification {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Managed => "managed",
            Self::Legacy => "legacy",
        }
    }
}

/// Workspace type of one Temote session, derived from its canonical working
/// directory with the same repository identity rules as the managed-worktree
/// broker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionWorkspaceType {
    /// The session workspace is the canonical primary checkout of a repository.
    CanonicalCheckout,
    /// The session workspace is an exact direct child of the trusted managed
    /// root of the selected repository.
    ManagedWorktree,
    /// Any other Git worktree: for example `<repository>/.wt/<name>`,
    /// `<src>/<repo>-*` or a linked worktree outside the managed root. Legacy
    /// worktrees are reported, never adopted, moved or deleted.
    LegacyWorktree,
}

impl SessionWorkspaceType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::CanonicalCheckout => "canonical_checkout",
            Self::ManagedWorktree => "managed_worktree",
            Self::LegacyWorktree => "legacy_worktree",
        }
    }
}

/// Bounded, non-secret session workspace identity.
///
/// Every field is derived from the canonical session working directory and the
/// configured `src` named root; no caller-supplied path and no `HOME` value
/// participates. `workspace_root` is always the canonical Git worktree root,
/// which may be an ancestor of the session working directory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionWorkspace {
    pub(crate) workspace_type: SessionWorkspaceType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) repository: Option<String>,
    pub(crate) repository_root: PathBuf,
    pub(crate) workspace_root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) task: Option<String>,
}

/// Derives the bounded session workspace identity for one canonical working
/// directory.
///
/// Returns `None` when the directory is not inside a supported standard Git
/// worktree. Managed classification requires the exact trusted managed root and
/// direct-child containment, so a swapped or symlinked managed root, a
/// subdirectory of the namespace, a nested path and every out-of-root worktree
/// resolve to `legacy_worktree` instead of being adopted.
pub(crate) fn inspect_session_workspace(
    cwd: &Path,
    src_root: Option<&Path>,
) -> Option<SessionWorkspace> {
    let workspace_root = crate::sandbox::git_worktree_root(cwd).ok()?;
    let repository_root = crate::sandbox::git_primary_checkout(&workspace_root).ok()?;
    let branch = crate::sandbox::git_current_branch(&workspace_root)
        .ok()
        .flatten();
    let repository = repository_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned);

    let managed = src_root
        .and_then(|src_root| ManagedRepository::resolve(&repository_root, src_root).ok())
        .filter(|repository| {
            trusted_canonical_managed_root(repository).as_deref() == Some(repository.managed_root())
                && workspace_root.parent() == Some(repository.managed_root())
        });
    let (workspace_type, task) = match managed {
        Some(_) => (
            SessionWorkspaceType::ManagedWorktree,
            workspace_root
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned),
        ),
        None if workspace_root == repository_root => {
            (SessionWorkspaceType::CanonicalCheckout, None)
        }
        None => (SessionWorkspaceType::LegacyWorktree, None),
    };

    Some(SessionWorkspace {
        workspace_type,
        repository,
        repository_root,
        workspace_root,
        branch,
        task,
    })
}

/// Resolves the configured `src` named root from `TEMOTE_MCP_ROOTS`.
///
/// The physical root always comes from the named-root authority, never from
/// `HOME` and never from a caller-supplied path.
pub(crate) fn configured_src_root_from_env() -> Option<PathBuf> {
    crate::named_roots::NamedRoots::from_env()
        .ok()?
        .canonical_root(MANAGED_SRC_ROOT_NAME)
        .map(Path::to_path_buf)
}

/// Canonical identity of the selected repository and its Temote-managed
/// worktree namespace.
///
/// The namespace is anchored to the configured `src` named root: the canonical
/// primary checkout must be exactly `<src-root>/<repo>` and the managed root is
/// always `<src-root>/worktrees/<repo>`. The checkout basename alone never
/// grants authority, and neither `HOME` nor a caller-supplied path is used.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedRepository {
    src_root: PathBuf,
    primary_checkout: PathBuf,
    repository_name: String,
    managed_root: PathBuf,
}

impl ManagedRepository {
    /// Resolves the managed namespace for one canonical primary checkout.
    ///
    /// Only the exact `<src-root>/<repo>` shape is accepted. Nested checkouts
    /// such as `<src-root>/group/repo`, checkouts below other named roots or
    /// outside any root (`/tmp/repo`), the reserved `worktrees` namespace and
    /// relative paths all fail closed. Filesystem authority (both directories
    /// exist, are not symlinks and are not swapped) is verified by
    /// [`Self::inspect_target_available`] and [`Self::prepare_target`], not by
    /// this pure path-shape resolution.
    pub(crate) fn resolve(primary_checkout: &Path, src_root: &Path) -> Result<Self> {
        anyhow::ensure!(
            src_root.is_absolute(),
            "configured {MANAGED_SRC_ROOT_NAME} root must be an absolute path: {}",
            src_root.display()
        );
        anyhow::ensure!(
            primary_checkout.is_absolute(),
            "canonical repository checkout must be an absolute path: {}",
            primary_checkout.display()
        );
        let repository_name = primary_checkout
            .file_name()
            .and_then(|name| name.to_str())
            .context("canonical repository checkout has no usable directory name")?
            .to_owned();
        anyhow::ensure!(
            repository_name != MANAGED_WORKTREE_ROOT_NAME,
            "canonical repository checkout must not use the reserved {MANAGED_WORKTREE_ROOT_NAME} name"
        );
        anyhow::ensure!(
            primary_checkout.parent() == Some(src_root),
            "canonical repository checkout must be exactly one directory below the configured \
             {MANAGED_SRC_ROOT_NAME} root: {} (src root {})",
            primary_checkout.display(),
            src_root.display()
        );
        let managed_root = src_root
            .join(MANAGED_WORKTREE_ROOT_NAME)
            .join(&repository_name);
        Ok(Self {
            src_root: src_root.to_path_buf(),
            primary_checkout: primary_checkout.to_path_buf(),
            repository_name,
            managed_root,
        })
    }

    pub(crate) fn primary_checkout(&self) -> &Path {
        &self.primary_checkout
    }

    pub(crate) fn repository_name(&self) -> &str {
        &self.repository_name
    }

    pub(crate) fn managed_root(&self) -> &Path {
        &self.managed_root
    }

    fn namespace_parent(&self) -> PathBuf {
        self.src_root.join(MANAGED_WORKTREE_ROOT_NAME)
    }

    /// Cross-checks an optional caller-supplied repository name against the
    /// canonical identity. The input never contributes path components.
    pub(crate) fn ensure_requested_repository(&self, requested: &str) -> Result<()> {
        ensure_requested_repository(requested, &self.repository_name)
    }

    pub(crate) fn target(&self, task: &str) -> Result<PathBuf> {
        validate_task_name(task)?;
        Ok(self.managed_root.join(task))
    }

    /// Read-only pre-approval inspection. Verifies repository authority, the
    /// target shape, the existing managed namespace and target collisions
    /// without creating, moving or deleting anything.
    pub(crate) fn inspect_target_available(&self, target: &Path) -> Result<()> {
        self.ensure_authority()?;
        self.ensure_target_shape(target)?;
        inspect_existing_normal_directory(&self.namespace_parent(), "managed worktree namespace")?;
        inspect_existing_normal_directory(&self.managed_root, "managed worktree root")?;
        self.ensure_target_absent(target)
    }

    /// Mutating preparation performed only after approval. Creates the managed
    /// namespace and re-verifies repository authority and target collision so a
    /// pre-approval inspection result is never trusted across the approval
    /// boundary.
    pub(crate) fn prepare_target(&self, target: &Path) -> Result<()> {
        self.ensure_authority()?;
        self.ensure_target_shape(target)?;
        create_normal_directory(&self.namespace_parent(), "managed worktree namespace")?;
        create_normal_directory(&self.managed_root, "managed worktree root")?;
        self.ensure_target_absent(target)
    }

    pub(crate) fn ensure_authority(&self) -> Result<()> {
        ensure_normal_directory(&self.src_root, "configured src root")?;
        ensure_normal_directory(&self.primary_checkout, "canonical repository checkout")
    }

    fn ensure_target_shape(&self, target: &Path) -> Result<()> {
        anyhow::ensure!(
            target.parent() == Some(self.managed_root.as_path())
                && target
                    .file_name()
                    .is_some_and(|name| name.to_str().is_some()),
            "managed worktree target must be a direct child of {}",
            self.managed_root.display()
        );
        Ok(())
    }

    fn ensure_target_absent(&self, target: &Path) -> Result<()> {
        match std::fs::symlink_metadata(target) {
            Ok(_) => anyhow::bail!(
                "managed worktree target already exists; choose a different task or reuse it explicitly: {}",
                target.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to inspect managed worktree target {}",
                    target.display()
                )
            }),
        }
    }
}

/// Cross-checks an optional caller-supplied repository name against the
/// canonical repository identity without granting it path authority.
pub(crate) fn ensure_requested_repository(requested: &str, canonical: &str) -> Result<()> {
    anyhow::ensure!(!requested.is_empty(), "repository must not be empty");
    anyhow::ensure!(
        requested.len() <= MAX_MANAGED_REPOSITORY_BYTES,
        "repository must be at most {MAX_MANAGED_REPOSITORY_BYTES} bytes"
    );
    anyhow::ensure!(
        !requested.chars().any(char::is_control),
        "repository must not contain control characters"
    );
    anyhow::ensure!(
        !requested.contains('/') && !requested.contains('\\'),
        "repository must not contain path separators"
    );
    anyhow::ensure!(
        requested == canonical,
        "requested repository {requested:?} does not match the selected canonical repository {canonical:?}"
    );
    Ok(())
}

/// Rejects every unsafe task directory input, including absolute paths,
/// traversal, separators, option-like values and control characters. The task
/// is a filesystem-safe single path component, never a path.
pub(crate) fn validate_task_name(task: &str) -> Result<()> {
    anyhow::ensure!(!task.is_empty(), "managed worktree task must not be empty");
    anyhow::ensure!(
        task.len() <= MAX_MANAGED_TASK_BYTES,
        "managed worktree task must be at most {MAX_MANAGED_TASK_BYTES} bytes"
    );
    anyhow::ensure!(
        !task.chars().any(char::is_control),
        "managed worktree task must not contain control characters"
    );
    anyhow::ensure!(
        task != "." && task != ".." && !task.contains(".."),
        "managed worktree task must not contain path traversal"
    );
    anyhow::ensure!(
        !task.contains('/') && !task.contains('\\'),
        "managed worktree task must be a single path component"
    );
    anyhow::ensure!(
        !task.starts_with('-'),
        "managed worktree task must not look like a command option"
    );
    anyhow::ensure!(
        task.chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric()),
        "managed worktree task must start with an ASCII letter or digit"
    );
    anyhow::ensure!(
        !task.ends_with('.'),
        "managed worktree task must not end with '.'"
    );
    anyhow::ensure!(
        task.chars()
            .all(|character| character.is_ascii_alphanumeric()
                || matches!(character, '-' | '_' | '.')),
        "managed worktree task must use only ASCII letters, digits, '.', '_' and '-'"
    );
    Ok(())
}

/// Derives a deterministic task directory from a validated branch name. A
/// branch `/` never becomes directory hierarchy; it is flattened to `-`.
pub(crate) fn derive_task_name(branch: &str) -> Result<String> {
    anyhow::ensure!(!branch.is_empty(), "branch must not be empty");
    let derived = branch.replace('/', "-");
    validate_task_name(&derived).with_context(|| {
        format!(
            "branch {branch:?} does not produce a safe managed task directory; pass task explicitly"
        )
    })?;
    Ok(derived)
}

/// Read-only authority check for an existing managed root.
///
/// Returns the canonical managed root only when it is a normal directory at the
/// exact derived path. A missing, symlinked or swapped root yields `None`, and
/// callers must not classify anything as managed.
pub(crate) fn trusted_canonical_managed_root(repository: &ManagedRepository) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(repository.managed_root()).ok()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return None;
    }
    let canonical = std::fs::canonicalize(repository.managed_root()).ok()?;
    (canonical == repository.managed_root()).then_some(canonical)
}

/// Verifies that an existing target may be reused as the selected repository's
/// managed worktree for `branch`.
///
/// Reuse never adopts legacy or unrelated state: the target must be a normal
/// canonical directory at the exact direct-child path below the trusted managed
/// root, and its canonical common Git directory, primary checkout and current
/// branch must equal the selected repository identity. Anything else fails
/// closed, so `<repository>/.wt/<name>`, `<src>/<repo>-*`, `/tmp` worktrees and
/// wrong-repository collisions are never adopted.
pub(crate) fn verify_reusable_managed_worktree(
    repository: &ManagedRepository,
    target: &Path,
    branch: &str,
    selected_common_dir: &Path,
    selected_primary_checkout: &Path,
) -> Result<()> {
    anyhow::ensure!(
        trusted_canonical_managed_root(repository).as_deref() == Some(repository.managed_root()),
        "managed worktree root is not a trusted normal directory: {}",
        repository.managed_root().display()
    );
    let metadata = std::fs::symlink_metadata(target)
        .with_context(|| format!("cannot inspect managed worktree {}", target.display()))?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "existing managed worktree target is not a normal directory: {}",
        target.display()
    );
    let canonical_target = std::fs::canonicalize(target)
        .with_context(|| format!("cannot resolve managed worktree {}", target.display()))?;
    anyhow::ensure!(
        canonical_target == target,
        "existing managed worktree target must be canonical and not a swapped path: {}",
        target.display()
    );
    anyhow::ensure!(
        target.parent() == Some(repository.managed_root()),
        "existing managed worktree target must be a direct child of {}",
        repository.managed_root().display()
    );
    anyhow::ensure!(
        crate::sandbox::git_common_dir(&canonical_target)? == selected_common_dir,
        "existing worktree does not belong to the selected repository (common Git directory mismatch): {}",
        target.display()
    );
    anyhow::ensure!(
        crate::sandbox::git_primary_checkout(&canonical_target)? == selected_primary_checkout,
        "existing worktree primary checkout mismatch: {}",
        target.display()
    );
    anyhow::ensure!(
        crate::sandbox::git_current_branch(&canonical_target)?.as_deref() == Some(branch),
        "existing managed worktree is not attached to the requested branch {branch:?}: {}",
        target.display()
    );
    Ok(())
}

/// Facts observed after the Git worktree mutation. Plain values keep the
/// verification predicate testable without a repository fixture.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CreatedTargetObservation {
    pub(crate) canonical_target: Option<PathBuf>,
    pub(crate) canonical_managed_root: Option<PathBuf>,
    pub(crate) managed_root_is_normal_directory: bool,
    pub(crate) target_is_symlink: bool,
    pub(crate) observed_common_dir: Option<PathBuf>,
    pub(crate) observed_primary_checkout: Option<PathBuf>,
}

/// Post-create verification. Git success alone is never enough: the created
/// target must resolve as a direct child of the exact trusted managed root with
/// the expected task component, and it must belong to the selected repository
/// identity (common Git directory plus primary checkout).
pub(crate) fn verify_created_managed_target(
    repository: &ManagedRepository,
    task: &str,
    selected_common_dir: &Path,
    selected_primary_checkout: &Path,
    observation: &CreatedTargetObservation,
) -> Result<()> {
    validate_task_name(task)?;
    anyhow::ensure!(
        observation.managed_root_is_normal_directory,
        "managed worktree root is not a normal directory after creation: {}",
        repository.managed_root().display()
    );
    let canonical_managed_root = observation
        .canonical_managed_root
        .as_deref()
        .context("cannot resolve the managed worktree root after creation")?;
    anyhow::ensure!(
        canonical_managed_root == repository.managed_root(),
        "managed worktree root does not match the trusted managed root after creation: {}",
        canonical_managed_root.display()
    );
    anyhow::ensure!(
        !observation.target_is_symlink,
        "created managed worktree target must not be a symbolic link: {}",
        repository.managed_root().join(task).display()
    );
    let canonical_target = observation
        .canonical_target
        .as_deref()
        .context("cannot resolve the created managed worktree target")?;
    anyhow::ensure!(
        canonical_target.parent() == Some(canonical_managed_root),
        "created managed worktree is not a direct child of the trusted managed root: {}",
        canonical_target.display()
    );
    anyhow::ensure!(
        canonical_target.file_name() == Some(std::ffi::OsStr::new(task)),
        "created managed worktree component does not match task {task:?}: {}",
        canonical_target.display()
    );
    anyhow::ensure!(
        observation.observed_common_dir.as_deref() == Some(selected_common_dir),
        "created managed worktree does not belong to the selected repository (common Git directory mismatch): {}",
        canonical_target.display()
    );
    anyhow::ensure!(
        observation.observed_primary_checkout.as_deref() == Some(selected_primary_checkout),
        "created managed worktree primary checkout mismatch: {}",
        canonical_target.display()
    );
    Ok(())
}

/// Identity facts for one registered worktree used by list classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegisteredWorktreeIdentity<'a> {
    pub(crate) canonical_path: Option<&'a Path>,
    pub(crate) common_dir: Option<&'a Path>,
    pub(crate) primary_checkout: Option<&'a Path>,
}

/// Classifies one registered worktree using direct-child containment of the
/// canonical managed root plus the canonical common Git directory and primary
/// checkout as repository identity. The managed root itself and nested
/// descendants are never managed. Anything that cannot be verified fails closed
/// as legacy. A `None` managed root means the managed root authority is
/// unavailable, so nothing is managed.
pub(crate) fn classify_registered_worktree(
    registered: RegisteredWorktreeIdentity<'_>,
    selected_primary_checkout: &Path,
    canonical_managed_root: Option<&Path>,
    selected_common_dir: &Path,
) -> WorktreeClassification {
    let Some(canonical_path) = registered.canonical_path else {
        return WorktreeClassification::Legacy;
    };
    if canonical_path == selected_primary_checkout {
        return WorktreeClassification::Primary;
    }
    let (Some(canonical_managed_root), Some(common_dir), Some(primary_checkout)) = (
        canonical_managed_root,
        registered.common_dir,
        registered.primary_checkout,
    ) else {
        return WorktreeClassification::Legacy;
    };
    if common_dir != selected_common_dir || primary_checkout != selected_primary_checkout {
        return WorktreeClassification::Legacy;
    }
    if canonical_path.parent() != Some(canonical_managed_root) {
        return WorktreeClassification::Legacy;
    }
    WorktreeClassification::Managed
}

/// One entry of `git worktree list --porcelain`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegisteredWorktree {
    pub(crate) path: PathBuf,
    pub(crate) head: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) bare: bool,
    pub(crate) detached: bool,
    pub(crate) prunable: bool,
}

pub(crate) fn parse_worktree_list(porcelain: &str) -> Result<Vec<RegisteredWorktree>> {
    let mut entries = Vec::new();
    let mut current: Option<RegisteredWorktree> = None;
    for line in porcelain.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            anyhow::ensure!(
                !path.starts_with('"'),
                "quoted Git worktree paths are not supported"
            );
            anyhow::ensure!(
                !path.is_empty() && Path::new(path).is_absolute(),
                "Git worktree listing contains an invalid path: {path:?}"
            );
            current = Some(RegisteredWorktree {
                path: PathBuf::from(path),
                head: None,
                branch: None,
                bare: false,
                detached: false,
                prunable: false,
            });
            continue;
        }
        let entry = current
            .as_mut()
            .context("Git worktree listing field appeared before any worktree entry")?;
        if let Some(head) = line.strip_prefix("HEAD ") {
            entry.head = Some(head.to_owned());
        } else if let Some(branch) = line.strip_prefix("branch ") {
            entry.branch = Some(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_owned(),
            );
        } else if line == "bare" {
            entry.bare = true;
        } else if line == "detached" {
            entry.detached = true;
        } else if line == "locked" || line.starts_with("locked ") {
        } else if line == "prunable" || line.starts_with("prunable ") {
            entry.prunable = true;
        } else {
            anyhow::bail!("unsupported Git worktree listing line: {line:?}");
        }
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    Ok(entries)
}

fn inspect_existing_normal_directory(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => ensure_normal_directory_metadata(path, label, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect {label} {}", path.display()))
        }
    }
}

fn ensure_normal_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {label} {}", path.display()))?;
    ensure_normal_directory_metadata(path, label, &metadata)
}

fn ensure_normal_directory_metadata(
    path: &Path,
    label: &str,
    metadata: &std::fs::Metadata,
) -> Result<()> {
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} must be a normal directory: {}",
        path.display()
    );
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {label} {}", path.display()))?;
    anyhow::ensure!(
        canonical == path,
        "{label} must not be a symbolic link or swapped path: {}",
        path.display()
    );
    Ok(())
}

fn create_normal_directory(path: &Path, label: &str) -> Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to create {label} {}", path.display()));
        }
    }
    ensure_normal_directory(path, label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    #[cfg(unix)]
    #[test]
    fn worktree_reservation_is_exclusive_and_releases_on_drop() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("repo").join("worktree");
        std::fs::create_dir_all(&target).unwrap();
        let target = std::fs::canonicalize(target).unwrap();

        let held = acquire_worktree_reservation(&target).unwrap();
        assert_eq!(held.identity(), target);
        assert!(try_acquire_worktree_reservation(&target).is_err());

        // Per-worktree reservations do not serialize unrelated repositories.
        let unrelated = fixture.path().join("other");
        std::fs::create_dir(&unrelated).unwrap();
        let unrelated_guard = try_acquire_worktree_reservation(&unrelated).unwrap();
        drop(unrelated_guard);

        drop(held);
        let released = try_acquire_worktree_reservation(&target).unwrap();
        drop(released);
    }

    #[cfg(unix)]
    #[test]
    fn stale_prune_entries_use_the_same_reservation_identity() {
        let fixture = tempfile::tempdir().unwrap();
        let stale = fixture.path().join("repo").join("removed-worktree");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();

        let held = acquire_worktree_reservation(&stale).unwrap();
        assert!(try_acquire_worktree_reservation(&stale).is_err());
        drop(held);
        let released = try_acquire_worktree_reservation(&stale).unwrap();
        drop(released);
    }

    #[cfg(unix)]
    #[test]
    fn multi_worktree_reservation_is_sorted_and_deduplicated() {
        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("repo").join("first");
        let second = fixture.path().join("repo").join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        let reservations =
            acquire_worktree_reservations(&[second.clone(), first.clone(), second.clone()])
                .unwrap();
        let identities = reservations
            .identities()
            .into_iter()
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        assert_eq!(
            identities,
            vec![
                std::fs::canonicalize(&first).unwrap(),
                std::fs::canonicalize(&second).unwrap(),
            ]
        );
        drop(reservations);
        assert!(try_acquire_worktree_reservation(&first).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn failed_multi_reservation_releases_partial_acquisition() {
        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("repo").join("first");
        let second = fixture.path().join("repo").join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        let shared_second = acquire_shared_worktree_reservation(&second).unwrap();
        assert!(
            try_acquire_worktree_reservations(&[first.clone(), second.clone()]).is_err(),
            "exclusive multi-reservation must fail on an active shared member"
        );
        drop(shared_second);

        let first_cleanup = try_acquire_worktree_reservation(&first).unwrap();
        drop(first_cleanup);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_reservation_identities_do_not_collide() {
        let fixture = tempfile::tempdir().unwrap();
        let first = fixture
            .path()
            .join(std::ffi::OsString::from_vec(b"work-\x80".to_vec()));
        let second = fixture
            .path()
            .join(std::ffi::OsString::from_vec(b"work-\x81".to_vec()));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        let first_guard = acquire_worktree_reservation(&first).unwrap();
        let second_guard = try_acquire_worktree_reservation(&second).unwrap();
        drop(second_guard);
        drop(first_guard);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn worktree_admission_waits_for_target_and_allows_unrelated_worktrees() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("target-repo");
        let unrelated = fixture.path().join("unrelated-repo");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();
        run_git_fixture(&target, &["init", "--quiet"]);
        run_git_fixture(&unrelated, &["init", "--quiet"]);
        let target = std::fs::canonicalize(target).unwrap();
        let unrelated = std::fs::canonicalize(unrelated).unwrap();
        let target_cwd = target.join("nested");
        std::fs::create_dir(&target_cwd).unwrap();

        let held = acquire_worktree_reservation(&target).unwrap();
        let mut target_admission = Box::pin(acquire_worktree_admission(&target_cwd, None));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut target_admission)
                .await
                .is_err(),
            "same-worktree admission must wait for the lifecycle reservation"
        );

        let unrelated_admission = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            acquire_worktree_admission(&unrelated, None),
        )
        .await
        .expect("unrelated worktree admission must not be serialized")
        .unwrap();
        assert_eq!(
            unrelated_admission.worktree_root.as_deref(),
            Some(unrelated.as_path())
        );
        drop(unrelated_admission);

        drop(held);
        let target_admission =
            tokio::time::timeout(std::time::Duration::from_secs(2), &mut target_admission)
                .await
                .expect("target admission must complete after reservation release")
                .unwrap();
        assert_eq!(target_admission.cwd, target_cwd);
        assert_eq!(
            target_admission.worktree_root.as_deref(),
            Some(target.as_path())
        );
        drop(target_admission);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn shared_admissions_coexist_and_exclusive_cleanup_fails_closed() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("repo");
        std::fs::create_dir_all(&target).unwrap();
        run_git_fixture(&target, &["init", "--quiet"]);
        let target = std::fs::canonicalize(target).unwrap();

        let first = acquire_worktree_admission(&target, None).await.unwrap();
        let second = acquire_worktree_admission(&target, None).await.unwrap();
        assert_eq!(first.worktree_root.as_deref(), Some(target.as_path()));
        assert_eq!(second.worktree_root.as_deref(), Some(target.as_path()));
        assert!(
            try_acquire_worktree_reservation(&target).is_err(),
            "exclusive cleanup must fail while shared admissions are active"
        );

        drop(first);
        assert!(
            try_acquire_worktree_reservation(&target).is_err(),
            "the remaining shared admission must keep cleanup excluded"
        );
        drop(second);

        let cleanup = try_acquire_worktree_reservation(&target).unwrap();
        drop(cleanup);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn repository_exclusive_gate_blocks_admission_even_without_prunable_targets() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = fixture.path().join("repo");
        std::fs::create_dir_all(&repository).unwrap();
        run_git_fixture(&repository, &["init", "--quiet"]);
        let repository = std::fs::canonicalize(repository).unwrap();

        let gate = try_acquire_repository_reservation_async(&repository)
            .await
            .unwrap();
        let mut admission = Box::pin(acquire_worktree_admission(&repository, None));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut admission)
                .await
                .is_err(),
            "a prune repository gate must block admission even when no target lock exists"
        );

        drop(gate);
        let admission = tokio::time::timeout(std::time::Duration::from_secs(2), &mut admission)
            .await
            .expect("admission must complete after repository gate release")
            .unwrap();
        drop(admission);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn repository_gate_shared_remove_conflicts_with_exclusive_prune() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = fixture.path().join("repo");
        std::fs::create_dir_all(&repository).unwrap();
        run_git_fixture(&repository, &["init", "--quiet"]);
        let repository = std::fs::canonicalize(repository).unwrap();

        let remove_gate = acquire_shared_repository_reservation_async(&repository)
            .await
            .unwrap();
        assert!(
            try_acquire_repository_reservation_async(&repository)
                .await
                .is_err(),
            "prune must fail closed while remove holds the shared repository gate"
        );
        drop(remove_gate);

        let prune_gate = try_acquire_repository_reservation_async(&repository)
            .await
            .unwrap();
        drop(prune_gate);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn released_admission_repository_gate_allows_prune_with_active_target_owner() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = fixture.path().join("repo");
        std::fs::create_dir_all(&repository).unwrap();
        run_git_fixture(&repository, &["init", "--quiet"]);
        let repository = std::fs::canonicalize(repository).unwrap();

        let admission = acquire_worktree_admission(&repository, None).await.unwrap();
        let WorktreeAdmission {
            repository_reservation,
            reservation,
            ..
        } = admission;
        drop(repository_reservation);

        let prune_gate = try_acquire_repository_reservation_async(&repository)
            .await
            .expect("active target ownership must not retain the short repository gate");
        drop(prune_gate);
        drop(reservation);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn shared_remove_repository_gate_allows_unrelated_same_repo_admission() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = fixture.path().join("repo");
        std::fs::create_dir_all(&repository).unwrap();
        run_git_fixture(&repository, &["init", "--quiet"]);
        let sibling = fixture.path().join("sibling");
        run_git_fixture(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                sibling.to_str().unwrap(),
                "-b",
                "sibling",
            ],
        );
        let repository = std::fs::canonicalize(repository).unwrap();
        let sibling = std::fs::canonicalize(sibling).unwrap();

        let remove_gate = acquire_shared_repository_reservation_async(&repository)
            .await
            .unwrap();
        let sibling_admission = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            acquire_worktree_admission(&sibling, None),
        )
        .await
        .expect("same-repository shared admission must not wait for remove")
        .unwrap();
        drop(sibling_admission);
        drop(remove_gate);
    }

    #[cfg(unix)]
    #[test]
    fn failed_repository_gate_scope_releases_exclusive_gate() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = fixture.path().join("repo");
        std::fs::create_dir_all(&repository).unwrap();
        run_git_fixture(&repository, &["init", "--quiet"]);
        let repository = std::fs::canonicalize(repository).unwrap();

        let result: Result<()> = (|| {
            let _gate = acquire_repository_reservation_inner(
                &repository,
                WorktreeReservationMode::Exclusive,
                true,
            )?;
            anyhow::bail!("injected prune failure after repository gate acquisition")
        })();
        assert!(result.is_err());

        let released = acquire_repository_reservation_inner(
            &repository,
            WorktreeReservationMode::Exclusive,
            true,
        )
        .unwrap();
        drop(released);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn missing_git_managed_target_admission_waits_and_fails_after_remove_or_swap() {
        let fixture = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(fixture.path()).unwrap();
        let managed_root = src_root.join("worktrees/repo");
        std::fs::create_dir_all(&managed_root).unwrap();

        let swap_target = managed_root.join("swap-task");
        let swap_cwd = swap_target.join("nested");
        std::fs::create_dir_all(&swap_cwd).unwrap();
        let held = acquire_worktree_reservation(&swap_target).unwrap();
        let mut swap_admission = Box::pin(acquire_worktree_admission(&swap_cwd, Some(&src_root)));
        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut swap_admission).await;
        assert!(
            blocked.is_err(),
            "missing-git managed target must still reserve its derived target root: {blocked:?}"
        );

        let unrelated = src_root.join("ordinary").join("nested");
        std::fs::create_dir_all(&unrelated).unwrap();
        let unrelated_admission = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            acquire_worktree_admission(&unrelated, Some(&src_root)),
        )
        .await
        .expect("unrelated non-managed directory must remain admissible")
        .unwrap();
        assert!(unrelated_admission.worktree_root.is_none());
        drop(unrelated_admission);

        let moved = managed_root.join("moved-task");
        std::fs::rename(&swap_target, &moved).unwrap();
        std::fs::create_dir_all(&swap_cwd).unwrap();
        drop(held);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), &mut swap_admission)
                .await
                .expect("swapped target admission must finish")
                .is_err(),
            "admission must fail closed after the managed target is swapped"
        );

        let remove_target = managed_root.join("remove-task");
        let remove_cwd = remove_target.join("nested");
        std::fs::create_dir_all(&remove_cwd).unwrap();
        let held = acquire_worktree_reservation(&remove_target).unwrap();
        let mut remove_admission =
            Box::pin(acquire_worktree_admission(&remove_cwd, Some(&src_root)));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut remove_admission)
                .await
                .is_err()
        );
        std::fs::remove_dir_all(&remove_target).unwrap();
        drop(held);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), &mut remove_admission)
                .await
                .expect("removed target admission must finish")
                .is_err(),
            "admission must fail closed after the managed target is removed"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn prune_reservations_block_each_member_admission_until_release() {
        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("first-repo");
        let second = fixture.path().join("second-repo");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        run_git_fixture(&first, &["init", "--quiet"]);
        run_git_fixture(&second, &["init", "--quiet"]);
        let first = std::fs::canonicalize(first).unwrap();
        let second = std::fs::canonicalize(second).unwrap();

        // This models the stable, deduplicated lock set acquired for one
        // bounded prune observation.
        let reservations =
            acquire_worktree_reservations(&[second.clone(), first.clone(), second.clone()])
                .unwrap();
        let mut first_admission = Box::pin(acquire_worktree_admission(&first, None));
        let mut second_admission = Box::pin(acquire_worktree_admission(&second, None));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut first_admission)
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut second_admission)
                .await
                .is_err()
        );

        drop(reservations);
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), &mut first_admission)
            .await
            .expect("first admission must complete after prune release")
            .unwrap();
        let second = tokio::time::timeout(std::time::Duration::from_secs(2), &mut second_admission)
            .await
            .expect("second admission must complete after prune release")
            .unwrap();
        drop(first);
        drop(second);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn cleanup_reservation_releases_after_injected_failure() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("repo");
        std::fs::create_dir(&target).unwrap();
        run_git_fixture(&target, &["init", "--quiet"]);
        let target = std::fs::canonicalize(target).unwrap();

        let result: Result<()> = (|| {
            let _reservation = acquire_worktree_reservation(&target)?;
            anyhow::bail!("injected cleanup failure before mutation or verification")
        })();
        assert!(result.is_err());
        let admission = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            acquire_worktree_admission(&target, None),
        )
        .await
        .expect("cleanup failure must release the lifecycle reservation")
        .unwrap();
        assert_eq!(admission.worktree_root.as_deref(), Some(target.as_path()));
        drop(admission);
    }

    fn src_root() -> PathBuf {
        PathBuf::from("/home/user/src")
    }

    fn primary_checkout(name: &str) -> PathBuf {
        src_root().join(name)
    }

    fn resolve(name: &str) -> Result<ManagedRepository> {
        ManagedRepository::resolve(&primary_checkout(name), &src_root())
    }

    #[test]
    fn managed_repository_root_is_derived_from_the_exact_src_root_child() {
        let repository = resolve("local-mcp").unwrap();
        assert_eq!(repository.repository_name(), "local-mcp");
        assert_eq!(repository.primary_checkout(), primary_checkout("local-mcp"));
        assert_eq!(
            repository.managed_root(),
            PathBuf::from("/home/user/src/worktrees/local-mcp")
        );
        assert_eq!(
            repository.target("issue-123-worktree-broker").unwrap(),
            PathBuf::from("/home/user/src/worktrees/local-mcp/issue-123-worktree-broker")
        );
    }

    #[test]
    fn exact_src_root_shape_fails_closed_for_every_other_layout() {
        let root = PathBuf::from("/home/user/src");
        for (checkout, src) in [
            (PathBuf::from("/home/user/src/nested/repo"), root.clone()),
            (PathBuf::from("/tmp/repo"), root.clone()),
            (PathBuf::from("/home/user/work/repo"), root.clone()),
            (PathBuf::from("/home/user/src/worktrees/repo"), root.clone()),
            (PathBuf::from("/home/user/src/worktrees"), root.clone()),
            (
                PathBuf::from("/home/user/src/repo"),
                PathBuf::from("/home/user/work"),
            ),
            (PathBuf::from("relative/repo"), root.clone()),
            (
                PathBuf::from("/home/user/src/repo"),
                PathBuf::from("relative/src"),
            ),
        ] {
            assert!(
                ManagedRepository::resolve(&checkout, &src).is_err(),
                "{checkout:?} under {src:?}"
            );
        }
    }

    #[test]
    fn src_root_and_checkout_must_be_normal_directories() {
        let fixture = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let checkout = root.join("repo");
        std::fs::create_dir(&checkout).unwrap();
        let repository = ManagedRepository::resolve(&checkout, &root).unwrap();
        let target = repository.target("task").unwrap();
        repository.inspect_target_available(&target).unwrap();
        assert!(repository.prepare_target(&target).is_ok());

        let missing = ManagedRepository::resolve(&root.join("missing"), &root).unwrap();
        assert!(
            missing
                .inspect_target_available(&missing.target("task").unwrap())
                .is_err()
        );

        #[cfg(unix)]
        {
            let outside = root.join("outside");
            std::fs::create_dir(&outside).unwrap();
            let link = root.join("linked");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            let linked = ManagedRepository::resolve(&link, &root).unwrap();
            assert!(
                linked
                    .inspect_target_available(&linked.target("task").unwrap())
                    .is_err()
            );
            assert!(
                linked
                    .prepare_target(&linked.target("task").unwrap())
                    .is_err()
            );
            assert!(!outside.join("worktrees").exists());
        }
    }

    #[test]
    fn requested_repository_must_match_the_canonical_identity() {
        let repository = resolve("local-mcp").unwrap();
        repository.ensure_requested_repository("local-mcp").unwrap();
        for requested in ["", "other-repo", "../local-mcp", "local/mcp"] {
            assert!(
                repository.ensure_requested_repository(requested).is_err(),
                "{requested:?}"
            );
        }
    }

    #[test]
    fn task_validation_rejects_injection_and_unsafe_names() {
        for task in ["feature", "task-123", "app1274-detail-sort", "r1.2", "a_b"] {
            validate_task_name(task).unwrap_or_else(|error| panic!("{task:?}: {error}"));
        }
        for task in [
            "",
            ".",
            "..",
            "a..b",
            "/tmp/x",
            "../x",
            "a/../../b",
            "nested/bad",
            "back\\slash",
            "-option",
            "--force",
            ".hidden",
            "trailing.",
            "bad\nname",
            "unicode-\u{65e5}\u{672c}",
        ] {
            assert!(validate_task_name(task).is_err(), "{task:?}");
        }
        let oversized = "a".repeat(MAX_MANAGED_TASK_BYTES + 1);
        assert!(validate_task_name(&oversized).is_err());
        validate_task_name(&"a".repeat(MAX_MANAGED_TASK_BYTES)).unwrap();
    }

    #[test]
    fn branch_slashes_never_create_directory_hierarchy() {
        assert_eq!(
            derive_task_name("feature/foo/bar").unwrap(),
            "feature-foo-bar"
        );
        assert_eq!(
            derive_task_name("feat/20260916-worktree-broker").unwrap(),
            "feat-20260916-worktree-broker"
        );
        for branch in ["feature/../escape", "-option", "release/v1.0.0-rc.1"] {
            let derived = derive_task_name(branch);
            if let Ok(task) = derived {
                assert!(!task.contains('/') && !task.contains('\\'));
                assert!(validate_task_name(&task).is_ok());
            }
        }
    }

    #[test]
    fn pre_approval_inspection_is_read_only_and_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("repo");
        std::fs::create_dir(&checkout).unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let repository =
            ManagedRepository::resolve(&std::fs::canonicalize(&checkout).unwrap(), &canonical)
                .unwrap();

        let target = repository.target("task-one").unwrap();
        repository.inspect_target_available(&target).unwrap();
        assert!(!repository.namespace_parent().exists());
        assert!(!repository.managed_root().exists());
        assert!(!target.exists());

        std::fs::create_dir_all(&target).unwrap();
        repository.inspect_target_available(&target).unwrap_err();

        let file_target = repository.target("task-file").unwrap();
        std::fs::write(&file_target, b"unrelated").unwrap();
        let error = repository
            .inspect_target_available(&file_target)
            .unwrap_err();
        assert!(error.to_string().contains("already exists"));
        std::fs::remove_file(&file_target).unwrap();

        let escaped = target.parent().unwrap().join("nested").join("task");
        assert!(repository.inspect_target_available(&escaped).is_err());
    }

    #[test]
    fn post_approval_preparation_creates_the_namespace_and_rechecks_collisions() {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("repo");
        std::fs::create_dir(&checkout).unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let repository =
            ManagedRepository::resolve(&std::fs::canonicalize(&checkout).unwrap(), &canonical)
                .unwrap();

        let target = repository.target("task-one").unwrap();
        repository.prepare_target(&target).unwrap();
        assert!(repository.namespace_parent().is_dir());
        assert!(repository.managed_root().is_dir());
        assert!(!target.exists());

        // A target that appears after the pre-approval inspection is rejected.
        std::fs::create_dir(&target).unwrap();
        assert!(repository.prepare_target(&target).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_managed_parents_fail_closed_before_and_after_approval() {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("repo");
        std::fs::create_dir(&checkout).unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let repository =
            ManagedRepository::resolve(&std::fs::canonicalize(&checkout).unwrap(), &canonical)
                .unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();

        std::os::unix::fs::symlink(&outside, repository.namespace_parent()).unwrap();
        let target = repository.target("task").unwrap();
        let error = repository.inspect_target_available(&target).unwrap_err();
        assert!(error.to_string().contains("normal directory"), "{error:#}");
        assert!(repository.prepare_target(&target).is_err());
        assert!(!outside.join("repo").exists());

        std::fs::remove_file(repository.namespace_parent()).unwrap();
        std::fs::create_dir(repository.namespace_parent()).unwrap();
        std::os::unix::fs::symlink(&outside, repository.managed_root()).unwrap();
        let error = repository.inspect_target_available(&target).unwrap_err();
        assert!(error.to_string().contains("normal directory"), "{error:#}");
        assert!(repository.prepare_target(&target).is_err());
        assert!(!outside.join("task").exists());
        assert!(!outside.join("repo").exists());
    }

    #[test]
    fn trusted_managed_root_requires_a_normal_directory_at_the_exact_path() {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("repo");
        std::fs::create_dir(&checkout).unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let repository =
            ManagedRepository::resolve(&std::fs::canonicalize(&checkout).unwrap(), &canonical)
                .unwrap();
        assert_eq!(trusted_canonical_managed_root(&repository), None);

        std::fs::create_dir_all(repository.managed_root()).unwrap();
        assert_eq!(
            trusted_canonical_managed_root(&repository),
            Some(repository.managed_root().to_path_buf())
        );

        #[cfg(unix)]
        {
            std::fs::remove_dir(repository.managed_root()).unwrap();
            let outside = root.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, repository.managed_root()).unwrap();
            assert_eq!(trusted_canonical_managed_root(&repository), None);
        }
    }

    #[test]
    fn created_target_verification_requires_exact_containment_and_identity() {
        let repository = resolve("repo").unwrap();
        let selected_common = PathBuf::from("/home/user/src/repo/.git");
        let selected_primary = primary_checkout("repo");
        let observation = CreatedTargetObservation {
            canonical_target: Some(PathBuf::from("/home/user/src/worktrees/repo/task")),
            canonical_managed_root: Some(PathBuf::from("/home/user/src/worktrees/repo")),
            managed_root_is_normal_directory: true,
            target_is_symlink: false,
            observed_common_dir: Some(selected_common.clone()),
            observed_primary_checkout: Some(selected_primary.clone()),
        };
        verify_created_managed_target(
            &repository,
            "task",
            &selected_common,
            &selected_primary,
            &observation,
        )
        .unwrap();

        fn symlinked_managed_root(observation: &mut CreatedTargetObservation) {
            observation.managed_root_is_normal_directory = false;
        }
        fn swapped_managed_root(observation: &mut CreatedTargetObservation) {
            observation.canonical_managed_root =
                Some(PathBuf::from("/home/user/src/worktrees/other"));
        }
        fn symlinked_target(observation: &mut CreatedTargetObservation) {
            observation.target_is_symlink = true;
        }
        fn nested_target(observation: &mut CreatedTargetObservation) {
            observation.canonical_target =
                Some(PathBuf::from("/home/user/src/worktrees/repo/nested/task"));
        }
        fn task_mismatch(observation: &mut CreatedTargetObservation) {
            observation.canonical_target =
                Some(PathBuf::from("/home/user/src/worktrees/repo/other-task"));
        }
        fn missing_target(observation: &mut CreatedTargetObservation) {
            observation.canonical_target = None;
        }
        fn wrong_common_dir(observation: &mut CreatedTargetObservation) {
            observation.observed_common_dir = Some(PathBuf::from("/home/user/src/other/.git"));
        }
        fn wrong_primary_checkout(observation: &mut CreatedTargetObservation) {
            observation.observed_primary_checkout = Some(PathBuf::from("/home/user/src/other"));
        }
        type ObservationMutation = fn(&mut CreatedTargetObservation);
        let mutations: [(&str, ObservationMutation); 8] = [
            ("symlinked managed root", symlinked_managed_root),
            ("swapped managed root", swapped_managed_root),
            ("symlinked target", symlinked_target),
            ("nested target", nested_target),
            ("task mismatch", task_mismatch),
            ("missing target", missing_target),
            ("wrong common dir", wrong_common_dir),
            ("wrong primary checkout", wrong_primary_checkout),
        ];
        for (label, mutate) in mutations {
            let mut mutated = observation.clone();
            mutate(&mut mutated);
            assert!(
                verify_created_managed_target(
                    &repository,
                    "task",
                    &selected_common,
                    &selected_primary,
                    &mutated,
                )
                .is_err(),
                "{label}"
            );
        }
    }

    #[test]
    fn classification_requires_canonical_containment_and_identity() {
        let primary = PathBuf::from("/src/repo");
        let managed_root = PathBuf::from("/src/worktrees/repo");
        let identity = PathBuf::from("/src/repo/.git");
        fn identity_for<'a>(
            path: &'a Path,
            common: Option<&'a Path>,
            primary: &'a Path,
        ) -> RegisteredWorktreeIdentity<'a> {
            RegisteredWorktreeIdentity {
                canonical_path: Some(path),
                common_dir: common,
                primary_checkout: Some(primary),
            }
        }

        assert_eq!(
            classify_registered_worktree(
                identity_for(&primary, Some(&identity), &primary),
                &primary,
                Some(&managed_root),
                &identity,
            ),
            WorktreeClassification::Primary
        );
        assert_eq!(
            classify_registered_worktree(
                identity_for(
                    Path::new("/src/worktrees/repo/task"),
                    Some(&identity),
                    &primary
                ),
                &primary,
                Some(&managed_root),
                &identity,
            ),
            WorktreeClassification::Managed
        );
        let managed_path = Path::new("/src/worktrees/repo/task");
        for (path, common) in [
            (
                Some(Path::new("/src/repo/.wt/legacy")),
                Some(identity.as_path()),
            ),
            (Some(Path::new("/tmp/legacy")), Some(identity.as_path())),
            (Some(managed_path), Some(Path::new("/other/repo/.git"))),
            (
                Some(Path::new("/src/worktrees/other/task")),
                Some(identity.as_path()),
            ),
            (Some(Path::new("/src/worktrees/repo/missing")), None),
            (None, None),
            // Direct-child containment is the contract: the managed root
            // itself and nested descendants are legacy even with a matching
            // common Git directory and primary checkout.
            (
                Some(Path::new("/src/worktrees/repo")),
                Some(identity.as_path()),
            ),
            (
                Some(Path::new("/src/worktrees/repo/task/nested")),
                Some(identity.as_path()),
            ),
        ] {
            assert_eq!(
                classify_registered_worktree(
                    RegisteredWorktreeIdentity {
                        canonical_path: path,
                        common_dir: common,
                        primary_checkout: Some(&primary),
                    },
                    &primary,
                    Some(&managed_root),
                    &identity,
                ),
                WorktreeClassification::Legacy,
                "{path:?} {common:?}"
            );
        }
        // A matching common directory with a different primary checkout is a
        // different repository identity and must not be adopted.
        assert_eq!(
            classify_registered_worktree(
                RegisteredWorktreeIdentity {
                    canonical_path: Some(managed_path),
                    common_dir: Some(&identity),
                    primary_checkout: Some(Path::new("/src/other")),
                },
                &primary,
                Some(&managed_root),
                &identity,
            ),
            WorktreeClassification::Legacy
        );
        // Managed root authority unavailable (missing, symlinked or swapped
        // root) or equal to the entry itself: nothing may be managed.
        for canonical_managed_root in [None, Some(managed_path)] {
            assert_eq!(
                classify_registered_worktree(
                    identity_for(managed_path, Some(&identity), &primary),
                    &primary,
                    canonical_managed_root,
                    &identity,
                ),
                WorktreeClassification::Legacy,
                "{canonical_managed_root:?}"
            );
        }
    }

    fn fake_repository(repository: &Path) -> PathBuf {
        std::fs::create_dir_all(repository.join(".git")).unwrap();
        std::fs::write(
            repository.join(".git").join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .unwrap();
        std::fs::canonicalize(repository).unwrap()
    }

    fn fake_linked_worktree(repository: &Path, name: &str, worktree: &Path, branch: &str) {
        let private = repository.join(".git").join("worktrees").join(name);
        std::fs::create_dir_all(&private).unwrap();
        std::fs::create_dir_all(worktree).unwrap();
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
        std::fs::write(private.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    }

    #[test]
    fn session_workspace_identity_uses_the_exact_managed_authority() {
        let fixture = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(fixture.path()).unwrap();
        let repository = fake_repository(&src_root.join("repo"));
        let managed_root = src_root.join("worktrees").join("repo");
        let managed = managed_root.join("task");
        let legacy_sibling = src_root.join("repo-legacy");
        let legacy_inside = repository.join(".wt").join("legacy");
        let subdirectory = repository.join("sub");
        std::fs::create_dir_all(&subdirectory).unwrap();
        fake_linked_worktree(&repository, "task", &managed, "feature/x");
        fake_linked_worktree(&repository, "legacy-linked", &legacy_sibling, "legacy");
        fake_linked_worktree(&repository, "wt-legacy", &legacy_inside, "wt");

        let canonical =
            inspect_session_workspace(&repository, Some(&src_root)).expect("canonical workspace");
        assert_eq!(
            canonical.workspace_type,
            SessionWorkspaceType::CanonicalCheckout
        );
        assert_eq!(canonical.repository.as_deref(), Some("repo"));
        assert_eq!(canonical.workspace_root, repository);
        assert_eq!(canonical.repository_root, repository);
        assert_eq!(canonical.branch.as_deref(), Some("main"));
        assert_eq!(canonical.task, None);

        let managed_view = inspect_session_workspace(&managed, Some(&src_root)).expect("managed");
        assert_eq!(
            managed_view.workspace_type,
            SessionWorkspaceType::ManagedWorktree
        );
        assert_eq!(managed_view.repository.as_deref(), Some("repo"));
        assert_eq!(managed_view.repository_root, repository);
        assert_eq!(managed_view.workspace_root, managed);
        assert_eq!(managed_view.branch.as_deref(), Some("feature/x"));
        assert_eq!(managed_view.task.as_deref(), Some("task"));

        // A subdirectory reports the enclosing worktree root, not the cwd.
        let sub = inspect_session_workspace(&subdirectory, Some(&src_root)).expect("subdirectory");
        assert_eq!(sub.workspace_type, SessionWorkspaceType::CanonicalCheckout);
        assert_eq!(sub.workspace_root, repository);

        // Legacy worktrees are reported but never adopted.
        for (path, label) in [(&legacy_sibling, "src sibling"), (&legacy_inside, ".wt")] {
            let workspace = inspect_session_workspace(path, Some(&src_root))
                .unwrap_or_else(|| panic!("{label}"));
            assert_eq!(
                workspace.workspace_type,
                SessionWorkspaceType::LegacyWorktree,
                "{label}"
            );
            assert_eq!(workspace.task, None, "{label}");
        }

        // The managed namespace directory itself is never classified managed.
        assert!(
            inspect_session_workspace(&managed_root, Some(&src_root)).is_none_or(|workspace| {
                workspace.workspace_type != SessionWorkspaceType::ManagedWorktree
            })
        );

        // Without the configured src authority nothing may be managed.
        let unconfigured = inspect_session_workspace(&managed, None).expect("managed unconfigured");
        assert_eq!(
            unconfigured.workspace_type,
            SessionWorkspaceType::LegacyWorktree
        );
        assert_eq!(unconfigured.task, None);

        // A directory whose `.git` pointer cannot be resolved has no workspace
        // identity at all.
        let plain = src_root.join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join(".git"), "not a gitdir pointer\n").unwrap();
        assert_eq!(inspect_session_workspace(&plain, Some(&src_root)), None);

        #[cfg(unix)]
        {
            std::fs::rename(&managed_root, src_root.join("swapped")).unwrap();
            std::os::unix::fs::symlink(src_root.join("swapped"), &managed_root).unwrap();
            let swapped = inspect_session_workspace(&managed, Some(&src_root)).expect("swapped");
            assert_eq!(swapped.workspace_type, SessionWorkspaceType::LegacyWorktree);
            assert_eq!(swapped.task, None);
        }
    }

    #[test]
    fn reusable_managed_worktree_requires_identity_and_branch() {
        let fixture = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(fixture.path()).unwrap();
        let repository = fake_repository(&src_root.join("repo"));
        let managed_root = src_root.join("worktrees").join("repo");
        let managed = managed_root.join("task");
        let legacy_inside = repository.join(".wt").join("legacy");
        fake_linked_worktree(&repository, "task", &managed, "feature/x");
        fake_linked_worktree(&repository, "wt-legacy", &legacy_inside, "feature/x");
        let other = fake_repository(&src_root.join("other"));

        let managed_repository = ManagedRepository::resolve(&repository, &src_root).unwrap();
        let common = crate::sandbox::git_common_dir(&repository).unwrap();
        let primary = crate::sandbox::git_primary_checkout(&repository).unwrap();

        verify_reusable_managed_worktree(
            &managed_repository,
            &managed,
            "feature/x",
            &common,
            &primary,
        )
        .unwrap();

        // Wrong branch, wrong repository identity and a legacy location fail
        // closed.
        assert!(
            verify_reusable_managed_worktree(
                &managed_repository,
                &managed,
                "feature/other",
                &common,
                &primary,
            )
            .is_err()
        );
        assert!(
            verify_reusable_managed_worktree(
                &managed_repository,
                &managed,
                "feature/x",
                &crate::sandbox::git_common_dir(&other).unwrap(),
                &primary,
            )
            .is_err()
        );
        assert!(
            verify_reusable_managed_worktree(
                &managed_repository,
                &repository.join(".wt").join("legacy"),
                "feature/x",
                &common,
                &primary,
            )
            .is_err()
        );
        #[cfg(unix)]
        {
            let symlinked = managed_root.join("symlinked");
            std::os::unix::fs::symlink(&managed, &symlinked).unwrap();
            assert!(
                verify_reusable_managed_worktree(
                    &managed_repository,
                    &symlinked,
                    "feature/x",
                    &common,
                    &primary,
                )
                .is_err()
            );
        }
        // A nested descendant is never a direct child of the managed root.
        let nested = managed.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(
            verify_reusable_managed_worktree(
                &managed_repository,
                &nested,
                "feature/x",
                &common,
                &primary,
            )
            .is_err()
        );
        // A missing target inside the fixture and a mismatched primary
        // checkout are not the selected repository's managed identity.
        assert!(
            verify_reusable_managed_worktree(
                &managed_repository,
                &src_root.join("missing-target"),
                "feature/x",
                &common,
                &primary,
            )
            .is_err()
        );
        assert!(
            verify_reusable_managed_worktree(
                &managed_repository,
                &managed,
                "feature/x",
                &common,
                Path::new("/home/user/src/other"),
            )
            .is_err()
        );
    }

    fn run_git_fixture(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", cwd.display());
    }

    #[test]
    fn reusable_managed_worktree_rejects_a_valid_worktree_outside_the_managed_root() {
        let fixture = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(fixture.path()).unwrap();
        let repository = src_root.join("repo");
        std::fs::create_dir(&repository).unwrap();
        run_git_fixture(&repository, &["init", "--quiet"]);
        run_git_fixture(&repository, &["config", "user.name", "Temote Test"]);
        run_git_fixture(
            &repository,
            &["config", "user.email", "temote-test@example.invalid"],
        );
        std::fs::write(repository.join("tracked.txt"), "base\n").unwrap();
        run_git_fixture(&repository, &["add", "tracked.txt"]);
        run_git_fixture(&repository, &["commit", "--quiet", "-m", "initial"]);
        run_git_fixture(&repository, &["branch", "-M", "main"]);
        run_git_fixture(&repository, &["branch", "outside-branch"]);

        // A real, structurally valid linked worktree of the selected
        // repository on the requested branch, but outside the trusted managed
        // root.
        let outside = src_root.join("elsewhere").join("task");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        run_git_fixture(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                outside.to_str().unwrap(),
                "outside-branch",
            ],
        );

        let repository = std::fs::canonicalize(&repository).unwrap();
        let outside = std::fs::canonicalize(&outside).unwrap();
        let managed_root = src_root.join("worktrees").join("repo");
        std::fs::create_dir_all(&managed_root).unwrap();
        let managed_repository = ManagedRepository::resolve(&repository, &src_root).unwrap();
        let common = crate::sandbox::git_common_dir(&repository).unwrap();
        let primary = crate::sandbox::git_primary_checkout(&repository).unwrap();

        // Preconditions: same repository identity, requested branch, normal
        // canonical directory and a trusted managed root that does not contain
        // the target.
        assert_eq!(crate::sandbox::git_common_dir(&outside).unwrap(), common);
        assert_eq!(
            crate::sandbox::git_primary_checkout(&outside).unwrap(),
            repository
        );
        assert_eq!(
            crate::sandbox::git_current_branch(&outside)
                .unwrap()
                .as_deref(),
            Some("outside-branch")
        );
        let metadata = std::fs::symlink_metadata(&outside).unwrap();
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        assert_eq!(std::fs::canonicalize(&outside).unwrap(), outside);
        assert_eq!(
            trusted_canonical_managed_root(&managed_repository),
            Some(managed_root.clone())
        );
        assert!(outside.parent() != Some(managed_root.as_path()));

        // The rejection reason is the managed-root / exact direct-child rule,
        // not a missing target or broken Git metadata.
        let error = verify_reusable_managed_worktree(
            &managed_repository,
            &outside,
            "outside-branch",
            &common,
            &primary,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("direct child"), "{message}");
        assert!(!message.contains("cannot inspect"), "{message}");
        assert!(
            !message.contains("outside the working directory"),
            "{message}"
        );
    }

    #[test]
    fn porcelain_listing_is_parsed_without_losing_legacy_metadata() {
        let porcelain = "\
worktree /src/repo
HEAD 0123456789abcdef0123456789abcdef01234567
branch refs/heads/main

worktree /src/worktrees/repo/task
HEAD 1111111111111111111111111111111111111111
branch refs/heads/feature/foo
locked maintenance

worktree /src/repo/.wt/legacy
HEAD 2222222222222222222222222222222222222222
detached
prunable gitdir file points to non-existent location

";
        let entries = parse_worktree_list(porcelain).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, PathBuf::from("/src/repo"));
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(
            entries[0].head.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(entries[1].branch.as_deref(), Some("feature/foo"));
        assert!(entries[2].detached);
        assert!(entries[2].prunable);
        assert!(entries[2].branch.is_none());

        for invalid in [
            "HEAD abc\n",
            "worktree relative/path\n",
            "worktree \"/quoted path\"\n",
            "worktree /src/repo\nunknown attribute\n",
        ] {
            assert!(parse_worktree_list(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn generated_task_inputs_never_escape_the_managed_root() -> noprop::TestResult {
        test_support::run(0x4d41_4e41_4745_4457, 1024, |ctx| {
            let src = PathBuf::from("/src");
            let checkout =
                PathBuf::from(format!("/src/repository-{:016x}", noprop::sample_u64(ctx)));
            let repository = ManagedRepository::resolve(&checkout, &src).unwrap();
            let len = noprop::sample_usize_in(ctx, 0..=MAX_MANAGED_TASK_BYTES + 4);
            let candidate = (0..len)
                .map(|_| match noprop::sample_usize_in(ctx, 0..=6) {
                    0 => '/',
                    1 => '.',
                    2 => '-',
                    3 => '_',
                    4 => '\\',
                    5 => 'a',
                    _ => char::from_u32(0x20 + noprop::sample_u32(ctx) % 95).unwrap(),
                })
                .collect::<String>();
            if validate_task_name(&candidate).is_ok() {
                let target = repository.target(&candidate).unwrap();
                assert!(target.parent() == Some(repository.managed_root()));
                assert!(target.starts_with(repository.managed_root()));
                assert!(!candidate.contains('/') && !candidate.contains('\\'));
            }
            Ok(())
        })
    }
}
