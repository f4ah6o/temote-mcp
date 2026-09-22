//! Pure, bounded rendering for validated activity events.

use std::fmt;

use uuid::Uuid;

use super::contract::{ActivityEvent, ActivityOperation, ActivityState};

pub const MAX_ACTIVITY_TIMESTAMP_DISPLAY_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderError {
    InvalidEvent,
    InvalidTimestamp,
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidEvent => "invalid_event",
            Self::InvalidTimestamp => "invalid_timestamp",
        })
    }
}

impl std::error::Error for RenderError {}

/// Renders one validated activity event with a caller-supplied display time.
///
/// Local-time conversion remains an I/O adapter concern. This function never
/// reads a clock, timezone, terminal, socket, or environment variable.
pub fn render_event(event: &ActivityEvent, display_timestamp: &str) -> Result<String, RenderError> {
    event.validate().map_err(|_| RenderError::InvalidEvent)?;
    validate_display_text(display_timestamp)?;

    let session = event.session_id().unwrap_or("-");
    let instance = event
        .session_instance()
        .map(short_uuid)
        .unwrap_or_else(|| "-".to_owned());
    let duration = event
        .duration_ms()
        .map(|duration| format!("{duration}ms"))
        .unwrap_or_else(|| "-".to_owned());
    let summary = if event.safe_summary().is_empty() {
        "-"
    } else {
        event.safe_summary()
    };

    Ok(format!(
        "{display_timestamp} seq={} session={session} instance={instance} operation={} state={} duration={duration} id={} summary={summary}",
        event.sequence(),
        operation_name(event.operation()),
        state_name(event.state()),
        short_uuid(event.operation_id()),
    ))
}

fn short_uuid(uuid: Uuid) -> String {
    uuid.simple().to_string()[..8].to_owned()
}

fn operation_name(operation: ActivityOperation) -> &'static str {
    match operation {
        ActivityOperation::SessionStart => "session_start",
        ActivityOperation::SessionStop => "session_stop",
        ActivityOperation::SessionRestart => "session_restart",
        ActivityOperation::SessionPermissionMode => "session_permission_mode",
        ActivityOperation::SessionPermissionAllow => "session_permission_allow",
        ActivityOperation::SessionPermissionRevoke => "session_permission_revoke",
        ActivityOperation::SessionRestartPolicy => "session_restart_policy",
        ActivityOperation::SessionForget => "session_forget",
        ActivityOperation::SessionCrash => "session_crash",
        ActivityOperation::SessionAutoRestart => "session_auto_restart",
        ActivityOperation::SupervisorUpgrade => "supervisor_upgrade",
        ActivityOperation::ReadFile => "read_file",
        ActivityOperation::WriteFile => "write_file",
        ActivityOperation::GitAdd => "git_add",
        ActivityOperation::GitCommit => "git_commit",
        ActivityOperation::GitFetch => "git_fetch",
        ActivityOperation::GitPull => "git_pull",
        ActivityOperation::GitPush => "git_push",
        ActivityOperation::GitBranchCreate => "git_branch_create",
        ActivityOperation::GitBranchDelete => "git_branch_delete",
        ActivityOperation::GitRemoteBranchDelete => "git_remote_branch_delete",
        ActivityOperation::GitSwitch => "git_switch",
        ActivityOperation::GitWorktreeAdd => "git_worktree_add",
        ActivityOperation::GitWorktreeCreate => "git_worktree_create",
        ActivityOperation::GitWorktreeList => "git_worktree_list",
        ActivityOperation::GitWorktreeRemove => "git_worktree_remove",
        ActivityOperation::GitWorktreePrune => "git_worktree_prune",
        ActivityOperation::GithubWorkflowDispatch => "github_workflow_dispatch",
        ActivityOperation::GithubWorkflowRunGet => "github_workflow_run_get",
        ActivityOperation::Execute => "execute",
        ActivityOperation::StartCommand => "start_command",
        ActivityOperation::StopJob => "stop_job",
        ActivityOperation::LocalAgentRun => "local_agent_run",
        ActivityOperation::DevToolRun => "dev_tool_run",
        ActivityOperation::GetImage => "get_image",
        ActivityOperation::EvidenceRead => "evidence_read",
        ActivityOperation::CodexStatus => "codex_status",
        ActivityOperation::CodexTaskStart => "codex_task_start",
        ActivityOperation::CodexTaskGet => "codex_task_get",
        ActivityOperation::CodexTaskControl => "codex_task_control",
        ActivityOperation::ListDirectory => "list_directory",
        ActivityOperation::ApplyPatch => "apply_patch",
        ActivityOperation::PollJob => "poll_job",
        ActivityOperation::JobList => "job_list",
        ActivityOperation::CheckpointSave => "checkpoint_save",
        ActivityOperation::CheckpointLoad => "checkpoint_load",
        ActivityOperation::WorkHandoff => "work_handoff",
        ActivityOperation::FrictionSummary => "friction_summary",
        ActivityOperation::LearningCandidateList => "learning_candidate_list",
        ActivityOperation::Recall => "recall",
        ActivityOperation::RecallFeedback => "recall_feedback",
        ActivityOperation::OnePasswordMcpDiscover => "onepassword_mcp_discover",
        ActivityOperation::OnePasswordMcpReadResource => "onepassword_mcp_read_resource",
        ActivityOperation::OnePasswordMcpCall => "onepassword_mcp_call",
        ActivityOperation::OnePasswordItemGet => "onepassword_item_get",
        ActivityOperation::OnePasswordSecretResolve => "onepassword_secret_resolve",
        ActivityOperation::OnePasswordServiceAccountStatus => "onepassword_service_account_status",
        ActivityOperation::OnePasswordServiceAccountRun => "onepassword_service_account_run",
        ActivityOperation::KintoneMcpStatus => "kintone_mcp_status",
        ActivityOperation::KintoneMcpDiscover => "kintone_mcp_discover",
        ActivityOperation::KintoneMcpCall => "kintone_mcp_call",
        ActivityOperation::KintoneCliStatus => "kintone_cli_status",
        ActivityOperation::KintoneCliRun => "kintone_cli_run",
        ActivityOperation::WithoutSandbox => "without_sandbox",
    }
}

fn state_name(state: ActivityState) -> &'static str {
    match state {
        ActivityState::Started => "started",
        ActivityState::WaitingApproval => "waiting_approval",
        ActivityState::Running => "running",
        ActivityState::Completed => "completed",
        ActivityState::Failed => "failed",
        ActivityState::Cancelled => "cancelled",
    }
}

fn validate_display_text(value: &str) -> Result<(), RenderError> {
    if value.is_empty()
        || value.len() > MAX_ACTIVITY_TIMESTAMP_DISPLAY_BYTES
        || value.chars().any(|character| {
            character.is_control()
                || character == '\u{061c}'
                || character == '\u{200e}'
                || character == '\u{200f}'
                || matches!(character, '\u{202a}'..='\u{202e}')
                || matches!(character, '\u{2066}'..='\u{2069}')
        })
    {
        return Err(RenderError::InvalidTimestamp);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::contract::{ActivityRemote, ActivitySummary, ActivityUpdate, EventStamp};

    fn event() -> ActivityEvent {
        let update = ActivityUpdate::new(
            Uuid::from_u128(0x1234_5678_0000_4000_8000_0000_0000_0001),
            ActivityOperation::GitPull,
            ActivityState::Completed,
            Some(1832),
            ActivitySummary::git(ActivityRemote::Origin),
        )
        .unwrap();
        ActivityEvent::from_update(
            update,
            EventStamp::new(
                812,
                1_780_000_000_000,
                Some("sf".to_owned()),
                Some(Uuid::from_u128(0x8765_4321_0000_4000_8000_0000_0000_0002)),
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn fixed_event_renders_as_one_stable_line_with_short_visual_ids() {
        assert_eq!(
            render_event(&event(), "2026-05-27 18:53:20.000 +09:00").unwrap(),
            "2026-05-27 18:53:20.000 +09:00 seq=812 session=sf instance=87654321 operation=git_pull state=completed duration=1832ms id=12345678 summary=remote=origin"
        );
    }

    #[test]
    fn nonterminal_and_session_independent_placeholders_are_explicit() {
        let event = ActivityEvent::from_update(
            ActivityUpdate::new(
                Uuid::from_u128(0xabcd_0000_0000_4000_8000_0000_0000_0001),
                ActivityOperation::SessionStart,
                ActivityState::Started,
                None,
                ActivitySummary::empty(),
            )
            .unwrap(),
            EventStamp::new(1, 0, None, None).unwrap(),
        )
        .unwrap();
        assert_eq!(
            render_event(&event, "1970-01-01 00:00:00.000 +00:00").unwrap(),
            "1970-01-01 00:00:00.000 +00:00 seq=1 session=- instance=- operation=session_start state=started duration=- id=abcd0000 summary=-"
        );
    }

    #[test]
    fn display_time_rejects_control_bidi_empty_and_oversized_text() {
        for invalid in [
            "",
            "2026-01-01\nforged",
            "2026-01-01\rforged",
            "2026-01-01\u{1b}[31m",
            "2026-01-01\u{202e}forged",
        ] {
            assert_eq!(
                render_event(&event(), invalid),
                Err(RenderError::InvalidTimestamp)
            );
        }
        assert_eq!(
            render_event(
                &event(),
                &"x".repeat(MAX_ACTIVITY_TIMESTAMP_DISPLAY_BYTES + 1)
            ),
            Err(RenderError::InvalidTimestamp)
        );
    }
}
