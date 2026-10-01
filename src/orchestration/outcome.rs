//! Separated task outcome states: execution, verification, and delivery.
//!
//! A backend's execution status (`completed`, `failed`, ...) says nothing
//! about whether the produced revision was verified or delivered. Records
//! carry optional [`VerificationRecord`] / [`DeliveryRecord`] values next to
//! the existing execution status; absence reads as `not_run` /
//! `not_started`, never as success.
//!
//! Verification is bound to the task record revision observed when the
//! result was recorded. Once the record revision moves on — or the verified
//! content was never identified — the stored result is no longer reported as
//! the current verification status: the view returns `not_run` with
//! `stale: true` while keeping the stored target visible.
//!
//! The logical task / execution distinction stays in the record structure:
//! `task_id` names the logical task and the execution `generation` selects
//! one execution. [`execution_id`] derives a stable id from that pair
//! without reassigning task ids.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

/// Bound for every text field kept in a verification or delivery record.
pub(crate) const MAX_OUTCOME_TEXT_BYTES: usize = 256;

/// Stable namespace for derived execution ids: distinct from the task-id and
/// request-fingerprint namespaces, so changing it renames executions.
const EXECUTION_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0xe8, 0xd0, 0xbd, 0xb2, 0x60, 0xfa, 0x51, 0xcb, 0xaf, 0x9f, 0x92, 0xc0, 0x35, 0x9a, 0xce, 0x9b,
]);

/// A recorded verification result for one revision of a task.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct VerificationRecord {
    pub(crate) status: VerificationStatus,
    pub(crate) target: VerificationTarget,
    /// The task record revision observed when the result was recorded.
    pub(crate) record_revision: u64,
    /// Unix seconds when the verification completed.
    pub(crate) checked_at: u64,
}

/// The verified outcome. `not_run` is the absence of a record, never a
/// stored value.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationStatus {
    Passed,
    Failed,
}

impl VerificationStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

/// What content a verification result describes. A commit alone does not
/// identify a dirty worktree, so verification of uncommitted content must
/// use an opaque snapshot id or be recorded as unidentified.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum VerificationTarget {
    /// A clean workspace at a known commit.
    Commit { commit: String },
    /// Content identified by an opaque snapshot id.
    Snapshot { snapshot: String },
    /// The verified content could not be identified; such a result is never
    /// reported as the current verification status.
    Unidentified { commit: Option<String>, dirty: bool },
}

impl VerificationRecord {
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.record_revision > 0,
            "verification record_revision must be positive"
        );
        anyhow::ensure!(
            self.checked_at > 0,
            "verification checked_at must be positive"
        );
        match &self.target {
            VerificationTarget::Commit { commit } => validate_text(commit, "verification commit"),
            VerificationTarget::Snapshot { snapshot } => {
                validate_text(snapshot, "verification snapshot")
            }
            VerificationTarget::Unidentified { commit, .. } => match commit {
                Some(commit) => validate_text(commit, "verification commit"),
                None => Ok(()),
            },
        }
    }
}

/// A recorded delivery state for one task.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct DeliveryRecord {
    pub(crate) status: DeliveryStatus,
    pub(crate) branch: Option<String>,
    pub(crate) pull_request: Option<String>,
    /// Unix seconds when the delivery state last changed.
    pub(crate) updated_at: u64,
}

/// Delivery progress. `not_started` is the absence of a record, never a
/// stored value.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryStatus {
    /// A delivery operation is accepted and in flight; the remote result is
    /// not yet known.
    Pending,
    /// The branch was pushed and its pull request created or updated.
    Submitted,
    Merged,
    /// The pull request was closed without merging.
    Closed,
    Failed,
}

impl DeliveryStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Submitted => "submitted",
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }
}

impl DeliveryRecord {
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.updated_at > 0, "delivery updated_at must be positive");
        if let Some(branch) = &self.branch {
            validate_text(branch, "delivery branch")?;
        }
        if let Some(pull_request) = &self.pull_request {
            validate_text(pull_request, "delivery pull_request")?;
        }
        Ok(())
    }
}

fn validate_text(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= MAX_OUTCOME_TEXT_BYTES && !value.contains('\0'),
        "{label} must contain 1..={MAX_OUTCOME_TEXT_BYTES} NUL-free UTF-8 bytes"
    );
    Ok(())
}

/// Stable execution id for one (logical task, execution generation) pair.
/// The logical `task_id` itself is never reassigned.
pub(crate) fn execution_id(task_id: Uuid, generation: u64) -> Uuid {
    let mut bytes = Vec::with_capacity(24);
    bytes.extend_from_slice(task_id.as_bytes());
    bytes.extend_from_slice(&generation.to_le_bytes());
    Uuid::new_v5(&EXECUTION_ID_NAMESPACE, &bytes)
}

/// Execution state view. The backend's status string is preserved verbatim,
/// so `waiting_input` / `unknown` / `reconciliation_required` are never
/// folded into success or failure.
pub(crate) fn execution_view(task_id: Uuid, generation: u64, state: &str) -> Value {
    json!({
        "id": execution_id(task_id, generation),
        "generation": generation,
        "state": state,
    })
}

/// Verification state view for the current task record revision.
///
/// The stored result applies only while `record_revision` still matches the
/// record and its target identifies content. Otherwise the current status is
/// `not_run` with `stale: true`; the stored target and time stay visible so
/// the previous result is not silently lost, but it is never reported as the
/// current PASS.
pub(crate) fn verification_view(
    stored: Option<&VerificationRecord>,
    record_revision: u64,
) -> Value {
    let Some(stored) = stored else {
        return json!({
            "status": "not_run",
            "stale": false,
            "target": null,
            "record_revision": null,
            "checked_at": null,
        });
    };
    let current = stored.record_revision == record_revision
        && !matches!(stored.target, VerificationTarget::Unidentified { .. });
    json!({
        "status": if current { stored.status.as_str() } else { "not_run" },
        "stale": !current,
        "target": &stored.target,
        "record_revision": stored.record_revision,
        "checked_at": stored.checked_at,
    })
}

/// Delivery state view. Absence reads as `not_started`; an execution status
/// or an agent-reported pull request list never substitutes for a recorded
/// delivery state.
pub(crate) fn delivery_view(stored: Option<&DeliveryRecord>) -> Value {
    match stored {
        None => json!({
            "status": "not_started",
            "branch": null,
            "pull_request": null,
            "updated_at": null,
        }),
        Some(stored) => json!({
            "status": stored.status.as_str(),
            "branch": stored.branch,
            "pull_request": stored.pull_request,
            "updated_at": stored.updated_at,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_id() -> Uuid {
        Uuid::parse_str("0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb").unwrap()
    }

    fn passed_at(record_revision: u64) -> VerificationRecord {
        VerificationRecord {
            status: VerificationStatus::Passed,
            target: VerificationTarget::Commit {
                commit: "abc123".to_owned(),
            },
            record_revision,
            checked_at: 1_700_000_000,
        }
    }

    fn delivered() -> DeliveryRecord {
        DeliveryRecord {
            status: DeliveryStatus::Submitted,
            branch: Some("feat/a4".to_owned()),
            pull_request: Some("https://example.invalid/pr/1".to_owned()),
            updated_at: 1_700_000_001,
        }
    }

    #[test]
    fn execution_id_is_stable_per_task_and_generation() {
        let task_id = task_id();
        let other = Uuid::parse_str("0199cccc-cccc-7ccc-8ccc-cccccccccccc").unwrap();
        assert_eq!(execution_id(task_id, 0), execution_id(task_id, 0));
        assert_ne!(execution_id(task_id, 0), execution_id(task_id, 1));
        assert_ne!(execution_id(task_id, 0), execution_id(other, 0));
    }

    #[test]
    fn execution_view_keeps_non_terminal_states_verbatim() {
        let task_id = task_id();
        for state in [
            "accepted",
            "running",
            "waiting_input",
            "waiting_approval",
            "retryable_failed",
            "completed",
            "interrupted",
            "failed",
            "reconciliation_required",
            "unknown",
        ] {
            let view = execution_view(task_id, 2, state);
            assert_eq!(view["state"], state);
        }
        let view = execution_view(task_id, 2, "waiting_input");
        assert_eq!(view["generation"], 2);
        assert_eq!(
            view["id"],
            execution_id(task_id, 2).to_string(),
            "execution id must be stable in the view"
        );
    }

    #[test]
    fn verification_absent_reads_as_not_run() {
        assert_eq!(
            verification_view(None, 4),
            json!({
                "status": "not_run",
                "stale": false,
                "target": null,
                "record_revision": null,
                "checked_at": null,
            })
        );
    }

    #[test]
    fn verification_at_the_current_revision_reports_the_stored_status() {
        let view = verification_view(Some(&passed_at(5)), 5);
        assert_eq!(view["status"], "passed");
        assert_eq!(view["stale"], false);
        assert_eq!(view["record_revision"], 5);
        assert_eq!(view["checked_at"], 1_700_000_000);
        assert_eq!(view["target"]["kind"], "commit");
        assert_eq!(view["target"]["commit"], "abc123");
    }

    #[test]
    fn stale_verification_is_not_reported_as_a_current_pass() {
        // The recorded result belongs to revision 4; the record has moved to
        // revision 5. It must not be presented as the current PASS.
        let view = verification_view(Some(&passed_at(4)), 5);
        assert_eq!(view["status"], "not_run");
        assert_eq!(view["stale"], true);
        assert_eq!(view["record_revision"], 4);
        assert_eq!(view["checked_at"], 1_700_000_000);
        assert_eq!(
            view["target"]["commit"], "abc123",
            "the previous target stays visible"
        );
    }

    #[test]
    fn snapshot_targets_can_be_current() {
        let record = VerificationRecord {
            status: VerificationStatus::Failed,
            target: VerificationTarget::Snapshot {
                snapshot: "sha256:deadbeef".to_owned(),
            },
            record_revision: 5,
            checked_at: 1_700_000_000,
        };
        let view = verification_view(Some(&record), 5);
        assert_eq!(view["status"], "failed");
        assert_eq!(view["stale"], false);
        assert_eq!(view["target"]["kind"], "snapshot");
    }

    #[test]
    fn unidentified_verification_is_never_current() {
        let record = VerificationRecord {
            status: VerificationStatus::Passed,
            target: VerificationTarget::Unidentified {
                commit: Some("abc123".to_owned()),
                dirty: true,
            },
            record_revision: 5,
            checked_at: 1_700_000_000,
        };
        let view = verification_view(Some(&record), 5);
        assert_eq!(view["status"], "not_run");
        assert_eq!(view["stale"], true);
        assert_eq!(view["target"]["kind"], "unidentified");
        assert_eq!(view["target"]["dirty"], true);
    }

    #[test]
    fn delivery_absent_reads_as_not_started() {
        assert_eq!(
            delivery_view(None),
            json!({
                "status": "not_started",
                "branch": null,
                "pull_request": null,
                "updated_at": null,
            })
        );
    }

    #[test]
    fn delivery_record_reports_its_bindings() {
        let view = delivery_view(Some(&delivered()));
        assert_eq!(view["status"], "submitted");
        assert_eq!(view["branch"], "feat/a4");
        assert_eq!(view["pull_request"], "https://example.invalid/pr/1");
        assert_eq!(view["updated_at"], 1_700_000_001);
    }

    #[test]
    fn outcome_records_round_trip_as_json() {
        let verification = VerificationRecord {
            status: VerificationStatus::Failed,
            target: VerificationTarget::Snapshot {
                snapshot: "sha256:deadbeef".to_owned(),
            },
            record_revision: 9,
            checked_at: 1_700_000_002,
        };
        let value = serde_json::to_value(&verification).unwrap();
        assert_eq!(value["target"]["kind"], "snapshot");
        assert_eq!(
            serde_json::from_value::<VerificationRecord>(value).unwrap(),
            verification
        );

        let value = serde_json::to_value(delivered()).unwrap();
        assert_eq!(
            serde_json::from_value::<DeliveryRecord>(value).unwrap(),
            delivered()
        );
    }

    #[test]
    fn validation_bounds_outcome_text_and_timestamps() {
        let mut record = passed_at(1);
        assert!(record.validate().is_ok());

        record.checked_at = 0;
        assert!(record.validate().is_err());
        record = passed_at(0);
        assert!(record.validate().is_err());
        record = passed_at(1);
        record.target = VerificationTarget::Commit {
            commit: String::new(),
        };
        assert!(record.validate().is_err());
        record.target = VerificationTarget::Commit {
            commit: "x".repeat(MAX_OUTCOME_TEXT_BYTES + 1),
        };
        assert!(record.validate().is_err());
        record.target = VerificationTarget::Unidentified {
            commit: None,
            dirty: true,
        };
        assert!(record.validate().is_ok());

        let mut delivery = delivered();
        assert!(delivery.validate().is_ok());
        delivery.updated_at = 0;
        assert!(delivery.validate().is_err());
        delivery = delivered();
        delivery.branch = Some(String::new());
        assert!(delivery.validate().is_err());
    }
}
