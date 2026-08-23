use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::line_protocol::{
    BoundedLine, ChildMessageKind, MAX_JSON_LINE_BYTES, classify_child_message, next_bounded_line,
};
use crate::{approvals, config};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CACHED_CLIENTS: usize = 64;
const PROTOCOL_VERSION: &str = "2025-06-18";

struct Client {
    child: Arc<Mutex<Child>>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    session_watcher: JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.session_watcher.abort();
    }
}

fn session_probe_means_stopped(probe: &Result<bool>) -> bool {
    matches!(probe, Ok(false))
}

fn clients() -> &'static Mutex<HashMap<String, Client>> {
    static CLIENTS: OnceLock<Mutex<HashMap<String, Client>>> = OnceLock::new();
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Client {
    async fn spawn(session: &config::Session) -> Result<Self> {
        let executable = executable_path()?;
        let mut child = Command::new(&executable)
            .current_dir(&session.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start 1Password MCP server at {}",
                    executable.display()
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .context("1Password MCP stdin is unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("1Password MCP stdout is unavailable")?;
        let child = Arc::new(Mutex::new(child));
        let watched_child = Arc::clone(&child);
        let watched_session_id = session.id.clone();
        let session_watcher = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let probe = config::session_is_active(&watched_session_id).await;
                if session_probe_means_stopped(&probe) {
                    let mut child = watched_child.lock().await;
                    let _ = child.kill().await;
                    return;
                }
            }
        });
        let mut client = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            session_watcher,
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
            .context("failed to initialize 1Password MCP server")?;
        client
            .notify("notifications/initialized", json!({}))
            .await
            .context("failed to finish 1Password MCP initialization")?;
        Ok(client)
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
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
                            "1Password MCP server response exceeds {MAX_JSON_LINE_BYTES} bytes"
                        )
                    }
                    Some(BoundedLine::InvalidUtf8) => {
                        anyhow::bail!("1Password MCP server returned invalid UTF-8")
                    }
                    None => {
                        let status = self.child.lock().await.try_wait().ok().flatten();
                        anyhow::bail!("1Password MCP server closed stdout (status: {status:?})")
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let message: Value = serde_json::from_str(&line)
                    .context("1Password MCP server returned invalid JSON")?;
                match classify_child_message(&message, id)? {
                    ChildMessageKind::Response => {
                        if let Some(error) = message.get("error") {
                            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32000);
                            let message = error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown 1Password MCP error");
                            anyhow::bail!("1Password MCP error {code}: {message}")
                        }
                        return message
                            .get("result")
                            .cloned()
                            .context("1Password MCP response is missing result");
                    }
                    ChildMessageKind::ServerRequest(request_id) => {
                        self.write_json(&json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "error": {
                                "code": -32601,
                                "message": "temote-mcp does not expose client-side MCP capabilities to 1Password"
                            }
                        }))
                        .await?;
                    }
                    ChildMessageKind::Notification => {}
                }
            }
        })
        .await
        .context("timed out waiting for 1Password MCP server")??;
        Ok(response)
    }

    async fn write_json(&mut self, value: &Value) -> Result<()> {
        self.stdin
            .write_all(serde_json::to_string(value)?.as_bytes())
            .await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

pub async fn discover(session: &config::Session) -> Result<Value> {
    let (resources, tools) = {
        let mut clients = clients().lock().await;
        ensure_client(&mut clients, session).await?;
        let client = clients
            .get_mut(&session.id)
            .context("1Password MCP client disappeared")?;
        let resources = match client.request("resources/list", json!({})).await {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        };
        let tools = match client.request("tools/list", json!({})).await {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        };
        (resources, tools)
    };
    approvals::activity(&session.id, "Discovered 1Password MCP capabilities", None).await;
    Ok(json!({"resources": resources["resources"], "tools": tools["tools"]}))
}

pub async fn read_resource(session: &config::Session, uri: &str) -> Result<Value> {
    anyhow::ensure!(
        uri.starts_with("1password://"),
        "unsupported 1Password resource URI"
    );
    let result = {
        let mut clients = clients().lock().await;
        ensure_client(&mut clients, session).await?;
        let client = clients
            .get_mut(&session.id)
            .context("1Password MCP client disappeared")?;
        let listed = client.request("resources/list", json!({})).await;
        let listed = match listed {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        };
        let allowed = listed["resources"].as_array().is_some_and(|resources| {
            resources
                .iter()
                .any(|resource| resource["uri"].as_str() == Some(uri))
        });
        anyhow::ensure!(allowed, "unknown 1Password MCP resource: {uri}");
        match client.request("resources/read", json!({"uri": uri})).await {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        }
    };
    approvals::activity(
        &session.id,
        "Read 1Password MCP resource",
        Some(format!("└ {uri}")),
    )
    .await;
    Ok(result)
}

pub async fn call_tool(
    session: &config::Session,
    tool_name: &str,
    arguments: Value,
) -> Result<Value> {
    anyhow::ensure!(
        !tool_name.is_empty(),
        "1Password tool name must not be empty"
    );
    anyhow::ensure!(
        arguments.is_object(),
        "1Password tool arguments must be an object"
    );
    enforce_path_boundary(session, tool_name, &arguments)?;

    let descriptor = {
        let mut clients = clients().lock().await;
        ensure_client(&mut clients, session).await?;
        let client = clients
            .get_mut(&session.id)
            .context("1Password MCP client disappeared")?;
        let listed = match client.request("tools/list", json!({})).await {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        };
        listed["tools"]
            .as_array()
            .and_then(|tools| {
                tools
                    .iter()
                    .find(|tool| tool["name"].as_str() == Some(tool_name))
                    .cloned()
            })
            .with_context(|| format!("unknown 1Password MCP tool: {tool_name}"))?
    };

    let read_only = descriptor
        .pointer("/annotations/readOnlyHint")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !read_only
        && !approvals::request(
            &session.id,
            "onepassword_mcp_call",
            safe_call_summary(tool_name, &arguments),
            session.cwd.clone(),
        )
        .await?
    {
        anyhow::bail!("user denied 1Password MCP tool call")
    }

    let result = {
        let mut clients = clients().lock().await;
        ensure_client(&mut clients, session).await?;
        let client = clients
            .get_mut(&session.id)
            .context("1Password MCP client disappeared")?;
        match client
            .request(
                "tools/call",
                json!({"name": tool_name, "arguments": arguments}),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        }
    };
    approvals::activity(
        &session.id,
        format!("Called 1Password MCP tool {tool_name}"),
        None,
    )
    .await;
    Ok(result)
}

async fn ensure_client(
    clients: &mut HashMap<String, Client>,
    session: &config::Session,
) -> Result<()> {
    retain_live_entries(clients, |client| !client.session_watcher.is_finished());
    if clients.contains_key(&session.id) {
        return Ok(());
    }
    anyhow::ensure!(
        client_capacity_available(false, clients.len()),
        "1Password MCP client limit reached ({MAX_CACHED_CLIENTS})"
    );
    clients.insert(session.id.clone(), Client::spawn(session).await?);
    Ok(())
}

fn retain_live_entries<T>(entries: &mut HashMap<String, T>, mut is_live: impl FnMut(&T) -> bool) {
    entries.retain(|_, entry| is_live(entry));
}

fn client_capacity_available(existing: bool, live_count: usize) -> bool {
    existing || live_count < MAX_CACHED_CLIENTS
}

fn enforce_path_boundary(
    session: &config::Session,
    tool_name: &str,
    arguments: &Value,
) -> Result<()> {
    if tool_name != "create_local_env_file" {
        return Ok(());
    }
    let mount_path = arguments
        .get("mountPath")
        .and_then(Value::as_str)
        .context("create_local_env_file requires mountPath")?;
    config::resolve_write_path(session, Path::new(mount_path))?;
    Ok(())
}

fn safe_call_summary(tool_name: &str, arguments: &Value) -> String {
    let mut keys = arguments
        .as_object()
        .map(|object| object.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    keys.sort();
    format!(
        "tool: {tool_name}\nargument keys: {}",
        if keys.is_empty() {
            "(none)".to_owned()
        } else {
            keys.join(", ")
        }
    )
}

fn executable_path() -> Result<PathBuf> {
    if let Ok(value) = std::env::var("TEMOTE_MCP_ONEPASSWORD_MCP") {
        let path = PathBuf::from(value);
        anyhow::ensure!(
            path.is_absolute(),
            "TEMOTE_MCP_ONEPASSWORD_MCP must be an absolute path"
        );
        anyhow::ensure!(
            path.is_file(),
            "1Password MCP executable not found: {}",
            path.display()
        );
        return Ok(path);
    }

    #[cfg(target_os = "macos")]
    let path = PathBuf::from("/Applications/1Password.app/Contents/MacOS/1password-mcp");
    #[cfg(target_os = "linux")]
    let path = PathBuf::from("/opt/1Password/1password-mcp");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!("1Password MCP support is currently available on macOS and Linux hosts");

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        anyhow::ensure!(
            path.is_file(),
            "1Password MCP executable not found at {}; enable the Temote MCP server in 1Password Developer settings",
            path.display()
        );
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn generated_client_registry_reaps_finished_entries_and_respects_capacity() -> noprop::TestResult
    {
        test_support::run(0x4f50_434c_4945_4e54, 512, |ctx| {
            let count = noprop::sample_usize_in(ctx, 0..=MAX_CACHED_CLIENTS + 16);
            let mut entries = HashMap::new();
            let mut expected_live = 0usize;
            for index in 0..count {
                let finished = noprop::sample_bool(ctx);
                entries.insert(format!("client-{index}"), finished);
                if !finished {
                    expected_live += 1;
                }
            }

            retain_live_entries(&mut entries, |finished| !*finished);
            assert_eq!(entries.len(), expected_live);
            assert!(entries.values().all(|finished| !*finished));

            let existing = noprop::sample_bool(ctx);
            assert_eq!(
                client_capacity_available(existing, entries.len()),
                existing || entries.len() < MAX_CACHED_CLIENTS
            );
            Ok(())
        })
    }

    #[test]
    fn generated_session_watcher_stops_only_on_explicit_inactive() -> noprop::TestResult {
        test_support::run(0x3150_5741_5443_4845, 512, |ctx| {
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

    #[test]
    fn approval_summary_never_contains_argument_values() {
        let summary = safe_call_summary(
            "append_variables",
            &json!({
                "accountId": "account-secret-ish",
                "environmentId": "environment-secret-ish",
                "variables": [{"name": "TOKEN", "value": "super-secret", "concealed": true}]
            }),
        );
        assert!(summary.contains("accountId"));
        assert!(summary.contains("environmentId"));
        assert!(summary.contains("variables"));
        assert!(!summary.contains("super-secret"));
        assert!(!summary.contains("TOKEN"));
        assert!(!summary.contains("account-secret-ish"));
    }

    #[test]
    fn local_env_file_mounts_stay_inside_normal_session_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = config::canonical_directory(root.path()).unwrap();
        let session = config::Session {
            id: "onepassword-test".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root],
            started_at: 0,
            process_id: 0,
            yolo: false,
        };
        assert!(
            enforce_path_boundary(
                &session,
                "create_local_env_file",
                &json!({"mountPath": "inside.env"}),
            )
            .is_ok()
        );
        assert!(
            enforce_path_boundary(
                &session,
                "create_local_env_file",
                &json!({"mountPath": outside.path().join("outside.env")}),
            )
            .is_err()
        );
    }

    #[test]
    fn generated_approval_summaries_expose_keys_but_never_values() -> noprop::TestResult {
        test_support::run(0x4f50_5355_4d4d_4152, test_support::DEFAULT_CASES, |ctx| {
            let key_a = format!("key_{}", test_support::safe_component(ctx));
            let key_b = format!("key_{}", test_support::safe_component(ctx));
            let value_a = format!(
                "value-{}-{}",
                test_support::safe_component(ctx),
                noprop::sample_u64(ctx)
            );
            let value_b = format!(
                "value-{}-{}",
                test_support::safe_component(ctx),
                noprop::sample_u64(ctx)
            );
            let arguments = json!({
                key_a.clone(): value_a.clone(),
                key_b.clone(): {"nested": value_b.clone()}
            });
            let summary = safe_call_summary("generated_tool", &arguments);
            assert!(summary.contains(&key_a));
            assert!(summary.contains(&key_b));
            assert!(
                !summary.contains(&value_a),
                "leaked {value_a:?} in {summary:?}"
            );
            assert!(
                !summary.contains(&value_b),
                "leaked {value_b:?} in {summary:?}"
            );

            let mut expected_keys = [key_a, key_b];
            expected_keys.sort();
            assert!(
                summary.ends_with(&expected_keys.join(", ")),
                "summary={summary:?}"
            );
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn generated_local_env_mounts_fail_closed_on_outside_and_symlink_paths() -> noprop::TestResult {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let root = config::canonical_directory(&root).unwrap();
        let session = config::Session {
            id: "onepassword-pbt".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root],
            started_at: 0,
            process_id: 0,
            yolo: false,
        };

        test_support::run(0x4f50_4d4f_554e_5401, 512, |ctx| {
            let leaf = format!("{}.env", test_support::safe_component(ctx));
            assert!(
                enforce_path_boundary(
                    &session,
                    "create_local_env_file",
                    &json!({"mountPath": leaf}),
                )
                .is_ok()
            );

            let escaped = if noprop::sample_bool(ctx) {
                outside.join(format!("{}.env", test_support::safe_component(ctx)))
            } else {
                PathBuf::from(format!("escape/{}.env", test_support::safe_component(ctx)))
            };
            assert!(
                enforce_path_boundary(
                    &session,
                    "create_local_env_file",
                    &json!({"mountPath": escaped}),
                )
                .is_err()
            );
            Ok(())
        })
    }
}
