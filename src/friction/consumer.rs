//! Independently fenced, bounded friction candidate consumer.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{FrictionEvent, FrictionKind, SourceInstance, Store as EventStore};
use crate::config;
use crate::observation::{ListFilter, Observation, ObservationKind, ObservationStore};

const SCHEMA: u32 = 1;
const BATCH: usize = 128;
const MAX_FILE: usize = 24 * 1024;
const MAX_SUPPORT: usize = 16;
const MAX_CRITERIA: usize = 4;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProducerFence {
    pub consumer_id: String,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Classification {
    TemoteFriction,
    TargetRepositoryBug,
    UpstreamTransient,
    InsufficientEvidence,
    KnownExistingIssue,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EvidenceAssessment {
    pub known_issue_match: bool,
    pub verified_target_reproduction: bool,
    pub verified_upstream_transient: bool,
    pub temote_reconciliation_count: u32,
}

pub(crate) fn classify(evidence: EvidenceAssessment) -> Classification {
    if evidence.known_issue_match {
        Classification::KnownExistingIssue
    } else if evidence.verified_target_reproduction {
        Classification::TargetRepositoryBug
    } else if evidence.verified_upstream_transient {
        Classification::UpstreamTransient
    } else if evidence.temote_reconciliation_count >= 2 {
        Classification::TemoteFriction
    } else {
        Classification::InsufficientEvidence
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CandidateStatus {
    Retained,
    Eligible,
    Published,
    Blocked,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservationRef {
    pub id: Uuid,
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Candidate {
    pub schema_version: u32,
    pub id: Uuid,
    pub fingerprint: String,
    pub status: CandidateStatus,
    pub scope: SourceInstance,
    pub classification: Classification,
    #[serde(default)]
    pub known_issue_ref: Option<String>,
    pub summary: String,
    pub impact: String,
    pub expected_behavior: String,
    pub resolution_hypothesis: String,
    pub acceptance_criteria: Vec<String>,
    pub support_observation_refs: Vec<ObservationRef>,
    pub support_friction_event_refs: Vec<Uuid>,
    pub facts: Vec<String>,
    pub hypotheses: Vec<String>,
    pub producer: ProducerFence,
    pub produced_at: u64,
    pub recurrence: u32,
    #[serde(default)]
    pub last_counted_revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    schema_version: u32,
    source: SourceInstance,
    producer: ProducerFence,
    through_revision: u64,
    updated_at: u64,
    #[serde(default)]
    degraded_events: bool,
}

pub(crate) struct Consumer {
    directory: PathBuf,
    observations: ObservationStore,
    events: EventStore,
}

pub(crate) struct SourceBatch {
    pub session_id: String,
    pub candidates: Vec<Candidate>,
    pub degraded: bool,
}

impl Consumer {
    /// Each source retains its own checkpoint and failure boundary. A failed
    /// source cannot prevent the following source from advancing.
    pub(crate) fn consume_sources(
        &self,
        sessions: &[config::Session],
        producer: &ProducerFence,
    ) -> Result<Vec<SourceBatch>> {
        ensure!(
            !sessions.is_empty() && sessions.len() <= 16,
            "expected 1..=16 friction sources"
        );
        Ok(sessions
            .iter()
            .map(|session| match self.consume(session, producer) {
                Ok(candidates) => SourceBatch {
                    session_id: session.id.clone(),
                    candidates,
                    degraded: false,
                },
                Err(_) => SourceBatch {
                    session_id: session.id.clone(),
                    candidates: Vec::new(),
                    degraded: true,
                },
            })
            .collect())
    }
    pub(crate) fn new(
        directory: PathBuf,
        observations: ObservationStore,
        events: EventStore,
    ) -> Self {
        Self {
            directory,
            observations,
            events,
        }
    }

    pub(crate) fn default_consumer() -> Result<Self> {
        Ok(Self::new(
            config::state_dir()?.join("friction-consumer"),
            ObservationStore::default_store()?,
            EventStore::default_store()?,
        ))
    }

    pub(crate) fn load_candidate(&self, fingerprint: &str) -> Result<Candidate> {
        ensure!(
            fingerprint.len() == 64 && fingerprint.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid candidate fingerprint"
        );
        private_dir(&self.directory)?;
        private_dir(&self.directory.join("candidates"))?;
        let path = self
            .directory
            .join("candidates")
            .join(format!("{fingerprint}.json"));
        let candidate: Candidate =
            read_optional(&path)?.ok_or_else(|| anyhow::anyhow!("friction candidate not found"))?;
        validate_candidate(&candidate)?;
        ensure!(
            candidate.fingerprint == fingerprint,
            "candidate fingerprint mismatch"
        );
        Ok(candidate)
    }

    /// Exact local-issue index seam. A semantic suggestion is not enough:
    /// the caller must supply a verified scoped `issues/open/` identity.
    pub(crate) fn mark_known_issue(
        &self,
        fingerprint: &str,
        issue_ref: &str,
        producer: &ProducerFence,
    ) -> Result<Candidate> {
        let mut candidate = self.load_candidate(fingerprint)?;
        ensure!(
            producer.generation >= candidate.producer.generation,
            "stale friction producer"
        );
        ensure!(
            producer.generation != candidate.producer.generation
                || producer.consumer_id == candidate.producer.consumer_id,
            "friction producer identity conflict"
        );
        candidate.classification = Classification::KnownExistingIssue;
        candidate.known_issue_ref = Some(issue_ref.to_owned());
        candidate.status = if candidate.recurrence >= 2 {
            CandidateStatus::Eligible
        } else {
            CandidateStatus::Retained
        };
        candidate.producer = producer.clone();
        candidate.produced_at = config::unix_time();
        validate_candidate(&candidate)?;
        let _lock = FileLock::new(&self.directory.join(".lock"))?;
        let path = self
            .directory
            .join("candidates")
            .join(format!("{fingerprint}.json"));
        let current: Candidate = read_optional(&path)?
            .ok_or_else(|| anyhow::anyhow!("friction candidate disappeared"))?;
        ensure!(
            current.id == candidate.id
                && current.producer.generation <= producer.generation
                && current.recurrence == candidate.recurrence
                && current.support_observation_refs == candidate.support_observation_refs,
            "friction candidate changed during issue match"
        );
        atomic_json(&path, &candidate)?;
        Ok(candidate)
    }

    /// Failure is returned to the independent worker, never to a coding task.
    /// Callers may continue scanning other sessions after one source fails.
    pub(crate) fn consume(
        &self,
        session: &config::Session,
        producer: &ProducerFence,
    ) -> Result<Vec<Candidate>> {
        ensure!(
            !producer.consumer_id.is_empty()
                && producer.consumer_id.len() <= 64
                && producer
                    .consumer_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid friction consumer ID"
        );
        ensure!(
            producer.generation > 0,
            "friction producer generation is required"
        );
        let source = SourceInstance::of(session)?;
        private_dir(&self.directory)?;
        private_dir(&self.directory.join("checkpoints"))?;
        private_dir(&self.directory.join("candidates"))?;
        let _lock = FileLock::new(&self.directory.join(".lock"))?;
        let source_key = digest(&serde_json::to_vec(&source)?);
        let checkpoint_path = self
            .directory
            .join("checkpoints")
            .join(format!("{source_key}.json"));
        let previous: Option<Checkpoint> = read_optional(&checkpoint_path)?;
        if let Some(cp) = &previous {
            ensure!(
                cp.schema_version == SCHEMA && cp.source == source,
                "friction checkpoint scope mismatch"
            );
            ensure!(
                producer.generation >= cp.producer.generation,
                "stale friction producer"
            );
            ensure!(
                producer.generation != cp.producer.generation
                    || producer.consumer_id == cp.producer.consumer_id,
                "friction producer identity conflict"
            );
        }
        let cursor = previous.as_ref().map_or(0, |cp| cp.through_revision);
        let (all, corrupt) = self.observations.list(
            &session.id,
            &ListFilter {
                after_revision: Some(cursor),
                ..ListFilter::default()
            },
        )?;
        ensure!(corrupt == 0, "degraded friction observation source");
        let scanned: Vec<&Observation> = all.iter().take(BATCH).collect();
        let page: Vec<&Observation> = scanned
            .iter()
            .copied()
            .filter(|obs| {
                obs.session_instance.started_at == source.started_at
                    && obs.session_instance.process_id == source.process_id
            })
            .collect();
        let (events, degraded_events) = match self.events.read_all() {
            Ok(events) => (events, false),
            Err(_) => (Vec::new(), true),
        };
        let events: Vec<&FrictionEvent> = events
            .iter()
            .filter(|event| event.session_instance.as_ref() == Some(&source))
            .take(BATCH)
            .collect();
        let mut candidates = Vec::new();
        let mut groups: std::collections::BTreeMap<String, Vec<&Observation>> =
            std::collections::BTreeMap::new();
        for obs in &page {
            if obs.kind == ObservationKind::Reconciliation {
                let key = format!("reconciliation:{}:{}", obs.target.backend, obs.action);
                groups.entry(key).or_default().push(obs);
            }
        }
        for (signal, observations) in groups {
            let fingerprint = digest(&serde_json::to_vec(&(&source, &signal))?);
            let path = self
                .directory
                .join("candidates")
                .join(format!("{fingerprint}.json"));
            let old: Option<Candidate> = read_optional(&path)?;
            let mut refs = old
                .as_ref()
                .map_or_else(Vec::new, |c| c.support_observation_refs.clone());
            let last_counted_revision = old.as_ref().map_or(0, |c| c.last_counted_revision);
            let mut new_count = 0_u32;
            let mut latest_counted_revision = last_counted_revision;
            for obs in observations {
                if obs.revision > last_counted_revision {
                    new_count = new_count.saturating_add(1);
                    latest_counted_revision = latest_counted_revision.max(obs.revision);
                }
                if !refs.iter().any(|r| r.id == obs.id) {
                    refs.push(ObservationRef {
                        id: obs.id,
                        revision: obs.revision,
                    });
                    if refs.len() > MAX_SUPPORT {
                        refs.remove(0);
                    }
                }
            }
            let recurrence = old
                .as_ref()
                .map_or(0, |c| c.recurrence)
                .saturating_add(new_count);
            let classified = classify(EvidenceAssessment {
                temote_reconciliation_count: recurrence,
                ..EvidenceAssessment::default()
            });
            let classified = if old
                .as_ref()
                .is_some_and(|c| c.classification == Classification::KnownExistingIssue)
            {
                Classification::KnownExistingIssue
            } else {
                classified
            };
            let mut event_refs = old
                .as_ref()
                .map_or_else(Vec::new, |c| c.support_friction_event_refs.clone());
            for event in &events {
                if matches!(
                    event.kind,
                    FrictionKind::AmbiguousMutationDetected | FrictionKind::ClientOperationRetried
                ) && !event_refs.contains(&event.event_id)
                    && event_refs.len() < MAX_SUPPORT
                {
                    event_refs.push(event.event_id);
                }
            }
            let candidate = Candidate {
                schema_version: SCHEMA,
                id: old.as_ref().map_or_else(Uuid::new_v4, |c| c.id),
                fingerprint,
                status: if matches!(
                    classified,
                    Classification::TemoteFriction | Classification::KnownExistingIssue
                ) && !degraded_events
                    && recurrence >= 2
                {
                    CandidateStatus::Eligible
                } else {
                    CandidateStatus::Retained
                },
                scope: source.clone(),
                classification: classified,
                known_issue_ref: old.as_ref().and_then(|c| c.known_issue_ref.clone()),
                summary: "Repeated Temote reconciliation for one backend operation".into(),
                impact:
                    "The delegated operation required reconciliation before its outcome was known."
                        .into(),
                expected_behavior: "A delegated operation has a bounded, reconcilable outcome."
                    .into(),
                resolution_hypothesis:
                    "Inspect the referenced operation transitions and remove unnecessary ambiguity."
                        .into(),
                acceptance_criteria: vec![
                    "Repeated operation replay preserves one accepted side effect.".into(),
                    "An uncertain response reconciles against the retained task before retry."
                        .into(),
                ],
                support_observation_refs: refs,
                support_friction_event_refs: event_refs,
                facts: vec![format!(
                    "{} distinct reconciliation observations in this session instance",
                    recurrence
                )],
                hypotheses: vec![
                    "The repeated reconciliation may indicate a Temote workflow defect.".into(),
                ],
                producer: producer.clone(),
                produced_at: config::unix_time(),
                recurrence,
                last_counted_revision: latest_counted_revision,
            };
            validate_candidate(&candidate)?;
            atomic_json(&path, &candidate)?;
            candidates.push(candidate);
        }
        let through = scanned.last().map_or(cursor, |obs| obs.revision);
        if through > cursor
            || previous
                .as_ref()
                .is_none_or(|cp| cp.degraded_events != degraded_events || cp.producer != *producer)
        {
            atomic_json(
                &checkpoint_path,
                &Checkpoint {
                    schema_version: SCHEMA,
                    source,
                    producer: producer.clone(),
                    through_revision: through,
                    updated_at: config::unix_time(),
                    degraded_events,
                },
            )?;
        }
        Ok(candidates)
    }
}

pub(crate) fn validate_candidate(c: &Candidate) -> Result<()> {
    ensure!(c.schema_version == SCHEMA, "unsupported candidate schema");
    crate::host_identity::validate(&c.scope.host_id)?;
    config::validate_session_id(&c.scope.session_id)?;
    super::validate_persisted_scope(&c.scope.scope_cwd)?;
    ensure!(
        c.scope.started_at > 0 && c.scope.process_id > 0,
        "candidate has incomplete session fence"
    );
    ensure!(
        c.fingerprint.len() == 64 && c.fingerprint.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid candidate fingerprint"
    );
    ensure!(
        c.support_observation_refs.len() <= MAX_SUPPORT
            && c.support_friction_event_refs.len() <= MAX_SUPPORT
            && c.acceptance_criteria.len() <= MAX_CRITERIA,
        "candidate exceeds reference bounds"
    );
    let unique: BTreeSet<Uuid> = c.support_observation_refs.iter().map(|r| r.id).collect();
    ensure!(
        unique.len() == c.support_observation_refs.len(),
        "duplicate candidate support"
    );
    for text in [
        &c.summary,
        &c.impact,
        &c.expected_behavior,
        &c.resolution_hypothesis,
    ]
    .into_iter()
    .chain(c.acceptance_criteria.iter())
    .chain(c.facts.iter())
    .chain(c.hypotheses.iter())
    {
        ensure!(
            text.len() <= 512 && !text.contains('\0'),
            "candidate text exceeds bound"
        );
    }
    ensure!(
        c.facts.len() <= 8 && c.hypotheses.len() <= 8,
        "candidate facts exceed bound"
    );
    ensure!(
        c.classification != Classification::TemoteFriction
            || !c.support_observation_refs.is_empty(),
        "Temote friction requires observation support"
    );
    if let Some(reference) = &c.known_issue_ref {
        ensure!(
            reference.starts_with("issues/open/")
                && reference.ends_with(".md")
                && reference.len() <= 160
                && !reference["issues/open/".len()..].contains('/')
                && !reference.contains("..")
                && reference
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.')),
            "invalid known issue reference"
        );
    }
    ensure!(
        (c.classification == Classification::KnownExistingIssue) == c.known_issue_ref.is_some(),
        "known issue classification and exact scoped reference must agree"
    );
    Ok(())
}

pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.file_type().is_dir()
            && meta.permissions().mode() & 0o077 == 0
            && meta.uid() == unsafe { libc::geteuid() },
        "friction state directory is not owner-only"
    );
    Ok(())
}

pub(super) fn read_optional<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.len() <= MAX_FILE as u64
            && meta.permissions().mode() & 0o077 == 0
            && meta.uid() == unsafe { libc::geteuid() },
        "friction state file is not bounded and owner-only"
    );
    let mut bytes = Vec::new();
    file.take((MAX_FILE + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_FILE, "friction state file exceeds limit");
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub(super) fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_FILE, "friction state exceeds limit");
    let temp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("missing friction parent"))?,
        )?
        .sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

pub(super) struct FileLock {
    file: File,
}
impl FileLock {
    pub(super) fn new(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let meta = file.metadata()?;
        ensure!(
            meta.permissions().mode() & 0o077 == 0 && meta.uid() == unsafe { libc::geteuid() },
            "friction lock is not owner-only"
        );
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "friction lock failed"
        );
        Ok(Self { file })
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::{
        ActorRef, ObservationContent, Provenance, SessionInstanceRef, TargetRef,
    };

    fn session(cwd: &Path, started_at: u64) -> config::Session {
        config::Session {
            id: Uuid::new_v4().to_string(),
            cwd: cwd.to_path_buf(),
            permitted_directories: vec![cwd.to_path_buf()],
            started_at,
            process_id: 42,
            permission_mode: config::PermissionMode::Agent,
            grants: config::SessionGrants::default(),
        }
    }

    fn observed(session: &config::Session, key: &str) -> Observation {
        Observation {
            id: Uuid::new_v4(),
            schema_version: 1,
            observed_at: 1,
            accepted_at: None,
            session_id: session.id.clone(),
            session_instance: SessionInstanceRef {
                started_at: session.started_at,
                process_id: session.process_id,
            },
            repository: None,
            repository_key: None,
            workspace_id: None,
            task_id: Some("task_1".into()),
            execution_id: None,
            operation_id: None,
            actor: ActorRef {
                transport: "test".into(),
                principal: None,
            },
            target: TargetRef {
                backend: "codex".into(),
            },
            action: "task_start".into(),
            kind: ObservationKind::Reconciliation,
            content: ObservationContent::None,
            state_ref: None,
            evidence_refs: vec![],
            provenance: Provenance {
                tool: "codex_task_start".into(),
                source: "orchestration".into(),
                control_action: None,
            },
            revision: 0,
            dedupe_key: key.into(),
        }
    }

    #[test]
    fn classification_requires_verified_discriminants() {
        assert_eq!(
            classify(EvidenceAssessment::default()),
            Classification::InsufficientEvidence
        );
        assert_eq!(
            classify(EvidenceAssessment {
                verified_target_reproduction: true,
                ..EvidenceAssessment::default()
            }),
            Classification::TargetRepositoryBug
        );
        assert_eq!(
            classify(EvidenceAssessment {
                verified_upstream_transient: true,
                ..EvidenceAssessment::default()
            }),
            Classification::UpstreamTransient
        );
        assert_eq!(
            classify(EvidenceAssessment {
                known_issue_match: true,
                temote_reconciliation_count: 3,
                ..EvidenceAssessment::default()
            }),
            Classification::KnownExistingIssue
        );
    }

    #[test]
    fn degraded_source_does_not_block_another_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let scope = fs::canonicalize(root.path()).unwrap();
        let broken = session(&scope, 10);
        let healthy = session(&scope, 20);
        let observations = ObservationStore::new(root.path().join("observations"));
        for source in [&broken, &healthy] {
            observations.append(observed(source, "one")).unwrap();
            observations.append(observed(source, "two")).unwrap();
        }
        let consumer = Consumer::new(
            root.path().join("consumer"),
            ObservationStore::new(root.path().join("observations")),
            EventStore::new(root.path().join("events")),
        );
        let producer = ProducerFence {
            consumer_id: "worker-1".into(),
            generation: 1,
        };
        let first = consumer
            .consume_sources(&[broken.clone(), healthy.clone()], &producer)
            .unwrap();
        assert!(
            first
                .iter()
                .all(|source| !source.degraded && source.candidates.len() == 1)
        );
        let broken_key =
            digest(&serde_json::to_vec(&SourceInstance::of(&broken).unwrap()).unwrap());
        fs::write(
            consumer
                .directory
                .join("checkpoints")
                .join(format!("{broken_key}.json")),
            b"invalid checkpoint",
        )
        .unwrap();
        observations.append(observed(&healthy, "three")).unwrap();
        let next = consumer
            .consume_sources(&[broken.clone(), healthy.clone()], &producer)
            .unwrap();
        assert!(next[0].degraded && next[0].candidates.is_empty());
        assert!(!next[1].degraded);
        assert_eq!(next[1].candidates[0].recurrence, 3);
        assert_eq!(next[1].candidates[0].id, first[1].candidates[0].id);
        let restarted = Consumer::new(
            consumer.directory.clone(),
            observations,
            EventStore::new(root.path().join("events")),
        );
        let replay = restarted
            .consume_sources(&[broken, healthy], &producer)
            .unwrap();
        assert!(replay[0].degraded);
        assert!(!replay[1].degraded && replay[1].candidates.is_empty());
    }

    #[test]
    fn replay_and_replacement_are_fenced() {
        let root = tempfile::tempdir().unwrap();
        let scope = fs::canonicalize(root.path()).unwrap();
        let mut session = session(&scope, 10);
        let observation_dir = root.path().join("observations");
        let observation_store = ObservationStore::new(observation_dir.clone());
        observation_store.append(observed(&session, "one")).unwrap();
        observation_store.append(observed(&session, "two")).unwrap();
        let consumer = Consumer::new(
            root.path().join("consumer"),
            ObservationStore::new(observation_dir.clone()),
            EventStore::new(root.path().join("events")),
        );
        let producer = ProducerFence {
            consumer_id: "worker-1".into(),
            generation: 1,
        };
        let first = consumer.consume(&session, &producer).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].classification, Classification::TemoteFriction);
        assert_eq!(first[0].recurrence, 2);
        let checkpoint_path = fs::read_dir(root.path().join("consumer/checkpoints"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut checkpoint: Checkpoint = read_optional(&checkpoint_path).unwrap().unwrap();
        checkpoint.through_revision = 0;
        atomic_json(&checkpoint_path, &checkpoint).unwrap();
        let replay = consumer.consume(&session, &producer).unwrap();
        assert_eq!(replay[0].id, first[0].id);
        assert_eq!(replay[0].recurrence, 2);
        let restarted = Consumer::new(
            root.path().join("consumer"),
            ObservationStore::new(observation_dir.clone()),
            EventStore::new(root.path().join("events")),
        );
        assert!(restarted.consume(&session, &producer).unwrap().is_empty());
        let known = consumer
            .mark_known_issue(
                &first[0].fingerprint,
                "issues/open/20261006-recurrence.md",
                &producer,
            )
            .unwrap();
        assert_eq!(known.classification, Classification::KnownExistingIssue);
        observation_store
            .append(observed(&session, "three"))
            .unwrap();
        let recurring = consumer.consume(&session, &producer).unwrap();
        assert_eq!(
            recurring[0].classification,
            Classification::KnownExistingIssue
        );
        assert_eq!(recurring[0].recurrence, 3);
        let stale = ProducerFence {
            consumer_id: "worker-1".into(),
            generation: 0,
        };
        assert!(consumer.consume(&session, &stale).is_err());
        session.started_at = 11;
        assert!(consumer.consume(&session, &producer).unwrap().is_empty());
        observation_store
            .append(observed(&session, "new-instance"))
            .unwrap();
        let replacement = consumer.consume(&session, &producer).unwrap();
        assert_eq!(replacement.len(), 1);
        assert_eq!(replacement[0].recurrence, 1);
        assert_ne!(replacement[0].id, first[0].id);
    }
}
