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
        ActivityOperation::ReadFile => "read_file",
        ActivityOperation::WriteFile => "write_file",
        ActivityOperation::GitPull => "git_pull",
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
