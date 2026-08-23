use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub(crate) const MAX_JSON_LINE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn encode_bounded_json_line(value: &Value, max_bytes: usize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).context("failed to serialize child MCP message")?;
    let wire_bytes = bytes
        .len()
        .checked_add(1)
        .context("child MCP message size overflow")?;
    anyhow::ensure!(
        wire_bytes <= max_bytes,
        "child MCP message exceeds {max_bytes} bytes"
    );
    bytes.push(b'\n');
    Ok(bytes)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BoundedLine {
    Line(String),
    TooLarge,
    InvalidUtf8,
}

pub(crate) async fn next_bounded_line<R>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<BoundedLine>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut oversized = false;
    let mut saw_any = false;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if !saw_any {
                return Ok(None);
            }
            break;
        }
        saw_any = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        if !oversized {
            if bytes.len().saturating_add(consumed) > max_bytes {
                oversized = true;
            } else {
                bytes.extend_from_slice(&available[..consumed]);
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }

    if oversized {
        return Ok(Some(BoundedLine::TooLarge));
    }
    match String::from_utf8(bytes) {
        Ok(line) => Ok(Some(BoundedLine::Line(line))),
        Err(_) => Ok(Some(BoundedLine::InvalidUtf8)),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RequestIdSequence {
    next: u64,
}

impl Default for RequestIdSequence {
    fn default() -> Self {
        Self { next: 1 }
    }
}

impl RequestIdSequence {
    pub(crate) fn take(&mut self) -> u64 {
        let id = self.next;
        self.next = self.next.checked_add(1).unwrap_or(1);
        id
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum ChildMessageKind {
    Response,
    ServerRequest(Value),
    Notification,
}

pub(crate) fn classify_child_message(
    message: &Value,
    expected_response_id: u64,
) -> Result<ChildMessageKind> {
    if message.get("method").and_then(Value::as_str).is_some() {
        return Ok(match message.get("id") {
            Some(id) => ChildMessageKind::ServerRequest(id.clone()),
            None => ChildMessageKind::Notification,
        });
    }

    let id = message
        .get("id")
        .ok_or_else(|| anyhow::anyhow!("child MCP message has neither method nor id"))?;
    anyhow::ensure!(
        id == &Value::from(expected_response_id),
        "unexpected child MCP response id: {id}"
    );
    Ok(ChildMessageKind::Response)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::test_support;

    #[test]
    fn generated_json_line_encoding_matches_wire_budget() -> noprop::TestResult {
        test_support::run(0x4a53_4f4e_4c49_4e45, 512, |ctx| {
            let max_bytes = noprop::sample_usize_in(ctx, 1..=256);
            let payload_len = noprop::sample_usize_in(ctx, 0..=300);
            let payload = (0..payload_len)
                .map(|_| match noprop::sample_usize_in(ctx, 0..=3) {
                    0 => 'x',
                    1 => '"',
                    2 => '\\',
                    _ => '\n',
                })
                .collect::<String>();
            let value = json!({"payload": payload});
            let serialized = serde_json::to_vec(&value).unwrap();
            let expected = serialized
                .len()
                .checked_add(1)
                .is_some_and(|wire| wire <= max_bytes);
            let result = encode_bounded_json_line(&value, max_bytes);
            assert_eq!(
                result.is_ok(),
                expected,
                "serialized={} max={max_bytes}",
                serialized.len()
            );
            if let Ok(line) = result {
                assert_eq!(line.len(), serialized.len() + 1);
                assert_eq!(line.last(), Some(&b'\n'));
                assert_eq!(&line[..line.len() - 1], serialized.as_slice());
            }
            Ok(())
        })
    }

    #[test]
    fn generated_request_ids_roll_over_without_sticking() -> noprop::TestResult {
        test_support::run(0x4348_494c_4449_4401, 1024, |ctx| {
            let start = noprop::sample_u64(ctx);
            let mut ids = RequestIdSequence { next: start };
            let first = ids.take();
            let second = ids.take();
            assert_eq!(first, start);
            assert_eq!(second, start.checked_add(1).unwrap_or(1));
            assert_ne!(first, second, "request id sequence stuck at {start}");
            Ok(())
        })
    }

    #[test]
    fn generated_child_message_interleavings_preserve_duplex_requests() -> noprop::TestResult {
        test_support::run(0x4348_494c_4449_4e54, 1024, |ctx| {
            let expected = noprop::sample_u64(ctx);
            let other = expected.wrapping_add(1);
            let mode = noprop::sample_usize_in(ctx, 0..6);
            let (message, expected_kind) = match mode {
                0 => (
                    json!({"jsonrpc":"2.0","id":expected,"result":{}}),
                    Some(ChildMessageKind::Response),
                ),
                1 => (json!({"jsonrpc":"2.0","id":other,"result":{}}), None),
                2 => (
                    json!({"jsonrpc":"2.0","id":expected,"method":"sampling/createMessage"}),
                    Some(ChildMessageKind::ServerRequest(json!(expected))),
                ),
                3 => (
                    json!({"jsonrpc":"2.0","id":other,"method":"roots/list"}),
                    Some(ChildMessageKind::ServerRequest(json!(other))),
                ),
                4 => (
                    json!({"jsonrpc":"2.0","method":"notifications/progress"}),
                    Some(ChildMessageKind::Notification),
                ),
                _ => (json!({"jsonrpc":"2.0","result":{}}), None),
            };
            let actual = classify_child_message(&message, expected);
            match expected_kind {
                Some(expected_kind) => assert_eq!(actual.unwrap(), expected_kind),
                None => assert!(actual.is_err(), "accepted invalid interleaving: {message}"),
            }
            Ok(())
        })
    }
}
