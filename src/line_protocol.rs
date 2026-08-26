#[path = "child_mcp.rs"]
mod child_mcp;
#[path = "integration.rs"]
pub(crate) mod integration;
pub(crate) use child_mcp::{ChildMcp, session_probe_means_stopped};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub(crate) const MAX_JSON_LINE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_CHILD_TOOL_NAME_BYTES: usize = 256;
pub(crate) const MAX_CHILD_ARGUMENT_KEYS: usize = 256;
pub(crate) const MAX_CHILD_ARGUMENT_KEY_BYTES: usize = 256;
pub(crate) const MAX_CHILD_RESOURCE_URI_BYTES: usize = 4096;

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

pub(crate) fn validate_child_tool_call(tool_name: &str, arguments: &Value) -> Result<()> {
    anyhow::ensure!(
        !tool_name.is_empty(),
        "child MCP tool name must not be empty"
    );
    anyhow::ensure!(
        tool_name.len() <= MAX_CHILD_TOOL_NAME_BYTES,
        "child MCP tool name exceeds {MAX_CHILD_TOOL_NAME_BYTES} bytes"
    );
    anyhow::ensure!(
        !tool_name.contains('\0'),
        "child MCP tool name must not contain NUL bytes"
    );
    let object = arguments
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("child MCP tool arguments must be an object"))?;
    anyhow::ensure!(
        object.len() <= MAX_CHILD_ARGUMENT_KEYS,
        "child MCP tool arguments exceed {MAX_CHILD_ARGUMENT_KEYS} keys"
    );
    for key in object.keys() {
        anyhow::ensure!(
            key.len() <= MAX_CHILD_ARGUMENT_KEY_BYTES,
            "child MCP argument key exceeds {MAX_CHILD_ARGUMENT_KEY_BYTES} bytes"
        );
        anyhow::ensure!(
            !key.contains('\0'),
            "child MCP argument keys must not contain NUL bytes"
        );
    }
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": tool_name, "arguments": arguments},
    });
    encode_bounded_json_line(&request, MAX_JSON_LINE_BYTES)?;
    Ok(())
}

pub(crate) fn validate_child_resource_uri(uri: &str, required_prefix: &str) -> Result<()> {
    anyhow::ensure!(
        uri.len() <= MAX_CHILD_RESOURCE_URI_BYTES,
        "child MCP resource URI exceeds {MAX_CHILD_RESOURCE_URI_BYTES} bytes"
    );
    anyhow::ensure!(
        !uri.contains('\0'),
        "child MCP resource URI must not contain NUL bytes"
    );
    anyhow::ensure!(
        uri.starts_with(required_prefix),
        "unsupported child MCP resource URI"
    );
    Ok(())
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
    fn generated_child_tool_call_bounds_match_reference_model() -> noprop::TestResult {
        test_support::run(0x4348_494c_4442_4f55, 512, |ctx| {
            let name_len = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => 0,
                1 => 1,
                2 => MAX_CHILD_TOOL_NAME_BYTES - 1,
                3 => MAX_CHILD_TOOL_NAME_BYTES,
                _ => MAX_CHILD_TOOL_NAME_BYTES + 1,
            };
            let mut tool_name = "t".repeat(name_len);
            let name_nul = name_len > 0 && noprop::sample_bool(ctx);
            if name_nul {
                tool_name.replace_range(0..1, "\0");
            }

            let key_count = match noprop::sample_usize_in(ctx, 0..=3) {
                0 => 0,
                1 => 1,
                2 => MAX_CHILD_ARGUMENT_KEYS,
                _ => MAX_CHILD_ARGUMENT_KEYS + 1,
            };
            let key_len = match noprop::sample_usize_in(ctx, 0..=3) {
                0 => 1,
                1 => MAX_CHILD_ARGUMENT_KEY_BYTES - 1,
                2 => MAX_CHILD_ARGUMENT_KEY_BYTES,
                _ => MAX_CHILD_ARGUMENT_KEY_BYTES + 1,
            };
            let key_nul = key_count > 0 && noprop::sample_bool(ctx);
            let mut object = serde_json::Map::new();
            for index in 0..key_count {
                let suffix = format!("_{index:x}");
                let prefix_len = key_len.saturating_sub(suffix.len());
                let mut key = format!("{}{}", "k".repeat(prefix_len), suffix);
                if key_nul && index == 0 {
                    key.replace_range(0..1, "\0");
                }
                object.insert(key, json!(index));
            }
            let arguments = Value::Object(object);
            let expected = name_len > 0
                && name_len <= MAX_CHILD_TOOL_NAME_BYTES
                && !name_nul
                && key_count <= MAX_CHILD_ARGUMENT_KEYS
                && (key_count == 0 || key_len <= MAX_CHILD_ARGUMENT_KEY_BYTES)
                && !key_nul;
            let actual = validate_child_tool_call(&tool_name, &arguments);
            assert_eq!(
                actual.is_ok(),
                expected,
                "name_len={name_len} key_count={key_count} key_len={key_len} name_nul={name_nul} key_nul={key_nul}"
            );
            Ok(())
        })
    }

    #[test]
    fn child_tool_call_rejects_wire_oversize_before_write() {
        let arguments = json!({"payload": "x".repeat(MAX_JSON_LINE_BYTES)});
        let error = validate_child_tool_call("oversized", &arguments).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn generated_child_resource_uri_bounds_match_reference_model() -> noprop::TestResult {
        test_support::run(0x4348_494c_4455_5249, 512, |ctx| {
            let prefix = "1password://";
            let target_len = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => prefix.len(),
                1 => MAX_CHILD_RESOURCE_URI_BYTES - 1,
                2 => MAX_CHILD_RESOURCE_URI_BYTES,
                3 => MAX_CHILD_RESOURCE_URI_BYTES + 1,
                _ => noprop::sample_usize_in(ctx, prefix.len()..=MAX_CHILD_RESOURCE_URI_BYTES + 1),
            };
            let valid_prefix = noprop::sample_bool(ctx);
            let actual_prefix = if valid_prefix { prefix } else { "other://" };
            let mut uri = actual_prefix.to_owned();
            uri.push_str(&"x".repeat(target_len.saturating_sub(actual_prefix.len())));
            let has_nul = !uri.is_empty() && noprop::sample_bool(ctx);
            if has_nul {
                uri.push('\0');
            }
            let expected =
                uri.len() <= MAX_CHILD_RESOURCE_URI_BYTES && !has_nul && uri.starts_with(prefix);
            assert_eq!(
                validate_child_resource_uri(&uri, prefix).is_ok(),
                expected,
                "len={} valid_prefix={valid_prefix} has_nul={has_nul}",
                uri.len()
            );
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
