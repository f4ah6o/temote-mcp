use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config;

const FRICTION_SCHEMA_VERSION: u64 = 1;
const MAX_EVENT_BYTES: usize = 16 * 1024;
const MAX_EVENTS: usize = 512;
const MAX_DIRECTORY_ENTRIES: usize = 4096;
const MAX_IDENTIFIER_BYTES: usize = 64;
const MAX_SCOPE_PATH_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FrictionKind {
    ApprovalDenied,
    SandboxDenied,
    PathEscapeRejected,
    ExecuteFailed,
    JobFailed,
    JobCancelled,
    SessionCrashed,
    GitOperationFailed,
    IntegrationFailed,
    ClientOperationRetried,
    AmbiguousMutationDetected,
    RecallMiss,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservationSource {
    Observed,
    ClientReported,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventOutcome {
    Denied,
    Failed,
    Cancelled,
    Retried,
    Ambiguous,
    NoHit,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct FrictionEvent {
    pub schema_version: u64,
    pub event_id: Uuid,
    pub session_id: String,
    pub scope_cwd: PathBuf,
    pub occurred_at: u64,
    pub kind: FrictionKind,
    pub source: ObservationSource,
    pub operation_class: Option<String>,
    pub tool_name: Option<String>,
    pub outcome: EventOutcome,
    pub retry_group: Option<Uuid>,
    pub related_checkpoint: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub(crate) struct Store {
    directory: PathBuf,
    max_events: usize,
}

impl Store {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            max_events: MAX_EVENTS,
        }
    }

    #[cfg(test)]
    fn with_max_events(mut self, max_events: usize) -> Self {
        self.max_events = max_events;
        self
    }

    pub(crate) fn default_store() -> Result<Self> {
        Ok(Self::new(config::state_dir()?.join("friction-events")))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &self,
        session: &config::Session,
        kind: FrictionKind,
        source: ObservationSource,
        operation_class: Option<&str>,
        tool_name: Option<&str>,
        outcome: EventOutcome,
        retry_group: Option<Uuid>,
        related_checkpoint: Option<Uuid>,
    ) -> Result<FrictionEvent> {
        validate_session_scope(session)?;
        let operation_class = validate_optional_identifier(operation_class, "operation_class")?;
        let tool_name = validate_optional_identifier(tool_name, "tool_name")?;
        self.ensure_directory()?;
        let _lock = self.acquire_lock()?;
        let event = FrictionEvent {
            schema_version: FRICTION_SCHEMA_VERSION,
            event_id: Uuid::new_v4(),
            session_id: session.id.clone(),
            scope_cwd: session.cwd.clone(),
            occurred_at: config::unix_time(),
            kind,
            source,
            operation_class,
            tool_name,
            outcome,
            retry_group,
            related_checkpoint,
        };
        validate_event(&event)?;
        self.write_event(&event)?;
        self.prune_locked()?;
        Ok(event)
    }

    fn ensure_directory(&self) -> Result<()> {
        if let Some(parent) = self.directory.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::symlink_metadata(&self.directory) {
            Ok(metadata) => validate_directory(&self.directory, &metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&self.directory)?;
                std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))?;
                let metadata = std::fs::symlink_metadata(&self.directory)?;
                validate_directory(&self.directory, &metadata)
            }
            Err(error) => Err(error).context("cannot inspect friction event store"),
        }
    }

    fn acquire_lock(&self) -> Result<FileLock> {
        let path = self.directory.join(".lock");
        reject_symlink(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        validate_private_file(&path, &file.metadata()?)?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("cannot lock friction store");
        }
        Ok(FileLock { file })
    }

    fn write_event(&self, event: &FrictionEvent) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(event)?;
        anyhow::ensure!(
            bytes.len() <= MAX_EVENT_BYTES,
            "friction event exceeds size limit"
        );
        let path = self.directory.join(format!("{}.json", event.event_id));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        if let Ok(directory) = File::open(&self.directory) {
            let _ = directory.sync_all();
        }
        Ok(())
    }

    fn read_all(&self) -> Result<Vec<FrictionEvent>> {
        let metadata = match std::fs::symlink_metadata(&self.directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("cannot inspect friction event store"),
        };
        validate_directory(&self.directory, &metadata)?;
        let mut events = Vec::new();
        let mut entries = 0usize;
        for entry in std::fs::read_dir(&self.directory)? {
            entries += 1;
            anyhow::ensure!(
                entries <= MAX_DIRECTORY_ENTRIES,
                "friction event directory contains too many entries"
            );
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Ok(event_id) = Uuid::parse_str(stem) else {
                continue;
            };
            if event_id.to_string() != stem {
                continue;
            }
            events.push(self.read_event(event_id)?);
        }
        Ok(events)
    }

    fn read_event(&self, event_id: Uuid) -> Result<FrictionEvent> {
        let path = self.directory.join(format!("{event_id}.json"));
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let metadata = file.metadata()?;
        validate_private_file(&path, &metadata)?;
        anyhow::ensure!(
            metadata.len() <= MAX_EVENT_BYTES as u64,
            "friction event exceeds size limit"
        );
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_EVENT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= MAX_EVENT_BYTES,
            "friction event exceeds size limit"
        );
        let event: FrictionEvent =
            serde_json::from_slice(&bytes).context("invalid friction event")?;
        anyhow::ensure!(event.event_id == event_id, "friction event ID mismatch");
        validate_event(&event)?;
        Ok(event)
    }

    fn prune_locked(&self) -> Result<()> {
        let mut events = self.read_all()?;
        if events.len() <= self.max_events {
            return Ok(());
        }
        events.sort_by_key(|event| (event.occurred_at, event.event_id));
        let remove_count = events.len() - self.max_events;
        for event in events.into_iter().take(remove_count) {
            let path = self.directory.join(format!("{}.json", event.event_id));
            reject_symlink(&path)?;
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("cannot prune friction event"),
            }
        }
        Ok(())
    }
}

struct FileLock {
    file: File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub(crate) fn record_observed(
    session: &config::Session,
    kind: FrictionKind,
    operation_class: Option<&str>,
    tool_name: Option<&str>,
    outcome: EventOutcome,
    retry_group: Option<Uuid>,
    related_checkpoint: Option<Uuid>,
) {
    if let Ok(store) = Store::default_store() {
        let _ = store.record(
            session,
            kind,
            ObservationSource::Observed,
            operation_class,
            tool_name,
            outcome,
            retry_group,
            related_checkpoint,
        );
    }
}

fn validate_session_scope(session: &config::Session) -> Result<()> {
    config::validate_session_id(&session.id)?;
    let canonical = config::canonical_directory(&session.cwd)?;
    anyhow::ensure!(
        canonical == session.cwd,
        "friction scope cwd is not canonical"
    );
    Ok(())
}

fn validate_optional_identifier(value: Option<&str>, label: &str) -> Result<Option<String>> {
    let Some(value) = value else { return Ok(None) };
    anyhow::ensure!(
        !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES,
        "{label} must contain 1..={MAX_IDENTIFIER_BYTES} bytes"
    );
    anyhow::ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')),
        "{label} contains unsupported characters"
    );
    Ok(Some(value.to_owned()))
}

fn validate_event(event: &FrictionEvent) -> Result<()> {
    anyhow::ensure!(
        event.schema_version == FRICTION_SCHEMA_VERSION,
        "unsupported friction schema version"
    );
    config::validate_session_id(&event.session_id)?;
    validate_persisted_scope(&event.scope_cwd)?;
    validate_optional_identifier(event.operation_class.as_deref(), "operation_class")?;
    validate_optional_identifier(event.tool_name.as_deref(), "tool_name")?;
    Ok(())
}

fn validate_persisted_scope(path: &Path) -> Result<()> {
    let bytes = path.as_os_str().as_bytes();
    anyhow::ensure!(path.is_absolute(), "friction scope must be absolute");
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_SCOPE_PATH_BYTES,
        "friction scope must contain 1..={MAX_SCOPE_PATH_BYTES} bytes"
    );
    anyhow::ensure!(
        !bytes.contains(&0),
        "friction scope must not contain NUL bytes"
    );

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => normalized.push(component.as_os_str()),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                anyhow::bail!("friction scope is not lexically normalized")
            }
        }
    }
    anyhow::ensure!(
        normalized.as_os_str().as_bytes() == bytes,
        "friction scope is not lexically normalized"
    );
    Ok(())
}

fn validate_directory(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "friction store must be a real directory"
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "friction store must be owner-only: {}",
        path.display()
    );
    Ok(())
}

fn validate_private_file(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "friction event is not a regular file: {}",
        path.display()
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(mode & 0o077 == 0, "friction event must be owner-only");
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "friction path may not be a symlink"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect friction path"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(root: &Path, id: &str) -> config::Session {
        let cwd = config::canonical_directory(root).unwrap();
        config::Session {
            id: id.to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 1,
            process_id: 2,
            permission_mode: config::PermissionMode::Yolo,
            grants: config::SessionGrants::default(),
        }
    }

    #[test]
    fn friction_event_records_operation_class_without_command_argv() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let store = Store::new(state.path().join("friction"));
        let session = session(root.path(), "privacy");
        let event = store
            .record(
                &session,
                FrictionKind::ExecuteFailed,
                ObservationSource::Observed,
                Some("execute"),
                Some("execute"),
                EventOutcome::Failed,
                None,
                None,
            )
            .unwrap();
        let bytes =
            std::fs::read(store.directory.join(format!("{}.json", event.event_id))).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("execute_failed"));
        assert!(!text.contains("command"));
        assert!(!text.contains("argv"));
        assert!(!text.contains("stdout"));
        assert!(!text.contains("stderr"));
        assert!(!text.contains("env"));
    }

    #[test]
    fn friction_retention_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let store = Store::new(state.path().join("friction")).with_max_events(3);
        let session = session(root.path(), "retention");
        for _ in 0..8 {
            store
                .record(
                    &session,
                    FrictionKind::ExecuteFailed,
                    ObservationSource::Observed,
                    Some("execute"),
                    Some("execute"),
                    EventOutcome::Failed,
                    None,
                    None,
                )
                .unwrap();
        }
        assert_eq!(store.read_all().unwrap().len(), 3);
    }

    #[test]
    fn persisted_scope_validation_is_existence_independent_and_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let canonical = config::canonical_directory(root.path()).unwrap();
        validate_persisted_scope(&canonical).unwrap();
        drop(root);
        assert!(!canonical.exists());
        validate_persisted_scope(&canonical).unwrap();

        for invalid in [
            PathBuf::from("relative/scope"),
            PathBuf::from("/tmp/../escape"),
            PathBuf::from("/tmp/./scope"),
            PathBuf::from("/tmp//scope"),
        ] {
            assert!(validate_persisted_scope(&invalid).is_err(), "{invalid:?}");
        }
        let oversized = PathBuf::from(format!("/{}", "x".repeat(MAX_SCOPE_PATH_BYTES)));
        assert!(validate_persisted_scope(&oversized).is_err());
    }

    #[test]
    fn generated_persisted_scope_validation_matches_normalized_absolute_path_model()
    -> noprop::TestResult {
        crate::test_support::run(0x4652_4943_5449_4f4e, 1024, |ctx| {
            let suffix = format!("scope-{:016x}", noprop::sample_u64(ctx));
            let canonical = PathBuf::from(format!("/tmp/{suffix}"));
            assert!(validate_persisted_scope(&canonical).is_ok());

            let invalid = match noprop::sample_usize_in(ctx, 0..=3) {
                0 => PathBuf::from(format!("tmp/{suffix}")),
                1 => PathBuf::from(format!("/tmp/../{suffix}")),
                2 => PathBuf::from(format!("/tmp/./{suffix}")),
                _ => PathBuf::from(format!("/tmp//{suffix}")),
            };
            assert!(validate_persisted_scope(&invalid).is_err());
            Ok(())
        })
    }

    #[test]
    fn persisted_event_integrity_checks_remain_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "integrity");

        let public_state = tempfile::tempdir().unwrap();
        let public_store = Store::new(public_state.path().join("friction"));
        let public_event = public_store
            .record(
                &session,
                FrictionKind::ExecuteFailed,
                ObservationSource::Observed,
                Some("execute"),
                Some("execute"),
                EventOutcome::Failed,
                None,
                None,
            )
            .unwrap();
        let public_path = public_store
            .directory
            .join(format!("{}.json", public_event.event_id));
        std::fs::set_permissions(&public_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(public_store.read_all().is_err());

        let mismatch_state = tempfile::tempdir().unwrap();
        let mismatch_store = Store::new(mismatch_state.path().join("friction"));
        let mismatch_event = mismatch_store
            .record(
                &session,
                FrictionKind::ExecuteFailed,
                ObservationSource::Observed,
                Some("execute"),
                Some("execute"),
                EventOutcome::Failed,
                None,
                None,
            )
            .unwrap();
        let mismatch_path = mismatch_store
            .directory
            .join(format!("{}.json", mismatch_event.event_id));
        let mut mismatched = mismatch_event.clone();
        mismatched.event_id = Uuid::new_v4();
        std::fs::write(
            &mismatch_path,
            serde_json::to_vec_pretty(&mismatched).unwrap(),
        )
        .unwrap();
        assert!(mismatch_store.read_all().is_err());

        let symlink_state = tempfile::tempdir().unwrap();
        let symlink_store = Store::new(symlink_state.path().join("friction"));
        let symlink_event = symlink_store
            .record(
                &session,
                FrictionKind::ExecuteFailed,
                ObservationSource::Observed,
                Some("execute"),
                Some("execute"),
                EventOutcome::Failed,
                None,
                None,
            )
            .unwrap();
        let symlink_path = symlink_store
            .directory
            .join(format!("{}.json", symlink_event.event_id));
        let target = symlink_state.path().join("target.json");
        std::fs::rename(&symlink_path, &target).unwrap();
        std::os::unix::fs::symlink(&target, &symlink_path).unwrap();
        assert!(symlink_store.read_all().is_err());
    }
}
