use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::config;
use crate::line_protocol::{
    BoundedLine, ChildMessageKind, MAX_JSON_LINE_BYTES, RequestIdSequence, classify_child_message,
    encode_bounded_json_line, next_bounded_line, validate_child_tool_call,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const PROTOCOL_VERSION: &str = "2025-06-18";
const KINTONE_ENV_NAMES: &[&str] = &[
    "KINTONE_BASE_URL",
    "KINTONE_USERNAME",
    "KINTONE_PASSWORD",
    "KINTONE_API_TOKEN",
    "KINTONE_BASIC_AUTH_USERNAME",
    "KINTONE_BASIC_AUTH_PASSWORD",
    "KINTONE_PFX_FILE_PATH",
    "KINTONE_PFX_FILE_PASSWORD",
    "KINTONE_ATTACHMENTS_DIR",
    "HTTPS_PROXY",
    "https_proxy",
];
const CHILD_RUNTIME_ENV_NAMES: &[&str] = &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"];

pub struct Bridge {
    executable_override: Option<PathBuf>,
    environment: BTreeMap<String, String>,
    client: Option<Client>,
}

impl Bridge {
    pub fn capture() -> Self {
        let executable_override = std::env::var_os("TEMOTE_MCP_KINTONE_MCP").map(PathBuf::from);
        let environment = KINTONE_ENV_NAMES
            .iter()
            .chain(CHILD_RUNTIME_ENV_NAMES.iter())
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(|value| ((*name).to_owned(), value))
            })
            .collect();
        Self {
            executable_override,
            environment,
            client: None,
        }
    }

    pub fn configured(&self) -> bool {
        self.environment
            .get("KINTONE_BASE_URL")
            .is_some_and(|value| !value.trim().is_empty())
            && self.auth_mode().is_some()
    }

    pub fn status(&self, session: &config::Session) -> Value {
        let executable_found = self.executable_path().is_ok();
        let configuration_valid = self.validated_environment(session).is_ok();
        json!({
            "configured": self.configured(),
            "configuration_valid": configuration_valid,
            "executable_found": executable_found,
            "auth_mode": self.auth_mode(),
            "basic_auth_configured": self.environment.contains_key("KINTONE_BASIC_AUTH_USERNAME")
                || self.environment.contains_key("KINTONE_BASIC_AUTH_PASSWORD"),
            "client_certificate_configured": self.environment.contains_key("KINTONE_PFX_FILE_PATH"),
            "attachments_dir_configured": self.environment.contains_key("KINTONE_ATTACHMENTS_DIR"),
            "proxy_configured": self.environment.contains_key("HTTPS_PROXY")
                || self.environment.contains_key("https_proxy"),
        })
    }

    pub async fn discover(&mut self, session: &config::Session) -> Result<Value> {
        self.ensure_client(session).await?;
        let result = self
            .client
            .as_mut()
            .context("kintone MCP client disappeared")?
            .request("tools/list", json!({}))
            .await;
        match result {
            Ok(value) => Ok(json!({"tools": value["tools"]})),
            Err(error) => {
                self.client = None;
                Err(error)
            }
        }
    }

    pub async fn call_tool(
        &mut self,
        session: &config::Session,
        tool_name: &str,
        arguments: Value,
    ) -> Result<Value> {
        validate_child_tool_call(tool_name, &arguments).context("invalid kintone MCP tool call")?;
        self.ensure_client(session).await?;

        let listed = self
            .client
            .as_mut()
            .context("kintone MCP client disappeared")?
            .request("tools/list", json!({}))
            .await;
        let listed = match listed {
            Ok(value) => value,
            Err(error) => {
                self.client = None;
                return Err(error);
            }
        };
        let known = listed["tools"].as_array().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool["name"].as_str() == Some(tool_name))
        });
        anyhow::ensure!(known, "unknown kintone MCP tool: {tool_name}");

        let result = self
            .client
            .as_mut()
            .context("kintone MCP client disappeared")?
            .request(
                "tools/call",
                json!({"name": tool_name, "arguments": arguments}),
            )
            .await;
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                self.client = None;
                Err(error)
            }
        }
    }

    async fn ensure_client(&mut self, session: &config::Session) -> Result<()> {
        let environment = self.validated_environment(session)?;
        if self.client.is_none() {
            let executable = self.executable_path()?;
            self.client = Some(Client::spawn(&executable, &session.cwd, &environment).await?);
        }
        Ok(())
    }

    fn auth_mode(&self) -> Option<&'static str> {
        let username = self
            .environment
            .get("KINTONE_USERNAME")
            .is_some_and(|value| !value.trim().is_empty());
        let password = self
            .environment
            .get("KINTONE_PASSWORD")
            .is_some_and(|value| !value.trim().is_empty());
        if username && password {
            return Some("password");
        }
        self.environment
            .get("KINTONE_API_TOKEN")
            .is_some_and(|value| !value.trim().is_empty())
            .then_some("api_token")
    }

    fn validated_environment(&self, session: &config::Session) -> Result<BTreeMap<String, String>> {
        let base_url = self
            .environment
            .get("KINTONE_BASE_URL")
            .filter(|value| !value.trim().is_empty())
            .context(
                "kintone MCP is not configured; start the session with KINTONE_BASE_URL set",
            )?;
        anyhow::ensure!(
            base_url.starts_with("https://") || base_url.starts_with("http://"),
            "KINTONE_BASE_URL must be an http:// or https:// URL"
        );
        anyhow::ensure!(
            self.auth_mode().is_some(),
            "kintone MCP authentication is not configured; set KINTONE_USERNAME and KINTONE_PASSWORD, or KINTONE_API_TOKEN, when starting the session"
        );
        anyhow::ensure!(
            self.environment.contains_key("KINTONE_USERNAME")
                == self.environment.contains_key("KINTONE_PASSWORD"),
            "KINTONE_USERNAME and KINTONE_PASSWORD must be set together"
        );
        anyhow::ensure!(
            self.environment.contains_key("KINTONE_BASIC_AUTH_USERNAME")
                == self.environment.contains_key("KINTONE_BASIC_AUTH_PASSWORD"),
            "KINTONE_BASIC_AUTH_USERNAME and KINTONE_BASIC_AUTH_PASSWORD must be set together"
        );
        anyhow::ensure!(
            self.environment.contains_key("KINTONE_PFX_FILE_PATH")
                == self.environment.contains_key("KINTONE_PFX_FILE_PASSWORD"),
            "KINTONE_PFX_FILE_PATH and KINTONE_PFX_FILE_PASSWORD must be set together"
        );

        let mut environment = self.environment.clone();
        if let Some(path) = self.environment.get("KINTONE_PFX_FILE_PATH") {
            let resolved = config::resolve_existing_path(session, Path::new(path))?;
            anyhow::ensure!(
                resolved.is_file(),
                "KINTONE_PFX_FILE_PATH must point to a file"
            );
            environment.insert(
                "KINTONE_PFX_FILE_PATH".to_owned(),
                resolved.display().to_string(),
            );
        }
        if let Some(path) = self.environment.get("KINTONE_ATTACHMENTS_DIR") {
            let path = Path::new(path);
            let resolved = if path.exists() {
                config::resolve_existing_path(session, path)?
            } else {
                config::resolve_write_path(session, path)?
            };
            if resolved.exists() {
                anyhow::ensure!(
                    resolved.is_dir(),
                    "KINTONE_ATTACHMENTS_DIR must point to a directory"
                );
            }
            environment.insert(
                "KINTONE_ATTACHMENTS_DIR".to_owned(),
                resolved.display().to_string(),
            );
        }
        Ok(environment)
    }

    fn executable_path(&self) -> Result<PathBuf> {
        if let Some(path) = &self.executable_override {
            anyhow::ensure!(
                path.is_absolute(),
                "TEMOTE_MCP_KINTONE_MCP must be an absolute path"
            );
            anyhow::ensure!(
                path.is_file(),
                "kintone MCP executable not found: {}",
                path.display()
            );
            return Ok(path.clone());
        }
        let path = self
            .environment
            .get("PATH")
            .and_then(|path| find_on_path("kintone-mcp-server", path));
        path.context(
            "kintone-mcp-server was not found in PATH; install @kintone/mcp-server globally or set TEMOTE_MCP_KINTONE_MCP to its absolute executable path",
        )
    }
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_ids: RequestIdSequence,
}

impl Client {
    async fn spawn(
        executable: &Path,
        cwd: &Path,
        environment: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        command
            .current_dir(cwd)
            .env_clear()
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start kintone MCP server at {}",
                executable.display()
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .context("kintone MCP stdin is unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("kintone MCP stdout is unavailable")?;
        let mut client = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            request_ids: RequestIdSequence::default(),
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
            .context("failed to initialize kintone MCP server")?;
        client
            .notify("notifications/initialized", json!({}))
            .await
            .context("failed to finish kintone MCP initialization")?;
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
        let id = self.request_ids.take();
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        tokio::time::timeout(REQUEST_TIMEOUT, async {
            loop {
                let line = match next_bounded_line(&mut self.stdout, MAX_JSON_LINE_BYTES).await? {
                    Some(BoundedLine::Line(line)) => line,
                    Some(BoundedLine::TooLarge) => {
                        anyhow::bail!(
                            "kintone MCP server response exceeds {MAX_JSON_LINE_BYTES} bytes"
                        )
                    }
                    Some(BoundedLine::InvalidUtf8) => {
                        anyhow::bail!("kintone MCP server returned invalid UTF-8")
                    }
                    None => {
                        let status = self.child.try_wait().ok().flatten();
                        anyhow::bail!("kintone MCP server closed stdout (status: {status:?})")
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let message: Value = serde_json::from_str(&line)
                    .context("kintone MCP server returned invalid JSON")?;
                match classify_child_message(&message, id)? {
                    ChildMessageKind::Response => {
                        if let Some(error) = message.get("error") {
                            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32000);
                            let message = error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown kintone MCP error");
                            anyhow::bail!("kintone MCP error {code}: {message}")
                        }
                        return message
                            .get("result")
                            .cloned()
                            .context("kintone MCP response is missing result");
                    }
                    ChildMessageKind::ServerRequest(request_id) => {
                        self.write_json(&json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "error": {
                                "code": -32601,
                                "message": "temote-mcp does not expose client-side MCP capabilities to kintone"
                            }
                        }))
                        .await?;
                    }
                    ChildMessageKind::Notification => {}
                }
            }
        })
        .await
        .context("timed out waiting for kintone MCP server")?
    }

    async fn write_json(&mut self, value: &Value) -> Result<()> {
        let line = encode_bounded_json_line(value, MAX_JSON_LINE_BYTES)?;
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

fn find_on_path(executable: &str, path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    fn bridge(environment: &[(&str, &str)]) -> Bridge {
        Bridge {
            executable_override: None,
            environment: environment
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            client: None,
        }
    }

    fn session(root: &Path) -> config::Session {
        let root = config::canonical_directory(root).unwrap();
        config::Session {
            id: "kintone-test".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root],
            started_at: 0,
            process_id: 0,
            yolo: false,
        }
    }

    #[test]
    fn accepts_password_or_api_token_authentication_without_exposing_values() {
        let password = bridge(&[
            ("KINTONE_BASE_URL", "https://example.cybozu.com"),
            ("KINTONE_USERNAME", "user"),
            ("KINTONE_PASSWORD", "secret"),
        ]);
        assert!(password.configured());
        assert_eq!(password.auth_mode(), Some("password"));

        let token = bridge(&[
            ("KINTONE_BASE_URL", "https://example.cybozu.com"),
            ("KINTONE_API_TOKEN", "secret-token"),
        ]);
        assert!(token.configured());
        assert_eq!(token.auth_mode(), Some("api_token"));
        let status = token.status(&session(tempfile::tempdir().unwrap().path()));
        let rendered = status.to_string();
        assert!(!rendered.contains("secret-token"));
        assert!(!rendered.contains("example.cybozu.com"));
    }

    #[test]
    fn normal_sessions_reject_kintone_file_paths_outside_permitted_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let pfx = outside.path().join("client.pfx");
        std::fs::write(&pfx, b"fake").unwrap();
        let bridge = bridge(&[
            ("KINTONE_BASE_URL", "https://example.cybozu.com"),
            ("KINTONE_API_TOKEN", "secret-token"),
            ("KINTONE_PFX_FILE_PATH", pfx.to_str().unwrap()),
        ]);
        let session = session(root.path());
        assert!(bridge.validated_environment(&session).is_err());
    }

    #[test]
    fn attachments_directory_must_stay_inside_permitted_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let bridge = bridge(&[
            ("KINTONE_BASE_URL", "https://example.cybozu.com"),
            ("KINTONE_API_TOKEN", "secret-token"),
            (
                "KINTONE_ATTACHMENTS_DIR",
                outside.path().join("downloads").to_str().unwrap(),
            ),
        ]);
        let session = session(root.path());
        assert!(bridge.validated_environment(&session).is_err());
    }

    #[test]
    fn generated_authentication_matrix_matches_bridge_policy() -> noprop::TestResult {
        test_support::run(0x4b4d_4350_4155_5448, test_support::DEFAULT_CASES, |ctx| {
            let username = match noprop::sample_usize_in(ctx, 0..=2) {
                0 => None,
                1 => Some(String::new()),
                _ => Some(test_support::safe_component(ctx)),
            };
            let password = match noprop::sample_usize_in(ctx, 0..=2) {
                0 => None,
                1 => Some("   ".to_owned()),
                _ => Some(test_support::safe_component(ctx)),
            };
            let token = match noprop::sample_usize_in(ctx, 0..=2) {
                0 => None,
                1 => Some(String::new()),
                _ => Some(test_support::safe_component(ctx)),
            };
            let mut environment =
                vec![("KINTONE_BASE_URL", "https://example.cybozu.com".to_owned())];
            if let Some(value) = &username {
                environment.push(("KINTONE_USERNAME", value.clone()));
            }
            if let Some(value) = &password {
                environment.push(("KINTONE_PASSWORD", value.clone()));
            }
            if let Some(value) = &token {
                environment.push(("KINTONE_API_TOKEN", value.clone()));
            }
            let bridge = Bridge {
                executable_override: None,
                environment: environment
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect(),
                client: None,
            };
            let root = tempfile::tempdir().unwrap();
            let session = session(root.path());

            let password_auth = username.as_deref().is_some_and(|v| !v.trim().is_empty())
                && password.as_deref().is_some_and(|v| !v.trim().is_empty());
            let token_auth = token.as_deref().is_some_and(|v| !v.trim().is_empty());
            let pair_shape = username.is_some() == password.is_some();
            let expected = (password_auth || token_auth) && pair_shape;
            assert_eq!(
                bridge.validated_environment(&session).is_ok(),
                expected,
                "username={username:?} password_present={} token_present={}",
                password.is_some(),
                token.is_some()
            );
            Ok(())
        })
    }

    #[test]
    fn generated_status_never_exposes_kintone_credentials() -> noprop::TestResult {
        test_support::run(0x4b4d_4350_5354_4154, 512, |ctx| {
            let host_secret = format!("{}.secret.example", test_support::safe_component(ctx));
            let token_secret = format!(
                "token-{}-{}",
                test_support::safe_component(ctx),
                noprop::sample_u64(ctx)
            );
            let user_secret = format!("user-{}", test_support::safe_component(ctx));
            let password_secret = format!(
                "password-{}-{}",
                test_support::safe_component(ctx),
                noprop::sample_u64(ctx)
            );
            let bridge = Bridge {
                executable_override: None,
                environment: [
                    (
                        "KINTONE_BASE_URL".to_owned(),
                        format!("https://{host_secret}"),
                    ),
                    ("KINTONE_USERNAME".to_owned(), user_secret.clone()),
                    ("KINTONE_PASSWORD".to_owned(), password_secret.clone()),
                    ("KINTONE_API_TOKEN".to_owned(), token_secret.clone()),
                ]
                .into_iter()
                .collect(),
                client: None,
            };
            let root = tempfile::tempdir().unwrap();
            let rendered = bridge.status(&session(root.path())).to_string();
            for secret in [&host_secret, &token_secret, &user_secret, &password_secret] {
                assert!(
                    !rendered.contains(secret),
                    "secret leaked in status: {secret:?}"
                );
            }
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn generated_kintone_paths_reject_symlink_escape() -> noprop::TestResult {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let session = session(&root);

        test_support::run(0x4b4d_4350_5041_5448, 512, |ctx| {
            let leaf = test_support::safe_component(ctx);
            let bridge = Bridge {
                executable_override: None,
                environment: [
                    (
                        "KINTONE_BASE_URL".to_owned(),
                        "https://example.cybozu.com".to_owned(),
                    ),
                    ("KINTONE_API_TOKEN".to_owned(), "token".to_owned()),
                    (
                        "KINTONE_ATTACHMENTS_DIR".to_owned(),
                        format!("escape/{leaf}"),
                    ),
                ]
                .into_iter()
                .collect(),
                client: None,
            };
            assert!(
                bridge.validated_environment(&session).is_err(),
                "symlink escape accepted: {leaf:?}"
            );
            Ok(())
        })
    }
}
