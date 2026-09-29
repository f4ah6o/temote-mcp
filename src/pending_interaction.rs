//! Shared bounded summary for backend approval and interaction state.
//!
//! These values deliberately contain only allow-listed kinds and freshness
//! metadata. They never carry request bodies, prompts, or approval details.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const MAX_SERIALIZED_BYTES: usize = 4 * 1024;
pub const MAX_COUNT: u8 = 64;
pub const MAX_TYPES: usize = 4;
pub const REFRESH_INTERVAL_SECS: u64 = 5;
pub const SCOPED_READ_TIMEOUT_SECS: u64 = 2;
pub const SUMMARY_TTL_SECS: u64 = 30;
pub const HOST_CONCURRENCY: usize = 4;
pub const CLOUD_OBSERVER_LEASE_SECS: u64 = 15;
const HOST_SLOT_COUNT: usize = HOST_CONCURRENCY;
const HOST_SLOT_RETRY: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SummaryState {
    None,
    Pending,
    Unknown,
    Unsupported,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProducerKind {
    RuntimeOwner,
    HostRemoteObserver,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum InteractionType {
    Permission,
    Question,
    Approval,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub state: SummaryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u8>,
    pub types: Vec<InteractionType>,
    pub summary_revision: u64,
    pub observed_at: u64,
    pub producer_kind: ProducerKind,
    pub producer_epoch: u64,
    pub expires_at: u64,
    pub truncated: bool,
}

/// Name used by task metadata and backend producers.
pub type PendingInteractionSummary = Summary;

impl Summary {
    /// Create an observation and advance the independent summary revision only
    /// when its semantic contents change.
    #[allow(clippy::too_many_arguments)]
    pub fn observe(
        previous: Option<&Self>,
        state: SummaryState,
        count: Option<u8>,
        types: &[InteractionType],
        truncated: bool,
        producer_kind: ProducerKind,
        producer_epoch: u64,
        now: u64,
    ) -> Result<Self> {
        anyhow::ensure!(
            count.is_none_or(|count| count <= MAX_COUNT),
            "pending interaction count exceeds {MAX_COUNT}"
        );

        let mut bounded_types = types.to_vec();
        bounded_types.sort_unstable();
        bounded_types.dedup();
        let was_truncated = truncated || bounded_types.len() > MAX_TYPES;
        bounded_types.truncate(MAX_TYPES);

        if state == SummaryState::None {
            anyhow::ensure!(
                bounded_types.is_empty() && count.is_none_or(|count| count == 0) && !was_truncated,
                "none pending interaction summary cannot contain pending items"
            );
        }

        if let Some(previous) = previous {
            previous.validate()?;
            anyhow::ensure!(
                previous.producer_kind != producer_kind
                    || producer_epoch >= previous.producer_epoch,
                "pending interaction producer epoch cannot move backwards"
            );
        }
        let semantic_change = previous.is_none_or(|previous| {
            previous.state != state
                || previous.count != count
                || previous.types != bounded_types
                || previous.truncated != was_truncated
        });
        let summary_revision = match previous {
            None => 1,
            Some(previous) if semantic_change => previous
                .summary_revision
                .checked_add(1)
                .context("pending interaction summary revision overflow")?,
            Some(previous) => previous.summary_revision,
        };
        let expires_at = now
            .checked_add(SUMMARY_TTL_SECS)
            .context("pending interaction summary expiry overflow")?;
        let summary = Self {
            state,
            count,
            types: bounded_types,
            summary_revision,
            observed_at: now,
            producer_kind,
            producer_epoch,
            expires_at,
            truncated: was_truncated,
        };
        summary.validate()?;
        Ok(summary)
    }

    /// Record a failed scoped read without pretending that the backend was
    /// observed successfully. Existing freshness timestamps remain unchanged.
    pub fn unavailable(
        previous: Option<&Self>,
        producer_kind: ProducerKind,
        producer_epoch: u64,
    ) -> Result<Self> {
        let (mut summary, semantic_change) = if let Some(previous) = previous {
            previous.validate()?;
            anyhow::ensure!(
                previous.producer_kind != producer_kind
                    || producer_epoch >= previous.producer_epoch,
                "pending interaction producer epoch cannot move backwards"
            );
            let mut summary = previous.clone();
            let semantic_change = summary.state != SummaryState::Unavailable
                || summary.count.is_some()
                || !summary.types.is_empty()
                || summary.truncated;
            summary.state = SummaryState::Unavailable;
            summary.count = None;
            summary.types.clear();
            summary.truncated = false;
            summary.producer_kind = producer_kind;
            summary.producer_epoch = producer_epoch;
            (summary, semantic_change)
        } else {
            (
                Self {
                    state: SummaryState::Unavailable,
                    count: None,
                    types: Vec::new(),
                    summary_revision: 1,
                    observed_at: 0,
                    producer_kind,
                    producer_epoch,
                    expires_at: SUMMARY_TTL_SECS,
                    truncated: false,
                },
                false,
            )
        };
        if semantic_change {
            summary.summary_revision = summary
                .summary_revision
                .checked_add(1)
                .context("pending interaction summary revision overflow")?;
        }
        summary.validate()?;
        Ok(summary)
    }

    /// Validate a deserialized summary before trusting it as task metadata.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.count.is_none_or(|count| count <= MAX_COUNT),
            "pending interaction count exceeds {MAX_COUNT}"
        );
        anyhow::ensure!(
            self.summary_revision > 0,
            "pending interaction summary revision must be positive"
        );
        anyhow::ensure!(
            self.producer_epoch > 0,
            "pending interaction producer epoch must be positive"
        );
        anyhow::ensure!(
            self.types.len() <= MAX_TYPES,
            "pending interaction summary contains too many types"
        );
        let mut sorted_types = self.types.clone();
        sorted_types.sort_unstable();
        sorted_types.dedup();
        anyhow::ensure!(
            sorted_types == self.types,
            "pending interaction summary types are not unique and sorted"
        );
        anyhow::ensure!(
            self.state != SummaryState::None
                || (self.types.is_empty()
                    && self.count.is_none_or(|count| count == 0)
                    && !self.truncated),
            "none pending interaction summary cannot contain pending items"
        );
        let expected_expiry = self
            .observed_at
            .checked_add(SUMMARY_TTL_SECS)
            .context("pending interaction summary expiry overflow")?;
        anyhow::ensure!(
            self.expires_at == expected_expiry,
            "pending interaction summary expiry does not match its TTL"
        );
        let bytes =
            serde_json::to_vec(self).context("cannot serialize pending interaction summary")?;
        anyhow::ensure!(
            bytes.len() <= MAX_SERIALIZED_BYTES,
            "pending interaction summary exceeds {MAX_SERIALIZED_BYTES} bytes"
        );
        Ok(())
    }

    /// Project task metadata for a reader without turning absent or stale
    /// values into `none`.
    pub fn projection(summary: Option<&Self>, now: u64) -> Value {
        let Some(summary) = summary else {
            return json!({"state": "unsupported"});
        };
        if summary.validate().is_err() {
            return json!({"state": "unavailable"});
        }
        if now >= summary.expires_at {
            return json!({
                "state": "unavailable",
                "summary_revision": summary.summary_revision,
                "observed_at": summary.observed_at,
                "producer_kind": summary.producer_kind,
                "producer_epoch": summary.producer_epoch,
                "expires_at": summary.expires_at,
            });
        }
        serde_json::to_value(summary).unwrap_or_else(|_| json!({"state": "unavailable"}))
    }
}

fn host_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(HOST_CONCURRENCY))))
}

/// A pending-observation slot held across one scoped backend read. The stable
/// lock file makes the host-wide limit work across competing processes too.
pub struct HostObservationPermit {
    _process_permit: OwnedSemaphorePermit,
    #[cfg(unix)]
    _lock_file: File,
}

/// Acquire one of the four host-wide pending-interaction observation slots.
///
/// On platforms without `flock`, observation fails closed because an
/// in-process semaphore cannot enforce the host-wide concurrency limit.
pub async fn acquire_host_semaphore_permit() -> Result<HostObservationPermit> {
    #[cfg(not(unix))]
    anyhow::bail!("host-wide pending interaction observation slots require Unix flock");

    #[cfg(unix)]
    acquire_host_semaphore_permit_at(&observation_slot_directory()?).await
}

#[cfg(unix)]
async fn acquire_host_semaphore_permit_at(directory: &Path) -> Result<HostObservationPermit> {
    let process_permit = host_semaphore()
        .acquire_owned()
        .await
        .context("pending interaction host semaphore is closed")?;
    loop {
        for slot in 0..HOST_SLOT_COUNT {
            if let Some(lock_file) = try_lock_host_slot(directory, slot)? {
                return Ok(HostObservationPermit {
                    _process_permit: process_permit,
                    _lock_file: lock_file,
                });
            }
        }
        tokio::time::sleep(HOST_SLOT_RETRY).await;
    }
}

#[cfg(unix)]
fn observation_slot_directory() -> Result<PathBuf> {
    let state_dir = crate::config::state_dir()?;
    ensure_private_directory(&state_dir)?;
    let slots = state_dir.join("pending-interaction-slots");
    ensure_private_directory(&slots)?;
    Ok(slots)
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "cannot create pending interaction lock parent {}",
                        parent.display()
                    )
                })?;
            }
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "cannot create pending interaction lock directory {}",
                            path.display()
                        )
                    });
                }
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot inspect pending interaction lock directory {}",
                    path.display()
                )
            });
        }
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let directory = options.open(path).with_context(|| {
        format!(
            "cannot open pending interaction lock directory {}",
            path.display()
        )
    })?;
    validate_private_directory(path, &directory)
}

#[cfg(unix)]
fn validate_private_directory(path: &Path, directory: &File) -> Result<()> {
    let metadata = directory.metadata()?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "pending interaction lock path must be a real directory: {}",
        path.display()
    );
    validate_directory_owner(metadata.uid())?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        // This is the dedicated Temote MCP state directory or the private
        // slot directory. Tighten only a directory we own, using its already
        // opened no-follow descriptor to avoid replacing a raced path.
        // SAFETY: `directory` is an open descriptor to a verified directory.
        let status = unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) };
        anyhow::ensure!(
            status == 0,
            "cannot restrict pending interaction lock directory permissions: {}",
            std::io::Error::last_os_error()
        );
    }
    let metadata = directory.metadata()?;
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "pending interaction lock directory must be owner-only (mode {mode:04o})"
    );
    Ok(())
}

#[cfg(unix)]
fn validate_directory_owner(actual_uid: u32) -> Result<()> {
    // SAFETY: geteuid has no memory-safety preconditions.
    let expected_uid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        actual_uid == expected_uid,
        "pending interaction lock directory is not owned by the current user"
    );
    Ok(())
}

#[cfg(unix)]
fn try_lock_host_slot(directory: &Path, slot: usize) -> Result<Option<File>> {
    let path = directory.join(format!("slot-{slot}.lock"));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(&path)
        .with_context(|| format!("cannot open pending interaction slot {slot}"))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "pending interaction lock path must be a regular file: {}",
        path.display()
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "pending interaction lock file must be owner-only (mode {mode:04o})"
    );
    // SAFETY: `file` is an open descriptor kept alive by the returned permit.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == Some(libc::EAGAIN)
        || error.raw_os_error() == Some(libc::EWOULDBLOCK)
    {
        return Ok(None);
    }
    Err(error).context("cannot lock pending interaction host slot")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn summary_bounds_and_revision_only_follow_semantics() {
        let first = Summary::observe(
            None,
            SummaryState::Pending,
            Some(1),
            &[InteractionType::Approval],
            false,
            ProducerKind::HostRemoteObserver,
            9,
            100,
        )
        .unwrap();
        let heartbeat = Summary::observe(
            Some(&first),
            SummaryState::Pending,
            Some(1),
            &[InteractionType::Approval],
            false,
            ProducerKind::HostRemoteObserver,
            9,
            105,
        )
        .unwrap();
        assert_eq!(first.summary_revision, 1);
        assert_eq!(heartbeat.summary_revision, 1);
        assert_eq!(heartbeat.observed_at, 105);
        assert_eq!(heartbeat.expires_at, 135);

        let changed = Summary::observe(
            Some(&heartbeat),
            SummaryState::None,
            Some(0),
            &[],
            false,
            ProducerKind::HostRemoteObserver,
            9,
            110,
        )
        .unwrap();
        assert_eq!(changed.summary_revision, 2);
        assert!(serde_json::to_vec(&changed).unwrap().len() <= MAX_SERIALIZED_BYTES);
    }

    #[test]
    fn projection_distinguishes_missing_stale_and_fresh() {
        assert_eq!(
            Summary::projection(None, 10),
            json!({"state": "unsupported"})
        );
        let summary = Summary::observe(
            None,
            SummaryState::None,
            Some(0),
            &[],
            false,
            ProducerKind::RuntimeOwner,
            4,
            10,
        )
        .unwrap();
        assert_eq!(Summary::projection(Some(&summary), 39)["state"], "none");
        assert_eq!(
            Summary::projection(Some(&summary), 40)["state"],
            "unavailable"
        );
        assert!(
            Summary::projection(Some(&summary), 40)
                .get("count")
                .is_none()
        );

        let unknown = Summary::observe(
            Some(&summary),
            SummaryState::Unknown,
            None,
            &[],
            false,
            ProducerKind::RuntimeOwner,
            4,
            11,
        )
        .unwrap();
        assert!(
            Summary::projection(Some(&unknown), 11)
                .get("count")
                .is_none()
        );
    }

    #[test]
    fn rejects_corruption_and_revision_overflow() {
        let mut invalid = Summary::observe(
            None,
            SummaryState::None,
            Some(0),
            &[],
            false,
            ProducerKind::RuntimeOwner,
            1,
            10,
        )
        .unwrap();
        invalid.expires_at += 1;
        assert!(invalid.validate().is_err());
        assert_eq!(
            Summary::projection(Some(&invalid), 10)["state"],
            "unavailable"
        );

        invalid.summary_revision = u64::MAX;
        invalid.expires_at = invalid.observed_at + SUMMARY_TTL_SECS;
        assert!(
            Summary::observe(
                Some(&invalid),
                SummaryState::Pending,
                None,
                &[InteractionType::Approval],
                false,
                ProducerKind::RuntimeOwner,
                1,
                11,
            )
            .is_err()
        );
    }

    #[test]
    fn unavailable_observation_preserves_last_successful_timestamp() {
        let observed = Summary::observe(
            None,
            SummaryState::Pending,
            Some(2),
            &[InteractionType::Approval],
            false,
            ProducerKind::HostRemoteObserver,
            7,
            100,
        )
        .unwrap();
        let failed =
            Summary::unavailable(Some(&observed), ProducerKind::HostRemoteObserver, 7).unwrap();
        assert_eq!(failed.state, SummaryState::Unavailable);
        assert_eq!(failed.observed_at, observed.observed_at);
        assert_eq!(failed.expires_at, observed.expires_at);
        assert_eq!(failed.summary_revision, observed.summary_revision + 1);
        assert!(failed.types.is_empty());

        let first_failure =
            Summary::unavailable(None, ProducerKind::HostRemoteObserver, 1).unwrap();
        assert_eq!(first_failure.observed_at, 0);
        assert_eq!(first_failure.expires_at, SUMMARY_TTL_SECS);
    }

    #[cfg(unix)]
    #[test]
    fn competing_processes_share_four_observation_slots() {
        const TEST_ROOT_ENV: &str = "TEMOTE_MCP_PENDING_SLOT_TEST_ROOT";
        const TEST_INDEX_ENV: &str = "TEMOTE_MCP_PENDING_SLOT_TEST_INDEX";
        const RELEASE_FILE: &str = "release";

        if let (Ok(root), Ok(index)) = (std::env::var(TEST_ROOT_ENV), std::env::var(TEST_INDEX_ENV))
        {
            let root = PathBuf::from(root);
            let marker = |prefix: &str| root.join(format!("{prefix}-{index}"));
            std::fs::write(marker("started"), b"ready").unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            runtime.block_on(async {
                let permit = acquire_host_semaphore_permit_at(&root).await.unwrap();
                std::fs::write(marker("acquired"), b"yes").unwrap();
                while !root.join(RELEASE_FILE).exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                drop(permit);
            });
            return;
        }

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("slots");
        ensure_private_directory(&root).unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut children = (0..HOST_CONCURRENCY + 1)
            .map(|index| {
                std::process::Command::new(&executable)
                    .args([
                        "--exact",
                        "pending_interaction::tests::competing_processes_share_four_observation_slots",
                        "--nocapture",
                    ])
                    .env(TEST_ROOT_ENV, &root)
                    .env(TEST_INDEX_ENV, index.to_string())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while count_markers(&root, "started-") < HOST_CONCURRENCY + 1
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = count_markers(&root, "started-");

        let acquire_deadline = std::time::Instant::now() + Duration::from_secs(10);
        while count_markers(&root, "acquired-") < HOST_CONCURRENCY
            && std::time::Instant::now() < acquire_deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(250));
        let simultaneous = count_markers(&root, "acquired-");

        std::fs::write(root.join(RELEASE_FILE), b"release").unwrap();
        let mut child_success = true;
        for child in &mut children {
            child_success &= child.wait().unwrap().success();
        }
        let acquired_after_release = count_markers(&root, "acquired-");
        assert_eq!(started, HOST_CONCURRENCY + 1);
        assert_eq!(simultaneous, HOST_CONCURRENCY);
        assert!(child_success);
        assert_eq!(acquired_after_release, HOST_CONCURRENCY + 1);
    }

    #[cfg(unix)]
    #[test]
    fn private_slot_directories_are_hardened_and_reject_symlinks_or_other_owners() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let temporary = tempfile::tempdir().unwrap();
        let broad = temporary.path().join("state");
        std::fs::create_dir(&broad).unwrap();
        std::fs::set_permissions(&broad, std::fs::Permissions::from_mode(0o775)).unwrap();
        ensure_private_directory(&broad).unwrap();
        assert_eq!(
            std::fs::metadata(&broad).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let uid = std::fs::metadata(&broad).unwrap().uid();
        assert!(validate_directory_owner(uid.wrapping_add(1)).is_err());

        let target = temporary.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = temporary.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(ensure_private_directory(&link).is_err());
    }

    #[cfg(unix)]
    fn count_markers(directory: &Path, prefix: &str) -> usize {
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(prefix))
            })
            .count()
    }

    #[test]
    fn generated_summary_revision_and_freshness_match_reference_model() -> noprop::TestResult {
        test_support::run(0x5045_4e44_5355_4d01, 1024, |ctx| {
            let steps = noprop::sample_usize_in(ctx, 0..=64);
            let mut previous: Option<Summary> = None;
            let mut model: Option<(SummaryState, Option<u8>, Vec<InteractionType>, bool)> = None;
            let mut expected_revision = 0u64;
            for now in (1u64..).take(steps) {
                let state = match noprop::sample_usize_in(ctx, 0..=3) {
                    0 => SummaryState::None,
                    1 => SummaryState::Pending,
                    2 => SummaryState::Unknown,
                    _ => SummaryState::Unavailable,
                };
                let count = match noprop::sample_usize_in(ctx, 0..=2) {
                    0 => None,
                    1 if state == SummaryState::None => Some(0),
                    1 => Some(noprop::sample_u8(ctx) % (MAX_COUNT + 1)),
                    _ if state == SummaryState::None => Some(0),
                    _ => None,
                };
                let raw_type_count = noprop::sample_usize_in(ctx, 0..=8);
                let mut raw_types = (0..raw_type_count)
                    .map(|_| match noprop::sample_usize_in(ctx, 0..=2) {
                        0 => InteractionType::Permission,
                        1 => InteractionType::Question,
                        _ => InteractionType::Approval,
                    })
                    .collect::<Vec<_>>();
                raw_types.sort_unstable();
                raw_types.dedup();
                let truncated = noprop::sample_bool(ctx);
                raw_types.truncate(MAX_TYPES);
                let (types, count, truncated) = if state == SummaryState::None {
                    (Vec::new(), Some(0), false)
                } else {
                    (raw_types, count, truncated)
                };
                let semantic = (state, count, types.clone(), truncated);
                if model.as_ref() != Some(&semantic) {
                    expected_revision = expected_revision.checked_add(1).unwrap();
                    model = Some(semantic);
                }
                let next = Summary::observe(
                    previous.as_ref(),
                    state,
                    count,
                    &types,
                    truncated,
                    ProducerKind::RuntimeOwner,
                    1,
                    now,
                )
                .unwrap();
                assert_eq!(next.summary_revision, expected_revision);
                assert_eq!(next.expires_at, now + SUMMARY_TTL_SECS);
                assert!(next.types.len() <= MAX_TYPES);
                assert!(serde_json::to_vec(&next).unwrap().len() <= MAX_SERIALIZED_BYTES);
                assert_eq!(
                    Summary::projection(Some(&next), next.expires_at - 1)["state"],
                    serde_json::to_value(next.state).unwrap()
                );
                assert_eq!(
                    Summary::projection(Some(&next), next.expires_at)["state"],
                    "unavailable"
                );
                previous = Some(next);
            }
            Ok(())
        })
    }
}
