use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{checkpoints, config, mcp, recall};

const HANDOFF_VERSION: u64 = 1;
const MAX_HANDOFF_BYTES: usize = 1024 * 1024;
const MAX_HANDOFF_JOBS: usize = 128;
const MAX_HANDOFF_RECALL_HITS: usize = 5;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkHandoffRequest {
    pub session_id: String,
    pub checkpoint_id: Option<Uuid>,
}

pub(crate) fn parse_request(value: &Value) -> Result<WorkHandoffRequest> {
    let object = value
        .as_object()
        .context("work_handoff arguments must be an object")?;
    if let Some(checkpoint_id) = object.get("checkpoint_id") {
        anyhow::ensure!(
            checkpoint_id.is_string(),
            "checkpoint_id must be a UUID string when provided"
        );
    }
    let request: WorkHandoffRequest =
        serde_json::from_value(value.clone()).context("invalid work_handoff arguments")?;
    config::validate_session_id(&request.session_id)?;
    Ok(request)
}

pub(crate) fn render(session: &config::Session, request: WorkHandoffRequest) -> Result<String> {
    let store = checkpoints::Store::default_store()?;
    let jobs = mcp::snapshot_jobs_for_session(&session.id, MAX_HANDOFF_JOBS);
    render_with_sources(session, request, &store, jobs)
}

fn automatic_recall(
    session: &config::Session,
    checkpoint: Option<&checkpoints::CheckpointEnvelope>,
) -> (Value, bool) {
    let Some(checkpoint) = checkpoint else {
        return (
            json!({
                "status": "not_run",
                "reason": "checkpoint_not_selected"
            }),
            false,
        );
    };

    let mut query = checkpoint.checkpoint.title.trim().to_owned();
    if let Some(next_step_id) = checkpoint.checkpoint.next_step_id.as_deref()
        && let Some(step) = checkpoint
            .checkpoint
            .steps
            .iter()
            .find(|step| step.id == next_step_id)
    {
        let description = step.description.trim();
        if !description.is_empty() {
            if !query.is_empty() {
                query.push(' ');
            }
            query.push_str(description);
        }
    }
    if query.is_empty() {
        return (
            json!({
                "status": "not_run",
                "reason": "checkpoint_has_no_searchable_text"
            }),
            false,
        );
    }

    match recall::search(session, &query, None, MAX_HANDOFF_RECALL_HITS) {
        Ok(response) => {
            let has_hits = !response.hits.is_empty();
            (
                json!({
                    "status": "searched",
                    "query_source": "checkpoint_title_and_next_step",
                    "response": response
                }),
                has_hits,
            )
        }
        Err(_) => (
            json!({
                "status": "unavailable",
                "query_source": "checkpoint_title_and_next_step",
                "reason": "automatic_recall_failed"
            }),
            false,
        ),
    }
}

fn render_with_sources(
    session: &config::Session,
    request: WorkHandoffRequest,
    store: &checkpoints::Store,
    jobs: mcp::JobListSnapshot,
) -> Result<String> {
    anyhow::ensure!(request.session_id == session.id, "session ID mismatch");
    let canonical_cwd = config::canonical_directory(&session.cwd)?;
    anyhow::ensure!(canonical_cwd == session.cwd, "session cwd is not canonical");

    let (checkpoint, available, available_truncated, available_incomplete) =
        match request.checkpoint_id {
            Some(checkpoint_id) => (
                Some(store.load(session, checkpoint_id)?),
                Vec::new(),
                false,
                false,
            ),
            None => {
                let listed = store.list_for_scope(session)?;
                (
                    None,
                    listed.checkpoints,
                    listed.truncated,
                    listed.incomplete,
                )
            }
        };

    let (automatic_recall, has_recall_hits) = automatic_recall(session, checkpoint.as_ref());
    let has_running_job = jobs.jobs.iter().any(|job| job.status == "running");
    let mut resume_hints = Vec::new();
    if checkpoint.is_none() && !available.is_empty() {
        resume_hints.push("choose_checkpoint");
    }
    if has_running_job {
        resume_hints.push("inspect_running_jobs_before_repeating_work");
    }
    if checkpoint.is_some() {
        resume_hints.push("revalidate_repository_and_checks");
    }
    if checkpoint
        .as_ref()
        .is_some_and(|record| record.checkpoint.next_step_id.is_some())
    {
        resume_hints.push("review_reported_next_step");
    }
    if has_recall_hits {
        resume_hints.push("review_recalled_learnings");
    }

    let response = json!({
        "handoff_version": HANDOFF_VERSION,
        "session": {
            "id": session.id,
            "cwd": session.cwd,
            "started_at": session.started_at,
            "process_id": session.process_id,
            "observation": "current_session"
        },
        "checkpoint": checkpoint,
        "automatic_recall": automatic_recall,
        "available_checkpoints": available,
        "available_checkpoints_truncated": available_truncated,
        "available_checkpoints_incomplete": available_incomplete,
        "jobs": {
            "source": "live_snapshot",
            "jobs": jobs.jobs,
            "truncated": jobs.truncated,
            "retention": "in_memory"
        },
        "freshness": "not_revalidated",
        "resume_hints": resume_hints
    });
    render_bounded_response(&response)
}

fn render_bounded_response(response: &Value) -> Result<String> {
    let rendered = serde_json::to_string_pretty(response)?;
    anyhow::ensure!(
        rendered.len() <= MAX_HANDOFF_BYTES,
        "work_handoff response exceeds {MAX_HANDOFF_BYTES} bytes"
    );
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoints::{
        CheckpointCheck, CheckpointStep, ClientCheckpoint, ReportedResult, ReportedStatus, Store,
    };
    use crate::mcp::{JobListSnapshot, JobSummary};

    fn session(root: &std::path::Path, id: &str) -> config::Session {
        let cwd = config::canonical_directory(root).unwrap();
        config::Session {
            id: id.to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 7,
            process_id: 11,
            permission_mode: config::PermissionMode::Yolo,
        }
    }

    fn checkpoint(title: &str, description: &str) -> ClientCheckpoint {
        ClientCheckpoint {
            title: title.to_owned(),
            base_commit: Some("0123456789012345678901234567890123456789".to_owned()),
            steps: vec![CheckpointStep {
                id: "resume".to_owned(),
                description: description.to_owned(),
                reported_status: ReportedStatus::InProgress,
            }],
            checks: vec![CheckpointCheck {
                step_id: "resume".to_owned(),
                name: "validation".to_owned(),
                reported_result: ReportedResult::NotRun,
                commit: None,
            }],
            next_step_id: Some("resume".to_owned()),
        }
    }

    fn empty_jobs() -> JobListSnapshot {
        JobListSnapshot {
            jobs: Vec::new(),
            truncated: false,
        }
    }

    fn learning(title: &str, tags: &str, body: &str) -> String {
        format!(
            "---\ntitle: \"{title}\"\ndate: 2026-09-08\ntags: [{tags}]\ndomain: technical\nverification: verified\n---\n\n## Problem\n{body}\n\n## Resolution\nResolved deterministically.\n\n## Reusable lesson\nReuse this bounded learning.\n"
        )
    }

    #[test]
    fn handoff_lists_checkpoints_without_selecting_one() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "handoff-list");
        let store = Store::new(store_root.path().join("checkpoints"));
        store
            .save(&session, None, 0, checkpoint("first", "reported one"))
            .unwrap();
        store
            .save(&session, None, 0, checkpoint("second", "reported two"))
            .unwrap();
        let rendered = render_with_sources(
            &session,
            WorkHandoffRequest {
                session_id: session.id.clone(),
                checkpoint_id: None,
            },
            &store,
            empty_jobs(),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&rendered).unwrap();
        assert!(value["checkpoint"].is_null());
        assert_eq!(value["automatic_recall"]["status"], "not_run");
        assert_eq!(
            value["automatic_recall"]["reason"],
            "checkpoint_not_selected"
        );
        assert_eq!(value["available_checkpoints"].as_array().unwrap().len(), 2);
        assert_eq!(value["resume_hints"], json!(["choose_checkpoint"]));
        assert_eq!(value["freshness"], "not_revalidated");
    }

    #[test]
    fn handoff_distinguishes_reported_and_observed() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "handoff-sources");
        let store = Store::new(store_root.path().join("checkpoints"));
        let mut reported = checkpoint("reported", "reported next step");
        reported.steps[0].reported_status = ReportedStatus::Verified;
        reported.checks[0].reported_result = ReportedResult::Pass;
        reported.checks[0].commit = reported.base_commit.clone();
        let saved = store.save(&session, None, 0, reported).unwrap();
        let jobs = JobListSnapshot {
            jobs: vec![JobSummary {
                job_id: Uuid::from_u128(1).to_string(),
                status: "failed".to_owned(),
            }],
            truncated: false,
        };
        let rendered = render_with_sources(
            &session,
            WorkHandoffRequest {
                session_id: session.id.clone(),
                checkpoint_id: Some(saved.checkpoint_id),
            },
            &store,
            jobs,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["checkpoint"]["source"], "client_reported");
        assert_eq!(
            value["checkpoint"]["checkpoint"]["steps"][0]["reported_status"],
            "verified"
        );
        assert_eq!(value["jobs"]["source"], "live_snapshot");
        assert_eq!(value["jobs"]["jobs"][0]["status"], "failed");
        assert_eq!(
            value["resume_hints"],
            json!([
                "revalidate_repository_and_checks",
                "review_reported_next_step"
            ])
        );
    }

    #[test]
    fn handoff_selected_checkpoint_injects_local_recall_hits() {
        let root = tempfile::tempdir().unwrap();
        let learnings = root.path().join("learnings");
        std::fs::create_dir(&learnings).unwrap();
        std::fs::write(
            learnings.join("steam-tls.md"),
            learning(
                "Steam TLS certificate validation",
                "steam, tls, wine",
                "GnuTLS-backed validation is required for Steam certificate handling.",
            ),
        )
        .unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "handoff-recall");
        let store = Store::new(store_root.path().join("checkpoints"));
        let saved = store
            .save(
                &session,
                None,
                0,
                checkpoint("Steam TLS resume", "verify certificate validation"),
            )
            .unwrap();
        let rendered = render_with_sources(
            &session,
            WorkHandoffRequest {
                session_id: session.id.clone(),
                checkpoint_id: Some(saved.checkpoint_id),
            },
            &store,
            empty_jobs(),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["automatic_recall"]["status"], "searched");
        assert_eq!(
            value["automatic_recall"]["query_source"],
            "checkpoint_title_and_next_step"
        );
        assert_eq!(
            value["automatic_recall"]["response"]["hits"][0]["title"],
            "Steam TLS certificate validation"
        );
        assert!(
            value["resume_hints"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hint| hint == "review_recalled_learnings")
        );
    }

    #[test]
    fn handoff_is_scoped_to_current_worktree() {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let first = session(first_root.path(), "handoff-first");
        let second = session(second_root.path(), "handoff-second");
        let store = Store::new(store_root.path().join("checkpoints"));
        let saved = store
            .save(&first, None, 0, checkpoint("scope", "same worktree only"))
            .unwrap();
        let error = render_with_sources(
            &second,
            WorkHandoffRequest {
                session_id: second.id.clone(),
                checkpoint_id: Some(saved.checkpoint_id),
            },
            &store,
            empty_jobs(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("CHECKPOINT_NOT_FOUND"));
    }

    #[test]
    fn handoff_after_restart_does_not_replay_work() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let marker = root.path().join("must-not-be-created");
        let first = session(root.path(), "before-restart");
        let second = session(root.path(), "after-restart");
        let store = Store::new(store_root.path().join("checkpoints"));
        let instruction = format!("touch {}", marker.display());
        let saved = store
            .save(&first, None, 0, checkpoint("reported", &instruction))
            .unwrap();
        let record_path = store_root
            .path()
            .join("checkpoints")
            .join(format!("{}.json", saved.checkpoint_id));
        let before = std::fs::read(&record_path).unwrap();
        let rendered = render_with_sources(
            &second,
            WorkHandoffRequest {
                session_id: second.id.clone(),
                checkpoint_id: Some(saved.checkpoint_id),
            },
            &store,
            empty_jobs(),
        )
        .unwrap();
        let after = std::fs::read(&record_path).unwrap();
        assert_eq!(
            before, after,
            "read-only handoff mutated checkpoint storage"
        );
        assert!(!marker.exists(), "handoff executed client-reported text");
        assert!(rendered.contains(&instruction));
    }

    #[test]
    fn handoff_is_read_only() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "handoff-read-only");
        let store_dir = store_root.path().join("checkpoints");
        let store = Store::new(store_dir.clone());
        let saved = store
            .save(&session, None, 0, checkpoint("reported", "data only"))
            .unwrap();
        let path = store_dir.join(format!("{}.json", saved.checkpoint_id));
        let before = std::fs::read(&path).unwrap();
        let _ = render_with_sources(
            &session,
            WorkHandoffRequest {
                session_id: session.id.clone(),
                checkpoint_id: Some(saved.checkpoint_id),
            },
            &store,
            empty_jobs(),
        )
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn handoff_response_bound_is_enforced() {
        let oversized = json!({"padding": "x".repeat(MAX_HANDOFF_BYTES + 1)});
        assert!(render_bounded_response(&oversized).is_err());
    }

    #[test]
    fn handoff_does_not_follow_text_instructions() {
        let marker = "touch /tmp/temote-must-not-run";
        let value = checkpoint("reported", marker);
        let rendered = serde_json::to_string(&value).unwrap();
        assert!(rendered.contains(marker));
        assert_eq!(value.steps[0].description, marker);
    }

    #[test]
    fn handoff_request_rejects_unknown_fields_and_invalid_uuid() {
        assert!(parse_request(&json!({"session_id":"test","extra":true})).is_err());
        assert!(parse_request(&json!({"session_id":"test","checkpoint_id":"not-a-uuid"})).is_err());
    }

    #[test]
    fn checkpoint_store_fixture_distinguishes_reported_source() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "handoff-source");
        let store = Store::new(store_root.path().join("checkpoints"));
        let saved = store
            .save(&session, None, 0, checkpoint("reported", "do not execute"))
            .unwrap();
        assert_eq!(saved.source, "client_reported");
    }
}
