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

use crate::line_protocol::{BoundedLine, next_bounded_line};
use crate::{child_env, config, sandbox};

pub const MAX_SECRET_REFERENCES: usize = 100;
const MAX_REFERENCE_BYTES: usize = 4 * 1024;
const MAX_REFERENCE_INPUT_BYTES: usize = 128 * 1024;
const MAX_ACCOUNT_BYTES: usize = 4 * 1024;
const SIDECAR_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const SIDECAR_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CACHED_CLIENTS: usize = 64;
const SIDECAR_BINARY_NAME: &str = "temote-onepassword-sdk";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveRequest {
    pub account: String,
    pub references: Vec<String>,
}

impl ResolveRequest {
    pub fn new(account: String, references: Vec<String>) -> Result<Self> {
        validate_account(&account)?;
        validate_references(&references)?;
        Ok(Self {
            account,
            references,
        })
    }

    pub fn approval_summary(&self) -> String {
        format!("references: {}\naccount: configured", self.references.len())
    }
}

struct Client {
    child: Arc<Mutex<Child>>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_id: u64,
    session_watcher: JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.session_watcher.abort();
    }
}

fn clients() -> &'static Mutex<HashMap<String, Arc<Mutex<Client>>>> {
    static CLIENTS: OnceLock<Mutex<HashMap<String, Arc<Mutex<Client>>>>> = OnceLock::new();
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Client {
    async fn spawn(session: &config::Session) -> Result<Self> {
        let executable = sidecar_executable()?;
        let mut command = sidecar_command(&executable, &session.cwd);
        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start 1Password SDK sidecar at {}",
                executable.display()
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .context("1Password SDK sidecar stdin is unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("1Password SDK sidecar stdout is unavailable")?;
        let child = Arc::new(Mutex::new(child));
        let watched_child = Arc::clone(&child);
        let watched_session_id = session.id.clone();
        let session_watcher = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if matches!(
                    config::session_is_active(&watched_session_id).await,
                    Ok(false)
                ) {
                    let mut child = watched_child.lock().await;
                    let _ = child.kill().await;
                    return;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: 1,
            session_watcher,
        })
    }

    async fn resolve(
        &mut self,
        request: &ResolveRequest,
    ) -> std::result::Result<Vec<String>, SdkResolveError> {
        let id = self.request_id;
        self.request_id = self.request_id.wrapping_add(1).max(1);
        let line = serde_json::to_vec(&json!({
            "id": id,
            "account": request.account,
            "references": request.references,
        }))
        .map_err(|error| SdkResolveError::Unavailable(error.into()))?;
        if line.len() > MAX_REFERENCE_INPUT_BYTES + 16 * 1024 {
            return Err(SdkResolveError::Unavailable(anyhow::anyhow!(
                "1Password SDK sidecar request is too large"
            )));
        }
        self.stdin
            .write_all(&line)
            .await
            .map_err(|error| SdkResolveError::Unavailable(error.into()))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|error| SdkResolveError::Unavailable(error.into()))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| SdkResolveError::Unavailable(error.into()))?;

        let response =
            tokio::time::timeout(SIDECAR_TIMEOUT, read_bounded_line(&mut self.stdout)).await;
        let response = match response {
            Ok(result) => result.map_err(SdkResolveError::Unavailable)?,
            Err(_) => {
                let mut child = self.child.lock().await;
                let _ = child.kill().await;
                return Err(SdkResolveError::Unavailable(anyhow::anyhow!(
                    "1Password SDK sidecar timed out"
                )));
            }
        };
        let value: Value = serde_json::from_slice(&response)
            .context("1Password SDK sidecar returned invalid JSON")
            .map_err(SdkResolveError::Unavailable)?;
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(SdkResolveError::Unavailable(anyhow::anyhow!(
                "1Password SDK sidecar response ID mismatch"
            )));
        }
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(SdkResolveError::Unavailable(anyhow::anyhow!(
                "{}",
                safe_sdk_error(error)
            )));
        }
        let result = value
            .get("result")
            .context("1Password SDK sidecar response is missing result")
            .map_err(SdkResolveError::Unavailable)?;
        extract_secrets(result, &request.references).map_err(SdkResolveError::Reference)
    }
}

enum SdkResolveError {
    Unavailable(anyhow::Error),
    Reference(anyhow::Error),
}

pub async fn resolve(session: &config::Session, request: &ResolveRequest) -> Result<Vec<String>> {
    let sdk_client = {
        let mut registry = clients().lock().await;
        registry.retain(|_, client| {
            client
                .try_lock()
                .map(|client| !client.session_watcher.is_finished())
                .unwrap_or(true)
        });
        if let Some(client) = registry.get(&session.id) {
            Some(Arc::clone(client))
        } else if registry.len() < MAX_CACHED_CLIENTS {
            match Client::spawn(session).await {
                Ok(client) => {
                    let client = Arc::new(Mutex::new(client));
                    registry.insert(session.id.clone(), Arc::clone(&client));
                    Some(client)
                }
                Err(_) => None,
            }
        } else {
            None
        }
    };

    if let Some(client) = sdk_client {
        let sdk_result = {
            let mut client = client.lock().await;
            client.resolve(request).await
        };
        match sdk_result {
            Ok(values) => return Ok(values),
            Err(SdkResolveError::Reference(error)) => return Err(error),
            Err(SdkResolveError::Unavailable(_error)) => {
                let mut registry = clients().lock().await;
                if registry
                    .get(&session.id)
                    .is_some_and(|current| Arc::ptr_eq(current, &client))
                {
                    registry.remove(&session.id);
                }
            }
        }
    }
    resolve_with_cli(session, request).await
}

async fn resolve_with_cli(
    session: &config::Session,
    request: &ResolveRequest,
) -> Result<Vec<String>> {
    let helper = sidecar_executable()?;
    let mut environment = HashMap::with_capacity(request.references.len());
    for (index, reference) in request.references.iter().enumerate() {
        environment.insert(format!("TEMOTE_MCP_OP_REF_{index:03}"), reference.clone());
    }
    let command = vec![
        "op".to_owned(),
        "run".to_owned(),
        "--no-masking".to_owned(),
        "--account".to_owned(),
        request.account.clone(),
        "--".to_owned(),
        helper.to_string_lossy().into_owned(),
        "--emit-env-json".to_owned(),
        request.references.len().to_string(),
    ];
    let output = sandbox::run_unrestricted_with_env(
        &command,
        &session.cwd,
        None,
        &environment,
        child_env::SENSITIVE_ENV_NAMES,
    )
    .await
    .context("failed to run 1Password CLI secret fallback")?;
    anyhow::ensure!(
        !output.truncated,
        "1Password CLI secret fallback output exceeded {} bytes",
        sandbox::MAX_COMMAND_OUTPUT_BYTES
    );
    anyhow::ensure!(
        output.status == 0,
        "1Password CLI failed to resolve secret references"
    );
    let values: Vec<String> = serde_json::from_str(&output.stdout)
        .context("1Password CLI secret fallback returned invalid JSON")?;
    anyhow::ensure!(
        values.len() == request.references.len(),
        "1Password CLI secret fallback returned an unexpected number of values"
    );
    Ok(values)
}

fn sidecar_command(executable: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    child_env::scrub_sensitive(&mut command, &[]);
    command
}

fn sidecar_executable() -> Result<PathBuf> {
    if let Ok(value) = std::env::var("TEMOTE_MCP_ONEPASSWORD_SDK_SIDECAR") {
        let path = PathBuf::from(value);
        anyhow::ensure!(
            path.is_absolute(),
            "TEMOTE_MCP_ONEPASSWORD_SDK_SIDECAR must be absolute"
        );
        anyhow::ensure!(
            path.is_file(),
            "1Password SDK sidecar not found: {}",
            path.display()
        );
        return Ok(path);
    }
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .context("temote-mcp executable has no parent directory")?;
    let candidates = if directory.file_name().is_some_and(|name| name == "deps") {
        directory
            .parent()
            .map(|profile| {
                vec![
                    directory.join(SIDECAR_BINARY_NAME),
                    profile.join(SIDECAR_BINARY_NAME),
                ]
            })
            .unwrap_or_else(|| vec![directory.join(SIDECAR_BINARY_NAME)])
    } else {
        vec![directory.join(SIDECAR_BINARY_NAME)]
    };
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .with_context(|| {
            format!(
                "1Password SDK sidecar {SIDECAR_BINARY_NAME} is missing next to {}",
                executable.display()
            )
        })
}

async fn read_bounded_line(reader: &mut BufReader<ChildStdout>) -> Result<Vec<u8>> {
    match next_bounded_line(reader, SIDECAR_RESPONSE_BYTES).await? {
        Some(BoundedLine::Line(line)) => Ok(line.into_bytes()),
        Some(BoundedLine::TooLarge) => anyhow::bail!("1Password SDK sidecar response is too large"),
        Some(BoundedLine::InvalidUtf8) => {
            anyhow::bail!("1Password SDK sidecar returned invalid UTF-8")
        }
        None => anyhow::bail!("1Password SDK sidecar closed stdout"),
    }
}

fn extract_secrets(result: &Value, references: &[String]) -> Result<Vec<String>> {
    let responses = result
        .get("individualResponses")
        .and_then(Value::as_object)
        .context("1Password SDK resolve response is missing individualResponses")?;
    references
        .iter()
        .map(|reference| {
            let response = responses
                .get(reference)
                .context("1Password SDK resolve response is missing a requested reference")?;
            if response.get("error").is_some_and(|error| !error.is_null()) {
                anyhow::bail!("1Password SDK could not resolve one or more secret references");
            }
            response
                .pointer("/content/secret")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .context("1Password SDK resolve response is missing secret content")
        })
        .collect()
}

fn validate_account(account: &str) -> Result<()> {
    anyhow::ensure!(!account.is_empty(), "account must not be empty");
    anyhow::ensure!(
        account.len() <= MAX_ACCOUNT_BYTES,
        "account exceeds {MAX_ACCOUNT_BYTES} bytes"
    );
    anyhow::ensure!(
        !account.chars().any(char::is_control),
        "account must not contain control characters"
    );
    Ok(())
}

fn validate_references(references: &[String]) -> Result<()> {
    anyhow::ensure!(!references.is_empty(), "references must not be empty");
    anyhow::ensure!(
        references.len() <= MAX_SECRET_REFERENCES,
        "references must contain at most {MAX_SECRET_REFERENCES} entries"
    );
    let mut total = 0usize;
    for reference in references {
        anyhow::ensure!(
            reference.starts_with("op://"),
            "secret references must start with op://"
        );
        anyhow::ensure!(
            reference.len() <= MAX_REFERENCE_BYTES,
            "secret reference exceeds {MAX_REFERENCE_BYTES} bytes"
        );
        anyhow::ensure!(
            !reference.chars().any(char::is_control),
            "secret references must not contain control characters"
        );
        total = total
            .checked_add(reference.len())
            .context("secret reference size overflow")?;
        anyhow::ensure!(
            total <= MAX_REFERENCE_INPUT_BYTES,
            "secret references exceed {MAX_REFERENCE_INPUT_BYTES} bytes in total"
        );
    }
    Ok(())
}

fn safe_sdk_error(error: &str) -> &'static str {
    if error.contains("Denied authorization") || error.contains("authorization was denied") {
        "1Password SDK desktop authorization was denied"
    } else if error.contains("DesktopSessionExpired") || error.contains("desktop session expired") {
        "1Password SDK desktop session expired"
    } else {
        "1Password SDK secret resolution failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_request_bounds_and_approval_summary_hide_values() {
        let request = ResolveRequest::new(
            "private-account".to_owned(),
            vec![
                "op://Vault/Item/password".to_owned(),
                "op://Vault/Item/token".to_owned(),
            ],
        )
        .unwrap();
        let summary = request.approval_summary();
        assert_eq!(summary, "references: 2\naccount: configured");
        assert!(!summary.contains("private-account"));
        assert!(!summary.contains("Vault"));
        assert!(ResolveRequest::new(String::new(), vec!["op://a/b/c".to_owned()]).is_err());
        assert!(ResolveRequest::new("a".to_owned(), vec!["not-a-reference".to_owned()]).is_err());
        assert!(
            ResolveRequest::new(
                "a".to_owned(),
                vec!["op://a/b/c".to_owned(); MAX_SECRET_REFERENCES + 1]
            )
            .is_err()
        );
    }

    #[test]
    fn extracts_sdk_secrets_in_request_order_and_supports_duplicates() {
        let result = json!({
            "individualResponses": {
                "op://v/i/a": {"content": {"secret": "alpha", "itemId": "i", "vaultId": "v"}},
                "op://v/i/b": {"content": {"secret": "beta", "itemId": "i", "vaultId": "v"}}
            }
        });
        let refs = vec![
            "op://v/i/b".to_owned(),
            "op://v/i/a".to_owned(),
            "op://v/i/b".to_owned(),
        ];
        assert_eq!(
            extract_secrets(&result, &refs).unwrap(),
            vec!["beta", "alpha", "beta"]
        );
    }

    #[test]
    fn sdk_errors_never_reflect_raw_details() {
        assert_eq!(
            safe_sdk_error("Denied authorization for SDK client account-secret"),
            "1Password SDK desktop authorization was denied"
        );
        assert_eq!(
            safe_sdk_error("reference op://secret/path exploded"),
            "1Password SDK secret resolution failed"
        );
    }

    #[test]
    fn sidecar_command_scrubs_sensitive_environment() {
        use std::ffi::OsStr;
        let mut command = sidecar_command(Path::new("sidecar"), Path::new("."));
        for name in child_env::SENSITIVE_ENV_NAMES {
            command.env(name, "sentinel");
        }
        child_env::scrub_sensitive(&mut command, &[]);
        let envs = command.as_std().get_envs().collect::<Vec<_>>();
        for name in child_env::SENSITIVE_ENV_NAMES {
            let value = envs
                .iter()
                .find(|(key, _)| *key == OsStr::new(name))
                .map(|(_, value)| *value);
            assert_eq!(value, Some(None), "credential leaked: {name}");
        }
    }
}
