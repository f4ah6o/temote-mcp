use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config;

const FRICTION_SCHEMA_VERSION: u64 = 1;
const MAX_EVENT_BYTES: usize = 16 * 1024;
const MAX_EVENTS: usize = 512;
const MAX_DIRECTORY_ENTRIES: usize = 4096;
const MAX_IDENTIFIER_BYTES: usize = 64;
const CANDIDATE_THRESHOLD: u64 = 4;
const CANDIDATE_NAMESPACE: Uuid = Uuid::from_bytes([
    0x16, 0xba, 0x3a, 0x2a, 0xf5, 0xdf, 0x4a, 0xb5, 0xb4, 0x98, 0x21, 0x0f, 0x87, 0xd4, 0xc4, 0x63,
]);

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

impl FrictionKind {
    fn weight(self) -> u64 {
        match self {
            Self::ApprovalDenied => 2,
            Self::SandboxDenied | Self::PathEscapeRejected => 2,
            Self::ExecuteFailed | Self::JobFailed | Self::GitOperationFailed => 2,
            Self::JobCancelled => 1,
            Self::SessionCrashed | Self::AmbiguousMutationDetected => 4,
            Self::IntegrationFailed => 2,
            Self::ClientOperationRetried => 1,
            Self::RecallMiss => 0,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ApprovalDenied => "approval_denied",
            Self::SandboxDenied => "sandbox_denied",
            Self::PathEscapeRejected => "path_escape_rejected",
            Self::ExecuteFailed => "execute_failed",
            Self::JobFailed => "job_failed",
            Self::JobCancelled => "job_cancelled",
            Self::SessionCrashed => "session_crashed",
            Self::GitOperationFailed => "git_operation_failed",
            Self::IntegrationFailed => "integration_failed",
            Self::ClientOperationRetried => "client_operation_retried",
            Self::AmbiguousMutationDetected => "ambiguous_mutation_detected",
            Self::RecallMiss => "recall_miss",
        }
    }
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

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct FrictionReason {
    pub kind: String,
    pub count: usize,
    pub effective_count: usize,
    pub contribution: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct FrictionSummary {
    pub session_id: String,
    pub scope_cwd: PathBuf,
    pub event_count: usize,
    pub observed_count: usize,
    pub client_reported_count: usize,
    pub score: u64,
    pub candidate: bool,
    pub reasons: Vec<FrictionReason>,
    pub knowledge_gap_bonus: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct LearningCandidate {
    pub candidate_id: Uuid,
    pub session_id: String,
    pub scope_cwd: PathBuf,
    pub created_at: u64,
    pub status: String,
    pub friction_summary: FrictionSummary,
    pub related_checkpoints: Vec<Uuid>,
    pub authoritative_learning_created: bool,
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

    pub(crate) fn events_for_session(
        &self,
        session: &config::Session,
    ) -> Result<Vec<FrictionEvent>> {
        validate_session_scope(session)?;
        let mut events = self.read_all()?;
        events.retain(|event| event.scope_cwd == session.cwd && event.session_id == session.id);
        events.sort_by_key(|event| (event.occurred_at, event.event_id));
        Ok(events)
    }

    pub(crate) fn summary(&self, session: &config::Session) -> Result<FrictionSummary> {
        let events = self.events_for_session(session)?;
        Ok(score_events(session, &events))
    }

    pub(crate) fn candidates(&self, session: &config::Session) -> Result<Vec<LearningCandidate>> {
        let events = self.events_for_session(session)?;
        let summary = score_events(session, &events);
        if !summary.candidate {
            return Ok(Vec::new());
        }
        let created_at = events
            .iter()
            .map(|event| event.occurred_at)
            .max()
            .unwrap_or(0);
        let mut related_checkpoints = events
            .iter()
            .filter_map(|event| event.related_checkpoint)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        related_checkpoints.sort_by_key(Uuid::to_string);
        let candidate_id = candidate_id(session);
        Ok(vec![LearningCandidate {
            candidate_id,
            session_id: session.id.clone(),
            scope_cwd: session.cwd.clone(),
            created_at,
            status: "candidate".to_owned(),
            friction_summary: summary,
            related_checkpoints,
            authoritative_learning_created: false,
        }])
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

pub(crate) async fn record_observed_for_session_id(
    session_id: &str,
    kind: FrictionKind,
    operation_class: Option<&str>,
    tool_name: Option<&str>,
    outcome: EventOutcome,
) {
    if let Ok(session) = config::load_session(session_id).await {
        record_observed(
            &session,
            kind,
            operation_class,
            tool_name,
            outcome,
            None,
            None,
        );
    }
}

pub(crate) fn record_client_reported_recall_miss(
    session: &config::Session,
    retry_group: Option<Uuid>,
) -> Result<FrictionEvent> {
    Store::default_store()?.record(
        session,
        FrictionKind::RecallMiss,
        ObservationSource::ClientReported,
        Some("recall"),
        Some("recall_feedback"),
        EventOutcome::NoHit,
        retry_group,
        None,
    )
}

fn score_events(session: &config::Session, events: &[FrictionEvent]) -> FrictionSummary {
    let mut counts = BTreeMap::<FrictionKind, usize>::new();
    let mut observed_count = 0usize;
    let mut client_reported_count = 0usize;
    for event in events {
        *counts.entry(event.kind).or_default() += 1;
        match event.source {
            ObservationSource::Observed => observed_count += 1,
            ObservationSource::ClientReported => client_reported_count += 1,
        }
    }

    let mut score = 0u64;
    let mut reasons = Vec::new();
    for (kind, count) in counts {
        let effective_count = count.min(3);
        let contribution = kind.weight() * effective_count as u64;
        score = score.saturating_add(contribution);
        reasons.push(FrictionReason {
            kind: kind.as_str().to_owned(),
            count,
            effective_count,
            contribution,
        });
    }
    let has_recall_miss = events
        .iter()
        .any(|event| event.kind == FrictionKind::RecallMiss);
    let knowledge_gap_bonus = u64::from(has_recall_miss && score > 0);
    score = score.saturating_add(knowledge_gap_bonus);
    FrictionSummary {
        session_id: session.id.clone(),
        scope_cwd: session.cwd.clone(),
        event_count: events.len(),
        observed_count,
        client_reported_count,
        score,
        candidate: score >= CANDIDATE_THRESHOLD,
        reasons,
        knowledge_gap_bonus,
    }
}

fn candidate_id(session: &config::Session) -> Uuid {
    let mut bytes = session.cwd.to_string_lossy().as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(session.id.as_bytes());
    Uuid::new_v5(&CANDIDATE_NAMESPACE, &bytes)
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
    let canonical = config::canonical_directory(&event.scope_cwd)?;
    anyhow::ensure!(
        canonical == event.scope_cwd,
        "friction scope is not canonical"
    );
    validate_optional_identifier(event.operation_class.as_deref(), "operation_class")?;
    validate_optional_identifier(event.tool_name.as_deref(), "tool_name")?;
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
            yolo: true,
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
    fn friction_event_distinguishes_observed_from_client_reported() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let store = Store::new(state.path().join("friction"));
        let session = session(root.path(), "sources");
        store
            .record(
                &session,
                FrictionKind::ApprovalDenied,
                ObservationSource::Observed,
                Some("approval"),
                Some("write_file"),
                EventOutcome::Denied,
                None,
                None,
            )
            .unwrap();
        store
            .record(
                &session,
                FrictionKind::RecallMiss,
                ObservationSource::ClientReported,
                Some("recall"),
                Some("recall_feedback"),
                EventOutcome::NoHit,
                None,
                None,
            )
            .unwrap();
        let summary = store.summary(&session).unwrap();
        assert_eq!(summary.observed_count, 1);
        assert_eq!(summary.client_reported_count, 1);
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
    fn clean_high_tool_count_session_does_not_trigger_candidate() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "clean");
        let summary = score_events(&session, &[]);
        assert_eq!(summary.score, 0);
        assert!(!summary.candidate);
    }

    #[test]
    fn repeated_same_root_failure_is_bounded_and_explainable() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "bounded-score");
        let mut events = Vec::new();
        for _ in 0..100 {
            events.push(FrictionEvent {
                schema_version: FRICTION_SCHEMA_VERSION,
                event_id: Uuid::new_v4(),
                session_id: session.id.clone(),
                scope_cwd: session.cwd.clone(),
                occurred_at: 1,
                kind: FrictionKind::ExecuteFailed,
                source: ObservationSource::Observed,
                operation_class: Some("execute".to_owned()),
                tool_name: Some("execute".to_owned()),
                outcome: EventOutcome::Failed,
                retry_group: None,
                related_checkpoint: None,
            });
        }
        let summary = score_events(&session, &events);
        assert_eq!(summary.score, 6);
        assert_eq!(summary.reasons[0].count, 100);
        assert_eq!(summary.reasons[0].effective_count, 3);
        assert_eq!(summary.reasons[0].contribution, 6);
    }

    #[test]
    fn approval_denial_contributes_without_recording_approval_body() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let store = Store::new(state.path().join("friction"));
        let session = session(root.path(), "approval");
        store
            .record(
                &session,
                FrictionKind::ApprovalDenied,
                ObservationSource::Observed,
                Some("approval"),
                Some("git_push"),
                EventOutcome::Denied,
                None,
                None,
            )
            .unwrap();
        let serialized = std::fs::read_to_string(
            std::fs::read_dir(&store.directory)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .find(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(!serialized.contains("approval body sentinel"));
        assert_eq!(store.summary(&session).unwrap().score, 2);
    }

    #[test]
    fn ambiguous_mutation_is_high_value_friction() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "ambiguous");
        let event = FrictionEvent {
            schema_version: FRICTION_SCHEMA_VERSION,
            event_id: Uuid::new_v4(),
            session_id: session.id.clone(),
            scope_cwd: session.cwd.clone(),
            occurred_at: 1,
            kind: FrictionKind::AmbiguousMutationDetected,
            source: ObservationSource::Observed,
            operation_class: Some("filesystem".to_owned()),
            tool_name: Some("apply_patch".to_owned()),
            outcome: EventOutcome::Ambiguous,
            retry_group: None,
            related_checkpoint: None,
        };
        let summary = score_events(&session, &[event]);
        assert!(summary.candidate);
        assert_eq!(summary.score, 4);
    }

    #[test]
    fn recall_no_hit_alone_does_not_create_candidate_but_can_bonus_real_friction() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "gap");
        let miss = FrictionEvent {
            schema_version: FRICTION_SCHEMA_VERSION,
            event_id: Uuid::new_v4(),
            session_id: session.id.clone(),
            scope_cwd: session.cwd.clone(),
            occurred_at: 1,
            kind: FrictionKind::RecallMiss,
            source: ObservationSource::ClientReported,
            operation_class: Some("recall".to_owned()),
            tool_name: Some("recall_feedback".to_owned()),
            outcome: EventOutcome::NoHit,
            retry_group: None,
            related_checkpoint: None,
        };
        let only_miss = score_events(&session, std::slice::from_ref(&miss));
        assert_eq!(only_miss.score, 0);
        assert!(!only_miss.candidate);
        let mut failure = miss.clone();
        failure.event_id = Uuid::new_v4();
        failure.kind = FrictionKind::ExecuteFailed;
        failure.source = ObservationSource::Observed;
        failure.outcome = EventOutcome::Failed;
        let combined = score_events(&session, &[miss, failure]);
        assert_eq!(combined.knowledge_gap_bonus, 1);
        assert_eq!(combined.score, 3);
    }

    #[test]
    fn candidate_does_not_become_learning_automatically_or_copy_transcript() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let store = Store::new(state.path().join("friction"));
        let session = session(root.path(), "candidate");
        let checkpoint = Uuid::new_v4();
        store
            .record(
                &session,
                FrictionKind::AmbiguousMutationDetected,
                ObservationSource::Observed,
                Some("filesystem"),
                Some("apply_patch"),
                EventOutcome::Ambiguous,
                None,
                Some(checkpoint),
            )
            .unwrap();
        let candidates = store.candidates(&session).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].authoritative_learning_created);
        assert_eq!(candidates[0].related_checkpoints, vec![checkpoint]);
        let serialized = serde_json::to_string(&candidates[0]).unwrap();
        assert!(!serialized.contains("transcript"));
        assert!(!serialized.contains("stdout"));
    }
}
