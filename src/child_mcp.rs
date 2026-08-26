use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::config;
use crate::line_protocol::{
    BoundedLine, ChildMessageKind, MAX_JSON_LINE_BYTES, RequestIdSequence, classify_child_message,
    encode_bounded_json_line, next_bounded_line,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const PROTOCOL_VERSION: &str = "2025-06-18";

pub(crate) struct ChildMcp {
    child: Arc<Mutex<Child>>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_ids: RequestIdSequence,
    session_watcher: Option<JoinHandle<()>>,
    server_label: &'static str,
    capability_label: &'static str,
}

impl Drop for ChildMcp {
    fn drop(&mut self) {
        if let Some(watcher) = self.session_watcher.take() {
            watcher.abort();
        }
    }
}

impl ChildMcp {
    pub(crate) async fn spawn(
        mut command: Command,
        server_label: &'static str,
        capability_label: &'static str,
        watch_session_id: Option<String>,
    ) -> Result<Self> {
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start {server_label} server"))?;
        let stdin = child
            .stdin
            .take()
            .with_context(|| format!("{server_label} stdin is unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .with_context(|| format!("{server_label} stdout is unavailable"))?;
        let child = Arc::new(Mutex::new(child));
        let session_watcher = watch_session_id.map(|session_id| {
            let watched_child = Arc::clone(&child);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let probe = config::session_is_active(&session_id).await;
                    if session_probe_means_stopped(&probe) {
                        let mut child = watched_child.lock().await;
                        let _ = child.kill().await;
                        return;
                    }
                }
            })
        });
        let mut client = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            request_ids: RequestIdSequence::default(),
            session_watcher,
            server_label,
            capability_label,
        };
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "temote-mcp",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await
            .with_context(|| format!("failed to initialize {server_label} server"))?;
        client
            .notify("notifications/initialized", json!({}))
            .await
            .with_context(|| format!("failed to finish {server_label} initialization"))?;
        Ok(client)
    }

    pub(crate) fn watcher_finished(&self) -> bool {
        self.session_watcher
            .as_ref()
            .is_some_and(|watcher| watcher.is_finished())
    }

    pub(crate) async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.request_ids.take();
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        let response = tokio::time::timeout(REQUEST_TIMEOUT, async {
            loop {
                let line = match next_bounded_line(&mut self.stdout, MAX_JSON_LINE_BYTES).await? {
                    Some(BoundedLine::Line(line)) => line,
                    Some(BoundedLine::TooLarge) => {
                        anyhow::bail!(
                            "{} server response exceeds {MAX_JSON_LINE_BYTES} bytes",
                            self.server_label
                        )
                    }
                    Some(BoundedLine::InvalidUtf8) => {
                        anyhow::bail!("{} server returned invalid UTF-8", self.server_label)
                    }
                    None => {
                        let status = self.child.lock().await.try_wait().ok().flatten();
                        anyhow::bail!(
                            "{} server closed stdout (status: {status:?})",
                            self.server_label
                        )
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let message: Value = serde_json::from_str(&line)
                    .with_context(|| format!("{} server returned invalid JSON", self.server_label))?;
                match classify_child_message(&message, id)? {
                    ChildMessageKind::Response => {
                        if let Some(error) = message.get("error") {
                            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32000);
                            let message = error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown child MCP error");
                            anyhow::bail!("{} error {code}: {message}", self.server_label)
                        }
                        return message
                            .get("result")
                            .cloned()
                            .with_context(|| format!("{} response is missing result", self.server_label));
                    }
                    ChildMessageKind::ServerRequest(request_id) => {
                        self.write_json(&json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "error": {
                                "code": -32601,
                                "message": format!(
                                    "temote-mcp does not expose client-side MCP capabilities to {}",
                                    self.capability_label
                                )
                            }
                        }))
                        .await?;
                    }
                    ChildMessageKind::Notification => {}
                }
            }
        })
        .await
        .with_context(|| format!("timed out waiting for {} server", self.server_label))??;
        Ok(response)
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn write_json(&mut self, value: &Value) -> Result<()> {
        let line = encode_bounded_json_line(value, MAX_JSON_LINE_BYTES)?;
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

pub(crate) fn session_probe_means_stopped(probe: &Result<bool>) -> bool {
    matches!(probe, Ok(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn generated_session_watcher_stops_only_on_explicit_inactive() -> noprop::TestResult {
        test_support::run(0x4348_4d43_5052_4f42, 512, |ctx| {
            let choice = noprop::sample_usize_in(ctx, 0..3);
            let probe = match choice {
                0 => Ok(true),
                1 => Ok(false),
                _ => Err(anyhow::anyhow!("probe failure")),
            };
            assert_eq!(session_probe_means_stopped(&probe), choice == 1);
            Ok(())
        })
    }
}
