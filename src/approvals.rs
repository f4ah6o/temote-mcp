use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::{self, Session};
use crate::{kintone_cli, kintone_mcp, sandbox, secret_broker};

const MAX_SESSION_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_APPROVAL_RESPONSE_BYTES: usize = 64;
const SESSION_MESSAGE_READ_TIMEOUT: Duration = Duration::from_secs(2);
const SESSION_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_PENDING_SESSION_READS: usize = 64;
const MAX_PENDING_APPROVALS: usize = 64;
const MAX_SERVICE_ACCOUNT_COMMAND_ARGUMENTS: usize = 256;
const MAX_SERVICE_ACCOUNT_COMMAND_ARGUMENT_BYTES: usize = 32 * 1024;
const MAX_SERVICE_ACCOUNT_COMMAND_TOTAL_BYTES: usize = 128 * 1024;
const MAX_SERVICE_ACCOUNT_ENV_FILES: usize = 32;
const MAX_SERVICE_ACCOUNT_ENV_FILE_PATH_BYTES: usize = 4096;
const MAX_SERVICE_ACCOUNT_ENV_VARS: usize = 64;
const MAX_SERVICE_ACCOUNT_ENV_NAME_BYTES: usize = 128;
const MAX_SERVICE_ACCOUNT_ENV_REF_BYTES: usize = 4096;
const MAX_SERVICE_ACCOUNT_ALLOWED_LOCATORS: usize = 128;
const REDACTED_TRUNCATED_SECRET_OUTPUT: &str = "[REDACTED_TRUNCATED_SECRET_OUTPUT]";
const MAX_ACTIVITY_TITLE_BYTES: usize = 512;
const MAX_ACTIVITY_DETAIL_BYTES: usize = 16 * 1024;
const MAX_APPROVAL_OPERATION_BYTES: usize = 256;
const MAX_APPROVAL_DETAIL_BYTES: usize = 64 * 1024;
const MAX_PENDING_APPROVAL_PROMPTS: usize = 128;
const MAX_PENDING_RUNTIME_COMMANDS: usize = 64;
#[cfg(test)]
const MAX_CONSOLE_PATH_BYTES: usize = 4096;
const MAX_CAPTURED_START_ENV_VALUE_BYTES: usize = 32 * 1024;
const MAX_CAPTURED_START_ENV_TOTAL_BYTES: usize = 56 * 1024;
const SERVICE_ACCOUNT_TOKEN_FD_ENV: &str = "TEMOTE_MCP_OP_SERVICE_ACCOUNT_TOKEN_FD";
static INHERITED_SERVICE_ACCOUNT_TOKEN: OnceLock<String> = OnceLock::new();

const CAPTURED_START_ENV_NAMES: &[&str] = &[
    "OP_SERVICE_ACCOUNT_TOKEN",
    "KINTONE_BASE_URL",
    "KINTONE_USERNAME",
    "KINTONE_PASSWORD",
    "KINTONE_API_TOKEN",
    "KINTONE_BASIC_AUTH_USERNAME",
    "KINTONE_BASIC_AUTH_PASSWORD",
    "KINTONE_PFX_FILE_PATH",
    "KINTONE_PFX_FILE_PASSWORD",
    "KINTONE_ATTACHMENTS_DIR",
    "KINTONE_GUEST_SPACE_ID",
    "HTTPS_PROXY",
    "https_proxy",
    "TEMOTE_MCP_KINTONE_MCP",
    "TEMOTE_MCP_KINTONE_CLI",
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
];

pub fn bootstrap_service_account_process_boundary() -> Result<()> {
    let startup_token_present = std::env::var_os("OP_SERVICE_ACCOUNT_TOKEN").is_some();
    let inherited_fd = std::env::var_os(SERVICE_ACCOUNT_TOKEN_FD_ENV);
    if startup_token_present || inherited_fd.is_some() {
        sandbox::protect_current_process_from_peer_inspection()?;
    }

    #[cfg(target_os = "linux")]
    if let Some(raw_fd) = inherited_fd {
        anyhow::ensure!(
            !startup_token_present,
            "service-account credential handoff is ambiguous"
        );
        let token = read_service_account_token_handoff(&raw_fd)?;
        INHERITED_SERVICE_ACCOUNT_TOKEN.set(token).map_err(|_| {
            anyhow::anyhow!("service-account credential handoff was already initialized")
        })?;
    }

    #[cfg(not(target_os = "linux"))]
    if inherited_fd.is_some() {
        anyhow::bail!("service-account credential FD handoff is unsupported on this platform");
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn read_service_account_token_handoff(raw_fd: &std::ffi::OsStr) -> Result<String> {
    use std::os::unix::fs::MetadataExt;

    let raw_fd = raw_fd
        .to_str()
        .context("service-account credential FD is not valid UTF-8")?;
    let fd = raw_fd
        .parse::<libc::c_int>()
        .context("service-account credential FD is invalid")?;
    anyhow::ensure!(fd >= 3, "service-account credential FD is invalid");

    let mut file = unsafe { File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .context("failed to inspect service-account credential FD")?;
    anyhow::ensure!(
        metadata.is_file() && metadata.mode() & 0o777 == 0,
        "service-account credential FD is not a protected anonymous file"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_CAPTURED_START_ENV_VALUE_BYTES as u64,
        "service-account credential exceeds {MAX_CAPTURED_START_ENV_VALUE_BYTES} bytes"
    );
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    anyhow::ensure!(seals >= 0, "service-account credential FD is not sealable");
    let required_seals =
        libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    anyhow::ensure!(
        seals & required_seals == required_seals,
        "service-account credential FD is not fully sealed"
    );

    file.seek(SeekFrom::Start(0))?;
    let mut token = String::new();
    file.take((MAX_CAPTURED_START_ENV_VALUE_BYTES + 1) as u64)
        .read_to_string(&mut token)
        .context("failed to read service-account credential handoff")?;
    anyhow::ensure!(
        !token.is_empty() && token.len() <= MAX_CAPTURED_START_ENV_VALUE_BYTES,
        "service-account credential handoff is empty or oversized"
    );
    Ok(token)
}

#[cfg(target_os = "linux")]
fn create_service_account_token_handoff(token: &str) -> Result<File> {
    use std::ffi::CString;

    anyhow::ensure!(
        !token.is_empty() && token.len() <= MAX_CAPTURED_START_ENV_VALUE_BYTES,
        "service-account credential is empty or oversized"
    );
    let name = CString::new("temote-service-account-token")?;
    let mut fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to create service-account credential handoff");
    }
    if fd < 3 {
        let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD, 3) };
        let duplicate_error = if duplicated < 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        unsafe {
            libc::close(fd);
        }
        if let Some(error) = duplicate_error {
            return Err(error).context("failed to reserve service-account credential FD");
        }
        fd = duplicated;
    }

    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(token.as_bytes())?;
    file.flush()?;
    file.seek(SeekFrom::Start(0))?;
    let chmod = unsafe { libc::fchmod(file.as_raw_fd(), 0) };
    if chmod != 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to protect service-account credential handoff permissions");
    }
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    let sealed = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) };
    if sealed != 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to seal service-account credential handoff");
    }
    Ok(file)
}

pub(crate) struct ExecCredentialHandoff {
    #[cfg(target_os = "linux")]
    _service_account_token: Option<File>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct CapturedStartEnvironment {
    #[serde(default)]
    values: BTreeMap<String, String>,
}

impl fmt::Debug for CapturedStartEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedStartEnvironment")
            .field("keys", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl CapturedStartEnvironment {
    pub fn capture() -> Self {
        let values = CAPTURED_START_ENV_NAMES
            .iter()
            .filter_map(|name| {
                let value = if *name == "OP_SERVICE_ACCOUNT_TOKEN" {
                    INHERITED_SERVICE_ACCOUNT_TOKEN
                        .get()
                        .cloned()
                        .or_else(|| std::env::var(name).ok())
                } else {
                    std::env::var(name).ok()
                };
                value
                    .filter(|value| !value.is_empty())
                    .map(|value| ((*name).to_owned(), value))
            })
            .collect();
        Self { values }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let mut total = 0usize;
        for (name, value) in &self.values {
            anyhow::ensure!(
                CAPTURED_START_ENV_NAMES.contains(&name.as_str()),
                "unsupported captured start environment variable: {name}"
            );
            anyhow::ensure!(
                value.len() <= MAX_CAPTURED_START_ENV_VALUE_BYTES,
                "captured start environment variable {name} exceeds {MAX_CAPTURED_START_ENV_VALUE_BYTES} bytes"
            );
            total = total
                .checked_add(name.len())
                .and_then(|total| total.checked_add(value.len()))
                .context("captured start environment size overflow")?;
        }
        anyhow::ensure!(
            total <= MAX_CAPTURED_START_ENV_TOTAL_BYTES,
            "captured start environment exceeds {MAX_CAPTURED_START_ENV_TOTAL_BYTES} bytes"
        );
        Ok(())
    }

    pub(crate) fn values(&self) -> &BTreeMap<String, String> {
        &self.values
    }

    pub(crate) fn restart_context_keys(&self) -> Vec<String> {
        self.values.keys().cloned().collect()
    }

    pub(crate) fn restart_context_mismatches(&self, available: &Self) -> Vec<String> {
        self.values
            .iter()
            .filter(|(name, value)| available.values.get(*name) != Some(*value))
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub(crate) fn select_restart_context(&self, keys: &[String]) -> Result<Self> {
        let values = keys
            .iter()
            .map(|name| {
                let value = self
                    .values
                    .get(name)
                    .with_context(|| format!("restart context is missing {name}"))?;
                Ok((name.clone(), value.clone()))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let selected = Self { values };
        selected.validate()?;
        Ok(selected)
    }

    pub(crate) fn apply_to_command(
        &self,
        command: &mut std::process::Command,
    ) -> Result<ExecCredentialHandoff> {
        for name in CAPTURED_START_ENV_NAMES {
            command.env_remove(name);
        }
        command.env_remove(SERVICE_ACCOUNT_TOKEN_FD_ENV);

        #[cfg(target_os = "linux")]
        let mut service_account_token = None;
        for (name, value) in &self.values {
            if name == "OP_SERVICE_ACCOUNT_TOKEN" {
                #[cfg(target_os = "linux")]
                {
                    let handoff = create_service_account_token_handoff(value)?;
                    command.env(
                        SERVICE_ACCOUNT_TOKEN_FD_ENV,
                        handoff.as_raw_fd().to_string(),
                    );
                    service_account_token = Some(handoff);
                }
                #[cfg(not(target_os = "linux"))]
                command.env(name, value);
            } else {
                command.env(name, value);
            }
        }

        Ok(ExecCredentialHandoff {
            #[cfg(target_os = "linux")]
            _service_account_token: service_account_token,
        })
    }

    #[cfg(test)]
    pub(crate) fn from_values(values: BTreeMap<String, String>) -> Result<Self> {
        let environment = Self { values };
        environment.validate()?;
        Ok(environment)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: Uuid,
    pub operation: String,
    pub detail: String,
    pub cwd: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Probe,
    Approval {
        request: Request,
    },
    Activity {
        title: String,
        detail: Option<String>,
    },
    OnePasswordServiceAccount {
        request: ServiceAccountRequest,
    },
    KintoneMcp {
        request: KintoneMcpRequest,
    },
    KintoneCli {
        request: KintoneCliRequest,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum ServiceAccountRequest {
    Status,
    Run {
        cwd: PathBuf,
        command: Vec<String>,
        env_files: Vec<PathBuf>,
        environment: BTreeMap<String, String>,
        #[serde(default)]
        allowed_locators: Vec<String>,
    },
}

#[derive(Serialize, Deserialize)]
struct ServiceAccountResponse {
    result: Option<Value>,
    error: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum KintoneMcpRequest {
    Status,
    Discover,
    Call { tool_name: String, arguments: Value },
}

#[derive(Serialize, Deserialize)]
struct KintoneMcpResponse {
    result: Option<Value>,
    error: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum KintoneCliRequest {
    Status,
    Run {
        cwd: PathBuf,
        arguments: Vec<String>,
        stdout_path: Option<PathBuf>,
    },
}

#[derive(Serialize, Deserialize)]
struct KintoneCliResponse {
    result: Option<Value>,
    error: Option<String>,
}

fn encode_json_line_with_limit<T: Serialize + ?Sized>(
    value: &T,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).context("failed to serialize session message")?;
    let wire_bytes = bytes
        .len()
        .checked_add(1)
        .context("session message size overflow")?;
    anyhow::ensure!(
        wire_bytes <= max_bytes,
        "session message exceeds {max_bytes} bytes"
    );
    bytes.push(b'\n');
    Ok(bytes)
}

fn encode_session_json_line<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    encode_json_line_with_limit(value, MAX_SESSION_MESSAGE_BYTES)
}

fn encode_session_result(result: Result<Value>, label: &str) -> Vec<u8> {
    let response = match result {
        Ok(result) => json!({"result": result, "error": Value::Null}),
        Err(error) => json!({"result": Value::Null, "error": format!("{error:#}")}),
    };
    match encode_session_json_line(&response) {
        Ok(bytes) => bytes,
        Err(_) => encode_session_json_line(&json!({
            "result": Value::Null,
            "error": format!("{label} exceeds {MAX_SESSION_MESSAGE_BYTES} bytes")
        }))
        .expect("bounded session error response must fit"),
    }
}

pub(crate) fn ensure_approval_detail_fits(detail: &str) -> Result<()> {
    anyhow::ensure!(
        detail.len() <= MAX_APPROVAL_DETAIL_BYTES,
        "approval detail exceeds {MAX_APPROVAL_DETAIL_BYTES} bytes; exact nested resolver locator scope cannot be displayed safely"
    );
    Ok(())
}

pub async fn request(
    session_id: &str,
    operation: &str,
    detail: String,
    cwd: PathBuf,
) -> Result<bool> {
    let request = Request {
        id: Uuid::new_v4(),
        operation: operation.to_owned(),
        detail,
        cwd,
    };
    let message = encode_session_json_line(&Message::Approval { request })?;
    let path = config::socket_path(session_id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `temote-mcp start`"))?;
    stream.write_all(&message).await?;
    stream.shutdown().await?;

    let response =
        read_session_response(stream, MAX_APPROVAL_RESPONSE_BYTES, "approval response").await?;
    match response.trim() {
        "allow" => Ok(true),
        "deny" => Ok(false),
        value => anyhow::bail!("invalid response from session: {value:?}"),
    }
}

async fn read_session_response(
    stream: UnixStream,
    max_bytes: usize,
    label: &str,
) -> Result<String> {
    let mut response = String::new();
    let read = tokio::time::timeout(SESSION_RESPONSE_TIMEOUT, async {
        BufReader::new(stream)
            .take((max_bytes + 1) as u64)
            .read_line(&mut response)
            .await
    })
    .await
    .with_context(|| format!("timed out waiting for {label}"))??;
    anyhow::ensure!(read > 0, "{label} closed without a response");
    anyhow::ensure!(read <= max_bytes, "{label} exceeds {max_bytes} bytes");
    Ok(response)
}

pub async fn onepassword_service_account_status(session_id: &str) -> Result<Value> {
    service_account_request(session_id, ServiceAccountRequest::Status).await
}

pub async fn onepassword_service_account_run(
    session_id: &str,
    cwd: PathBuf,
    command: Vec<String>,
    env_files: Vec<PathBuf>,
    environment: BTreeMap<String, String>,
    allowed_locators: Vec<String>,
) -> Result<Value> {
    service_account_request(
        session_id,
        ServiceAccountRequest::Run {
            cwd,
            command,
            env_files,
            environment,
            allowed_locators,
        },
    )
    .await
}

pub async fn kintone_mcp_status(session_id: &str) -> Result<Value> {
    kintone_mcp_request(session_id, KintoneMcpRequest::Status).await
}

pub async fn kintone_mcp_discover(session_id: &str) -> Result<Value> {
    kintone_mcp_request(session_id, KintoneMcpRequest::Discover).await
}

pub async fn kintone_mcp_call(
    session_id: &str,
    tool_name: &str,
    arguments: Value,
) -> Result<Value> {
    kintone_mcp_request(
        session_id,
        KintoneMcpRequest::Call {
            tool_name: tool_name.to_owned(),
            arguments,
        },
    )
    .await
}

pub async fn kintone_cli_status(session_id: &str) -> Result<Value> {
    kintone_cli_request(session_id, KintoneCliRequest::Status).await
}

pub async fn kintone_cli_run(
    session_id: &str,
    cwd: PathBuf,
    arguments: Vec<String>,
    stdout_path: Option<PathBuf>,
) -> Result<Value> {
    kintone_cli_request(
        session_id,
        KintoneCliRequest::Run {
            cwd,
            arguments,
            stdout_path,
        },
    )
    .await
}

async fn kintone_cli_request(session_id: &str, request: KintoneCliRequest) -> Result<Value> {
    let message = encode_session_json_line(&Message::KintoneCli { request })?;
    let path = config::socket_path(session_id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `temote-mcp start`"))?;
    stream.write_all(&message).await?;
    stream.shutdown().await?;

    let response =
        read_session_response(stream, MAX_SESSION_MESSAGE_BYTES, "cli-kintone response").await?;
    let response: KintoneCliResponse =
        serde_json::from_str(response.trim()).context("invalid cli-kintone response")?;
    if let Some(error) = response.error {
        anyhow::bail!(error);
    }
    response
        .result
        .context("cli-kintone response is missing result")
}

async fn kintone_mcp_request(session_id: &str, request: KintoneMcpRequest) -> Result<Value> {
    let message = encode_session_json_line(&Message::KintoneMcp { request })?;
    let path = config::socket_path(session_id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `temote-mcp start`"))?;
    stream.write_all(&message).await?;
    stream.shutdown().await?;

    let response =
        read_session_response(stream, MAX_SESSION_MESSAGE_BYTES, "kintone MCP response").await?;
    let response: KintoneMcpResponse =
        serde_json::from_str(response.trim()).context("invalid kintone MCP response")?;
    if let Some(error) = response.error {
        anyhow::bail!(error);
    }
    response
        .result
        .context("kintone MCP response is missing result")
}

async fn service_account_request(
    session_id: &str,
    request: ServiceAccountRequest,
) -> Result<Value> {
    let message = encode_session_json_line(&Message::OnePasswordServiceAccount { request })?;
    let path = config::socket_path(session_id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `temote-mcp start`"))?;
    stream.write_all(&message).await?;
    stream.shutdown().await?;

    let response = read_session_response(
        stream,
        MAX_SESSION_MESSAGE_BYTES,
        "1Password service-account response",
    )
    .await?;
    let response: ServiceAccountResponse = serde_json::from_str(response.trim())
        .context("invalid 1Password service-account response")?;
    if let Some(error) = response.error {
        anyhow::bail!(error);
    }
    response
        .result
        .context("1Password service-account response is missing result")
}

/// Sends a one-way activity update to the `start` screen. Activity reporting is
/// deliberately best-effort: an MCP operation must not fail just because its UI
/// was closed between loading the session and completing the operation.
pub async fn activity(session_id: &str, title: impl Into<String>, detail: Option<String>) {
    let Ok(path) = config::socket_path(session_id) else {
        return;
    };
    let Ok(mut stream) = UnixStream::connect(path).await else {
        return;
    };
    let message = Message::Activity {
        title: title.into(),
        detail,
    };
    let Ok(bytes) = encode_session_json_line(&message) else {
        return;
    };
    let _ = stream.write_all(&bytes).await;
    let _ = stream.shutdown().await;
}

pub struct ApprovalPrompt {
    pub session_id: String,
    pub request: Request,
    response: oneshot::Sender<bool>,
}

impl ApprovalPrompt {
    pub fn respond(self, allowed: bool) {
        let _ = self.response.send(allowed);
    }
}

pub type ApprovalReceiver = mpsc::Receiver<ApprovalPrompt>;
pub type ApprovalSender = mpsc::Sender<ApprovalPrompt>;

fn approval_channel_with_capacity(capacity: usize) -> (ApprovalSender, ApprovalReceiver) {
    mpsc::channel(capacity)
}

pub fn approval_channel() -> (ApprovalSender, ApprovalReceiver) {
    approval_channel_with_capacity(MAX_PENDING_APPROVAL_PROMPTS)
}

pub async fn request_approval(
    sender: &ApprovalSender,
    session_id: impl Into<String>,
    request: Request,
) -> Result<bool> {
    let (response, receiver) = oneshot::channel();
    let prompt = ApprovalPrompt {
        session_id: session_id.into(),
        request,
        response,
    };
    sender.try_send(prompt).map_err(|error| {
        error.into_inner().respond(false);
        anyhow::anyhow!("local approval console is unavailable or busy")
    })?;
    Ok(receiver.await.unwrap_or(false))
}

pub async fn request_supervisor_approval(
    sender: &ApprovalSender,
    operation: &str,
    detail: String,
) -> Result<bool> {
    let request = Request {
        id: Uuid::new_v4(),
        operation: operation.to_owned(),
        detail,
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    request_approval(sender, "oauth", request).await
}

#[cfg_attr(not(test), allow(dead_code))]
enum RuntimeCommand {
    SetYolo {
        value: bool,
        response: oneshot::Sender<Result<()>>,
    },
    AllowDirectory {
        path: PathBuf,
        response: oneshot::Sender<Result<()>>,
    },
    RevokeDirectory {
        path: PathBuf,
        response: oneshot::Sender<Result<()>>,
    },
    Snapshot {
        response: oneshot::Sender<Session>,
    },
    SetUpgradeQuiesced {
        value: bool,
        response: oneshot::Sender<Result<()>>,
    },
    #[cfg(test)]
    CrashForTest,
    Shutdown,
}

pub struct RuntimeHandle {
    id: String,
    cwd: PathBuf,
    commands: mpsc::Sender<RuntimeCommand>,
    join: JoinHandle<Result<()>>,
}

impl RuntimeHandle {
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn set_yolo(&self, value: bool) -> Result<()> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(RuntimeCommand::SetYolo { value, response })
            .await
            .map_err(|_| anyhow::anyhow!("session {} runtime stopped", self.id))?;
        receiver
            .await
            .context("session runtime stopped before updating permission mode")??;
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn allow_directory(&self, path: PathBuf) -> Result<()> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(RuntimeCommand::AllowDirectory { path, response })
            .await
            .map_err(|_| anyhow::anyhow!("session {} runtime stopped", self.id))?;
        receiver
            .await
            .context("session runtime stopped before adding sandbox root")??;
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn revoke_directory(&self, path: PathBuf) -> Result<()> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(RuntimeCommand::RevokeDirectory { path, response })
            .await
            .map_err(|_| anyhow::anyhow!("session {} runtime stopped", self.id))?;
        receiver
            .await
            .context("session runtime stopped before revoking sandbox root")??;
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn snapshot(&self) -> Result<Session> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(RuntimeCommand::Snapshot { response })
            .await
            .map_err(|_| anyhow::anyhow!("session {} runtime stopped", self.id))?;
        receiver
            .await
            .context("session runtime stopped before snapshot")
    }

    pub(crate) async fn set_upgrade_quiesced(&self, value: bool) -> Result<()> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(RuntimeCommand::SetUpgradeQuiesced { value, response })
            .await
            .map_err(|_| anyhow::anyhow!("session {} runtime stopped", self.id))?;
        receiver
            .await
            .context("session runtime stopped before changing upgrade quiesce state")??;
        Ok(())
    }

    pub async fn shutdown(self) -> Result<()> {
        if config::read_session_lifecycle(&self.id)
            .await
            .ok()
            .flatten()
            .is_some_and(|state| state.status == config::LifecycleStatus::Crashed)
        {
            let session_id = self.id.clone();
            if let Err(error) = self.wait().await {
                eprintln!("session {session_id} was already crashed before shutdown: {error:#}");
            }
            return Ok(());
        }
        if self.join.is_finished() {
            return self.wait().await;
        }
        if let Ok(Some(mut lifecycle)) = config::read_session_lifecycle(&self.id).await {
            lifecycle.status = config::LifecycleStatus::Stopping;
            lifecycle.exit_reason = Some("graceful shutdown requested".to_owned());
            lifecycle.last_error = None;
            if let Err(error) = config::save_session_lifecycle(&self.id, &lifecycle).await {
                eprintln!("failed to mark session {} stopping: {error:#}", self.id);
            }
        }
        let _ = self.commands.send(RuntimeCommand::Shutdown).await;
        self.join
            .await
            .context("session runtime task failed to join")??;
        Ok(())
    }

    #[cfg(test)]
    pub async fn crash_for_test(&self) -> Result<()> {
        self.commands
            .send(RuntimeCommand::CrashForTest)
            .await
            .map_err(|_| anyhow::anyhow!("session {} runtime stopped", self.id))
    }

    pub async fn wait(self) -> Result<()> {
        self.join
            .await
            .context("session runtime task failed to join")??;
        Ok(())
    }
}

#[cfg(test)]
pub async fn spawn_runtime(
    cwd: &Path,
    session_id: Option<&str>,
    yolo: bool,
    approval_sender: ApprovalSender,
) -> Result<RuntimeHandle> {
    spawn_runtime_with_logical_path_and_environment(
        cwd,
        session_id,
        yolo,
        approval_sender,
        None,
        CapturedStartEnvironment::capture(),
    )
    .await
}

pub async fn spawn_runtime_with_logical_path_and_environment(
    cwd: &Path,
    session_id: Option<&str>,
    yolo: bool,
    approval_sender: ApprovalSender,
    logical_path: Option<String>,
    environment: CapturedStartEnvironment,
) -> Result<RuntimeHandle> {
    environment.validate()?;
    let service_account_token = environment
        .values()
        .get("OP_SERVICE_ACCOUNT_TOKEN")
        .filter(|value| !value.trim().is_empty())
        .cloned();
    let kintone_bridge = Arc::new(tokio::sync::Mutex::new(kintone_mcp::Bridge::capture_from(
        environment.values(),
    )));
    let kintone_cli_bridge = Arc::new(kintone_cli::Bridge::capture_from(environment.values()));
    let id = config::session_id(session_id)?;
    let previous_session = config::read_session_metadata(&id).await.ok();
    config::remove_inactive_socket(&id).await?;
    let mut session = config::new_session(cwd, Some(&id), yolo)?;
    if let Some(previous) = previous_session
        && previous.cwd == session.cwd
    {
        session.permitted_directories = previous.permitted_directories;
        if !session.permitted_directories.contains(&session.cwd) {
            session.permitted_directories.push(session.cwd.clone());
            session.permitted_directories.sort();
        }
    }
    let path = config::socket_path(&session.id)?;
    let state_dir = path.parent().context("session socket has no parent")?;
    tokio::fs::create_dir_all(state_dir).await?;
    tokio::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).await?;

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to listen at {}", path.display()))?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    session.process_id = std::process::id();
    let previous_lifecycle = config::read_session_lifecycle(&session.id)
        .await
        .ok()
        .flatten();
    let mut lifecycle = config::SessionLifecycle::starting(session.started_at, logical_path);
    if let Some(previous) = previous_lifecycle {
        lifecycle.restart_policy = previous.restart_policy;
        lifecycle.restart_count = previous.restart_count;
        lifecycle.last_restart_at = previous.last_restart_at;
        lifecycle.next_restart_at = None;
        lifecycle.restart_limit_reason = previous.restart_limit_reason;
    }
    if let Err(error) = config::save_session_lifecycle(&session.id, &lifecycle).await {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    if let Err(error) = config::save_session(&session).await {
        lifecycle.status = config::LifecycleStatus::Crashed;
        lifecycle.stopped_at = Some(config::unix_time());
        lifecycle.exit_reason = Some("failed to persist runtime metadata".to_owned());
        lifecycle.last_error = Some(format!("{error:#}"));
        let _ = config::save_session_lifecycle(&session.id, &lifecycle).await;
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    lifecycle.status = config::LifecycleStatus::Active;
    if let Err(error) = config::save_session_lifecycle(&session.id, &lifecycle).await {
        session.process_id = 0;
        if let Err(save_error) = config::save_session(&session).await {
            eprintln!(
                "failed to roll back session metadata for {} after lifecycle save failure: {save_error:#}",
                session.id
            );
        }
        lifecycle.status = config::LifecycleStatus::Crashed;
        lifecycle.stopped_at = Some(config::unix_time());
        lifecycle.exit_reason = Some("failed to persist active lifecycle state".to_owned());
        lifecycle.last_error = Some(format!("{error:#}"));
        let _ = config::save_session_lifecycle(&session.id, &lifecycle).await;
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }

    let id_for_handle = session.id.clone();
    let cwd_for_handle = session.cwd.clone();
    let fallback_session = session.clone();
    let final_path = path.clone();
    let (commands, command_receiver) = mpsc::channel(MAX_PENDING_RUNTIME_COMMANDS);
    let runtime_join = tokio::spawn(async move {
        let result = run_runtime(
            listener,
            &mut session,
            command_receiver,
            approval_sender,
            service_account_token.as_deref(),
            kintone_bridge,
            kintone_cli_bridge,
        )
        .await;
        (session, result)
    });
    let join = tokio::spawn(async move {
        let (mut session, result) = match runtime_join.await {
            Ok((session, result)) => (session, result),
            Err(error) => (
                fallback_session,
                Err(anyhow::anyhow!("session runtime task failed: {error}")),
            ),
        };
        session.process_id = 0;
        if let Err(error) = config::save_session(&session).await {
            eprintln!(
                "failed to persist final session metadata for {}: {error:#}",
                session.id
            );
        }

        let mut lifecycle = config::read_session_lifecycle(&session.id)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| config::SessionLifecycle::starting(session.started_at, None));
        lifecycle.stopped_at = Some(config::unix_time());
        match &result {
            Ok(()) => {
                lifecycle.status = config::LifecycleStatus::Stopped;
                lifecycle.exit_reason = Some("graceful shutdown".to_owned());
                lifecycle.last_error = None;
            }
            Err(error) => {
                lifecycle.status = config::LifecycleStatus::Crashed;
                lifecycle.exit_reason = Some("unexpected runtime termination".to_owned());
                lifecycle.last_error = Some(format!("{error:#}"));
            }
        }
        if let Err(error) = config::save_session_lifecycle(&session.id, &lifecycle).await {
            eprintln!("failed to persist lifecycle for {}: {error:#}", session.id);
        }
        if let Err(error) = tokio::fs::remove_file(&final_path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "failed to clean up session socket {}: {error}",
                final_path.display()
            );
        }
        result
    });

    Ok(RuntimeHandle {
        id: id_for_handle,
        cwd: cwd_for_handle,
        commands,
        join,
    })
}

async fn read_session_message(stream: &mut UnixStream) -> Result<Option<String>> {
    let mut line = String::new();
    let read = BufReader::new(stream)
        .take((MAX_SESSION_MESSAGE_BYTES + 1) as u64)
        .read_line(&mut line)
        .await
        .context("failed to read session message")?;
    if read == 0 {
        return Ok(None);
    }
    anyhow::ensure!(
        read <= MAX_SESSION_MESSAGE_BYTES,
        "session message exceeds {MAX_SESSION_MESSAGE_BYTES} bytes"
    );
    Ok(Some(line))
}

struct IncomingSessionMessage {
    stream: UnixStream,
    message: Message,
    _permit: OwnedSemaphorePermit,
}

fn queue_incoming_session_message(
    sender: &mpsc::Sender<IncomingSessionMessage>,
    stream: UnixStream,
    message: Message,
    permit: OwnedSemaphorePermit,
) -> bool {
    sender
        .try_send(IncomingSessionMessage {
            stream,
            message,
            _permit: permit,
        })
        .is_ok()
}

async fn receive_session_message(
    mut stream: UnixStream,
    session_id: String,
    sender: mpsc::Sender<IncomingSessionMessage>,
    permit: OwnedSemaphorePermit,
) {
    let line = match tokio::time::timeout(
        SESSION_MESSAGE_READ_TIMEOUT,
        read_session_message(&mut stream),
    )
    .await
    {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => return,
        Ok(Err(error)) => {
            eprintln!("[session {session_id}] rejected session message: {error:#}");
            return;
        }
        Err(_) => {
            eprintln!("[session {session_id}] timed out waiting for a session message");
            return;
        }
    };
    let message: Message = match serde_json::from_str(&line) {
        Ok(message) => message,
        Err(error) => {
            eprintln!("[session {session_id}] ignoring invalid session message: {error}");
            return;
        }
    };
    let _ = queue_incoming_session_message(&sender, stream, message, permit);
}

struct ActiveOperationGuard {
    count: Arc<AtomicUsize>,
}

impl ActiveOperationGuard {
    fn new(count: Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        Self { count }
    }
}

impl Drop for ActiveOperationGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn run_runtime(
    listener: UnixListener,
    session: &mut Session,
    mut commands: mpsc::Receiver<RuntimeCommand>,
    approval_sender: ApprovalSender,
    service_account_token: Option<&str>,
    kintone_bridge: Arc<tokio::sync::Mutex<kintone_mcp::Bridge>>,
    kintone_cli_bridge: Arc<kintone_cli::Bridge>,
) -> Result<()> {
    let (approval_lifetime, _) = watch::channel(false);
    let (incoming_sender, mut incoming_receiver) = mpsc::channel(MAX_PENDING_SESSION_READS);
    let read_slots = Arc::new(Semaphore::new(MAX_PENDING_SESSION_READS));
    let approval_slots = Arc::new(Semaphore::new(MAX_PENDING_APPROVALS));
    let active_operations = Arc::new(AtomicUsize::new(0));
    let mut upgrade_quiesced = false;
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let (stream, _) = connection?;
                let permit = match Arc::clone(&read_slots).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        eprintln!("[session {}] rejecting session connection: too many pending reads", session.id);
                        continue;
                    }
                };
                let sender = incoming_sender.clone();
                let session_id = session.id.clone();
                tokio::spawn(async move {
                    receive_session_message(stream, session_id, sender, permit).await;
                });
            }
            incoming = incoming_receiver.recv() => {
                let Some(IncomingSessionMessage { mut stream, message, _permit }) = incoming else { continue };
                drop(_permit);
                if upgrade_quiesced && !matches!(&message, Message::Probe | Message::Activity { .. }) {
                    match &message {
                        Message::Approval { .. } => {
                            let _ = stream.write_all(b"deny\n").await;
                        }
                        Message::OnePasswordServiceAccount { .. } => {
                            let bytes = encode_session_result(
                                Err(anyhow::anyhow!("session is quiesced for supervisor upgrade")),
                                "1Password service-account response",
                            );
                            let _ = stream.write_all(&bytes).await;
                        }
                        Message::KintoneMcp { .. } => {
                            let bytes = encode_session_result(
                                Err(anyhow::anyhow!("session is quiesced for supervisor upgrade")),
                                "kintone MCP response",
                            );
                            let _ = stream.write_all(&bytes).await;
                        }
                        Message::KintoneCli { .. } => {
                            let bytes = encode_session_result(
                                Err(anyhow::anyhow!("session is quiesced for supervisor upgrade")),
                                "cli-kintone response",
                            );
                            let _ = stream.write_all(&bytes).await;
                        }
                        Message::Probe | Message::Activity { .. } => unreachable!(),
                    }
                    let _ = stream.shutdown().await;
                    continue;
                }
                match message {
                    Message::Probe => {
                        if let Err(error) = stream.write_all(b"active\n").await {
                            eprintln!("[session {}] probe client disconnected before response: {error}", session.id);
                        }
                    }
                    Message::Activity { title, detail } => {
                        show_activity_for_session(&session.id, &title, detail.as_deref());
                    }
                    Message::OnePasswordServiceAccount { request } => {
                        let session = session.clone();
                        let token = service_account_token.map(str::to_owned);
                        let operation = ActiveOperationGuard::new(Arc::clone(&active_operations));
                        tokio::spawn(async move {
                            let _operation = operation;
                            let response = handle_service_account_request(&session, token.as_deref(), request).await;
                            let bytes = encode_session_result(response, "1Password service-account response");
                            let _ = stream.write_all(&bytes).await;
                            let _ = stream.shutdown().await;
                        });
                    }
                    Message::KintoneMcp { request } => {
                        let session = session.clone();
                        let bridge = Arc::clone(&kintone_bridge);
                        let operation = ActiveOperationGuard::new(Arc::clone(&active_operations));
                        tokio::spawn(async move {
                            let _operation = operation;
                            let response = handle_kintone_mcp_request(&session, bridge, request).await;
                            let bytes = encode_session_result(response, "kintone MCP response");
                            let _ = stream.write_all(&bytes).await;
                            let _ = stream.shutdown().await;
                        });
                    }
                    Message::KintoneCli { request } => {
                        let session = session.clone();
                        let bridge = Arc::clone(&kintone_cli_bridge);
                        let operation = ActiveOperationGuard::new(Arc::clone(&active_operations));
                        tokio::spawn(async move {
                            let _operation = operation;
                            let response = handle_kintone_cli_request(&session, bridge, request).await;
                            let bytes = encode_session_result(response, "cli-kintone response");
                            let _ = stream.write_all(&bytes).await;
                            let _ = stream.shutdown().await;
                        });
                    }
                    Message::Approval { request } if session.yolo => {
                        eprintln!(
                            "[session {}] [yolo] allowing {}: {}",
                            session.id,
                            bounded_console_text(&request.operation, MAX_APPROVAL_OPERATION_BYTES),
                            bounded_console_text(&request.detail, MAX_APPROVAL_DETAIL_BYTES),
                        );
                        if let Err(error) = stream.write_all(b"allow\n").await {
                            eprintln!("[session {}] approval client disconnected before response: {error}", session.id);
                        }
                    }
                    Message::Approval { request } => {
                        let permit = match Arc::clone(&approval_slots).try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                tokio::spawn(async move {
                                    let _ = stream.write_all(b"deny\n").await;
                                    let _ = stream.shutdown().await;
                                });
                                continue;
                            }
                        };
                        let (response, receiver) = oneshot::channel();
                        let prompt = ApprovalPrompt {
                            session_id: session.id.clone(),
                            request,
                            response,
                        };
                        if let Err(error) = approval_sender.try_send(prompt) {
                            error.into_inner().respond(false);
                            drop(permit);
                            let _ = stream.write_all(b"deny\n").await;
                            let _ = stream.shutdown().await;
                            continue;
                        }
                        let mut runtime_alive = approval_lifetime.subscribe();
                        let operation = ActiveOperationGuard::new(Arc::clone(&active_operations));
                        tokio::spawn(async move {
                            let _operation = operation;
                            let _permit = permit;
                            let allowed = tokio::select! {
                                response = receiver => response.unwrap_or(false),
                                _ = runtime_alive.changed() => false,
                            };
                            let _ = stream
                                .write_all(if allowed { b"allow\n" } else { b"deny\n" })
                                .await;
                            let _ = stream.shutdown().await;
                        });
                    }
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    anyhow::bail!("runtime command channel closed unexpectedly");
                };
                match command {
                    RuntimeCommand::SetYolo { value, response } => {
                        session.yolo = value;
                        let result = config::save_session(session).await;
                        let _ = response.send(result);
                    }
                    RuntimeCommand::AllowDirectory { path, response } => {
                        let result = (|| -> Result<()> {
                            let directory = config::canonical_directory(&path)?;
                            if !session.permitted_directories.contains(&directory) {
                                session.permitted_directories.push(directory);
                                session.permitted_directories.sort();
                            }
                            Ok(())
                        })();
                        let result = match result {
                            Ok(()) => config::save_session(session).await,
                            Err(error) => Err(error),
                        };
                        let _ = response.send(result);
                    }
                    RuntimeCommand::RevokeDirectory { path, response } => {
                        let result = (|| -> Result<()> {
                            let directory = config::canonical_directory(&path)?;
                            anyhow::ensure!(directory != session.cwd, "cannot revoke the session cwd");
                            session.permitted_directories.retain(|item| item != &directory);
                            Ok(())
                        })();
                        let result = match result {
                            Ok(()) => config::save_session(session).await,
                            Err(error) => Err(error),
                        };
                        let _ = response.send(result);
                    }
                    RuntimeCommand::Snapshot { response } => {
                        let _ = response.send(session.clone());
                    }
                    RuntimeCommand::SetUpgradeQuiesced { value, response } => {
                        let result = if value && active_operations.load(Ordering::Acquire) != 0 {
                            Err(anyhow::anyhow!(
                                "session {} has {} in-flight operation(s)",
                                session.id,
                                active_operations.load(Ordering::Acquire)
                            ))
                        } else {
                            upgrade_quiesced = value;
                            Ok(())
                        };
                        let _ = response.send(result);
                    }
                    #[cfg(test)]
                    RuntimeCommand::CrashForTest => {
                        anyhow::bail!("injected runtime crash for test");
                    }
                    RuntimeCommand::Shutdown => {
                        let _ = approval_lifetime.send(true);
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn handle_kintone_cli_request(
    session: &Session,
    bridge: Arc<kintone_cli::Bridge>,
    request: KintoneCliRequest,
) -> Result<Value> {
    match request {
        KintoneCliRequest::Status => Ok(bridge.status()),
        KintoneCliRequest::Run {
            cwd,
            arguments,
            stdout_path,
        } => bridge.run(session, &cwd, arguments, stdout_path).await,
    }
}

async fn handle_kintone_mcp_request(
    session: &Session,
    bridge: Arc<tokio::sync::Mutex<kintone_mcp::Bridge>>,
    request: KintoneMcpRequest,
) -> Result<Value> {
    let mut bridge = bridge.lock().await;
    match request {
        KintoneMcpRequest::Status => Ok(bridge.status(session)),
        KintoneMcpRequest::Discover => bridge.discover(session).await,
        KintoneMcpRequest::Call {
            tool_name,
            arguments,
        } => bridge.call_tool(session, &tool_name, arguments).await,
    }
}

async fn handle_service_account_request(
    session: &Session,
    token: Option<&str>,
    request: ServiceAccountRequest,
) -> Result<Value> {
    let Some(token) = token else {
        return match request {
            ServiceAccountRequest::Status => {
                Ok(json!({"configured": false, "authenticated": false}))
            }
            ServiceAccountRequest::Run { .. } => {
                anyhow::bail!(
                    "1Password service account is not configured; start the session with OP_SERVICE_ACCOUNT_TOKEN set"
                )
            }
        };
    };

    match request {
        ServiceAccountRequest::Status => service_account_status(session, token).await,
        ServiceAccountRequest::Run {
            cwd,
            command,
            env_files,
            environment,
            allowed_locators,
        } => {
            service_account_run(
                session,
                token,
                cwd,
                command,
                env_files,
                environment,
                allowed_locators,
            )
            .await
        }
    }
}

async fn service_account_status(session: &Session, token: &str) -> Result<Value> {
    let op_executable = service_account_cli_executable()?;
    service_account_status_with_op(session, token, &op_executable.to_string_lossy()).await
}

fn service_account_cli_executable() -> Result<PathBuf> {
    let path = resolve_executable_from_path("op")?;
    validate_service_account_cli_process_boundary(&path)?;
    Ok(path)
}

fn resolve_executable_from_path(name: &str) -> Result<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return std::fs::canonicalize(path)
            .with_context(|| format!("failed to resolve executable {name}"));
    }
    let search_path = std::env::var_os("PATH").context("PATH is not configured")?;
    for directory in std::env::split_paths(&search_path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return std::fs::canonicalize(&candidate)
                .with_context(|| format!("failed to resolve executable {}", candidate.display()));
        }
    }
    anyhow::bail!("1Password CLI executable was not found on PATH")
}

#[cfg(target_os = "linux")]
fn validate_service_account_cli_process_boundary(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed to inspect 1Password CLI at {}", path.display()))?;
    anyhow::ensure!(metadata.is_file(), "1Password CLI must be a regular file");
    validate_service_account_cli_metadata(
        metadata.uid(),
        metadata.gid(),
        metadata.mode(),
        unsafe { libc::geteuid() },
        unsafe { libc::getegid() },
        &supplementary_groups()?,
        linux_suid_dumpable()?,
    )
}

#[cfg(not(target_os = "linux"))]
fn validate_service_account_cli_process_boundary(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_service_account_cli_metadata(
    owner_uid: u32,
    owner_gid: u32,
    mode: u32,
    effective_uid: u32,
    effective_gid: u32,
    supplementary_groups: &[u32],
    suid_dumpable: u32,
) -> Result<()> {
    anyhow::ensure!(
        effective_uid != 0,
        "service-account execution as root is not supported"
    );
    anyhow::ensure!(owner_uid == 0, "1Password CLI must be owned by root");
    anyhow::ensure!(
        mode & 0o2000 != 0,
        "1Password CLI must have the setgid bit enabled"
    );
    anyhow::ensure!(
        mode & 0o001 != 0,
        "1Password CLI must be executable by the Temote user"
    );
    anyhow::ensure!(
        mode & 0o022 == 0,
        "1Password CLI must not be writable by its group or other users"
    );
    anyhow::ensure!(
        owner_gid != effective_gid && !supplementary_groups.contains(&owner_gid),
        "1Password CLI setgid group must not be available to the Temote user"
    );
    anyhow::ensure!(
        matches!(suid_dumpable, 0 | 2),
        "Linux fs.suid_dumpable must be 0 or 2 for service-account execution"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn supplementary_groups() -> Result<Vec<u32>> {
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count < 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to inspect supplementary groups");
    }
    let mut groups = vec![0 as libc::gid_t; count as usize];
    if count > 0 {
        let read = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        if read < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to inspect supplementary groups");
        }
        groups.truncate(read as usize);
    }
    Ok(groups.into_iter().collect())
}

#[cfg(target_os = "linux")]
fn linux_suid_dumpable() -> Result<u32> {
    let value = std::fs::read_to_string("/proc/sys/fs/suid_dumpable")
        .context("failed to read Linux fs.suid_dumpable policy")?;
    value
        .trim()
        .parse::<u32>()
        .context("Linux fs.suid_dumpable policy is invalid")
}

async fn service_account_status_with_op(
    session: &Session,
    token: &str,
    op_executable: &str,
) -> Result<Value> {
    let command = vec![
        op_executable.to_owned(),
        "whoami".to_owned(),
        "--format=json".to_owned(),
    ];
    let mut environment = HashMap::new();
    environment.insert("OP_SERVICE_ACCOUNT_TOKEN".to_owned(), token.to_owned());
    let output = sandbox::run_unrestricted_with_env(
        &command,
        &session.cwd,
        None,
        &environment,
        &["OP_SERVICE_ACCOUNT_TOKEN"],
    )
    .await
    .context("failed to check 1Password service-account authentication")?;
    let authenticated = output.status == 0;
    let account = if authenticated {
        serde_json::from_str::<Value>(&output.stdout)
            .ok()
            .map(|value| {
                json!({
                    "account_uuid": value.get("account_uuid").cloned().unwrap_or(Value::Null),
                    "account_url": value.get("account_url").cloned().unwrap_or(Value::Null),
                    "user_uuid": value.get("user_uuid").cloned().unwrap_or(Value::Null),
                })
            })
    } else {
        None
    };
    Ok(json!({
        "configured": true,
        "authenticated": authenticated,
        "account": account,
    }))
}

struct ServiceAccountRunSpec {
    cwd: PathBuf,
    command: Vec<String>,
    env_files: Vec<PathBuf>,
    environment_refs: BTreeMap<String, String>,
    allowed_locators: Vec<String>,
}

async fn service_account_run(
    session: &Session,
    token: &str,
    cwd: PathBuf,
    command: Vec<String>,
    env_files: Vec<PathBuf>,
    environment_refs: BTreeMap<String, String>,
    allowed_locators: Vec<String>,
) -> Result<Value> {
    let op_executable = service_account_cli_executable()?;
    service_account_run_with_op(
        session,
        token,
        ServiceAccountRunSpec {
            cwd,
            command,
            env_files,
            environment_refs,
            allowed_locators,
        },
        &op_executable.to_string_lossy(),
    )
    .await
}

async fn service_account_run_with_op(
    session: &Session,
    token: &str,
    spec: ServiceAccountRunSpec,
    op_executable: &str,
) -> Result<Value> {
    let ServiceAccountRunSpec {
        cwd,
        command,
        env_files,
        environment_refs,
        allowed_locators,
    } = spec;
    validate_service_account_run_input(&command, &env_files, &environment_refs, &allowed_locators)?;
    let cwd = config::resolve_cwd(session, Some(&cwd))?;
    let env_files = env_files
        .into_iter()
        .map(|path| {
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            config::resolve_existing_path(session, &path)
        })
        .collect::<Result<Vec<_>>>()?;

    let secret_environment_names =
        service_account_secret_environment_names(&env_files, &environment_refs)?;
    let (mut target_environment, mut resolved_secrets) =
        service_account_resolve_environment_with_op(
            token,
            &cwd,
            &env_files,
            &environment_refs,
            op_executable,
            &secret_environment_names,
        )
        .await?;

    // Nested locators are pre-resolved before target spawn. The broker is an
    // in-memory capability over this exact map, never an `op read` proxy.
    let mut resolved_locators = BTreeMap::new();
    for locator in allowed_locators {
        if resolved_locators.contains_key(&locator) {
            continue;
        }
        let value =
            service_account_read_secret_with_op(session, token, &locator, op_executable).await?;
        resolved_locators.insert(locator, value);
    }

    let broker = if resolved_locators.is_empty() {
        None
    } else {
        Some(secret_broker::SecretBroker::start(resolved_locators).await?)
    };
    let capability_token = broker
        .as_ref()
        .map(|broker| broker.capability_token().to_owned());
    if let Some(broker) = &broker {
        target_environment.insert(
            secret_broker::SOCKET_ENV.to_owned(),
            broker.socket_path().display().to_string(),
        );
        target_environment.insert(
            secret_broker::TOKEN_ENV.to_owned(),
            broker.capability_token().to_owned(),
        );
    }
    target_environment.remove("OP_SERVICE_ACCOUNT_TOKEN");

    #[cfg(target_os = "linux")]
    let output = sandbox::run_unrestricted_with_env_and_spawn_hook_private_pid(
        &command,
        &cwd,
        None,
        &target_environment,
        &[
            "OP_SERVICE_ACCOUNT_TOKEN",
            secret_broker::SOCKET_ENV,
            secret_broker::TOKEN_ENV,
        ],
        |pid| match &broker {
            Some(broker) => broker.bind_target_pid(pid),
            None => Ok(()),
        },
    )
    .await;
    #[cfg(not(target_os = "linux"))]
    let output = sandbox::run_unrestricted_with_env_and_spawn_hook(
        &command,
        &cwd,
        None,
        &target_environment,
        &[
            "OP_SERVICE_ACCOUNT_TOKEN",
            secret_broker::SOCKET_ENV,
            secret_broker::TOKEN_ENV,
        ],
        |pid| match &broker {
            Some(broker) => broker.bind_target_pid(pid),
            None => Ok(()),
        },
    )
    .await;
    if let Some(broker) = broker {
        for secret in broker.close().await {
            if !secret.is_empty() && !resolved_secrets.iter().any(|known| known == &secret) {
                resolved_secrets.push(secret);
            }
        }
    }
    let output = output.context("failed to run command through 1Password service account")?;
    Ok(redact_service_account_output(
        output,
        token,
        capability_token.as_deref(),
        &resolved_secrets,
    ))
}

async fn service_account_resolve_environment_with_op(
    token: &str,
    cwd: &Path,
    env_files: &[PathBuf],
    environment_refs: &BTreeMap<String, String>,
    op_executable: &str,
    secret_environment_names: &BTreeSet<String>,
) -> Result<(HashMap<String, String>, Vec<String>)> {
    let mut op_command = vec![
        op_executable.to_owned(),
        "run".to_owned(),
        "--no-masking".to_owned(),
    ];
    for path in env_files {
        op_command.push(format!("--env-file={}", path.display()));
    }
    op_command.extend([
        "--".to_owned(),
        "/usr/bin/env".to_owned(),
        "-u".to_owned(),
        "OP_SERVICE_ACCOUNT_TOKEN".to_owned(),
        "-u".to_owned(),
        secret_broker::SOCKET_ENV.to_owned(),
        "-u".to_owned(),
        secret_broker::TOKEN_ENV.to_owned(),
        "-0".to_owned(),
    ]);
    let mut environment = environment_refs
        .iter()
        .map(|(name, reference)| (name.clone(), reference.clone()))
        .collect::<HashMap<_, _>>();
    environment.insert("OP_SERVICE_ACCOUNT_TOKEN".to_owned(), token.to_owned());
    let output = sandbox::run_unrestricted_with_env(
        &op_command,
        cwd,
        None,
        &environment,
        &["OP_SERVICE_ACCOUNT_TOKEN"],
    )
    .await
    .context("failed to resolve 1Password service-account environment")?;
    anyhow::ensure!(
        output.status == 0 && !output.truncated,
        "1Password service-account environment resolution failed"
    );
    let mut target_environment = parse_nul_environment(&output.stdout)?;
    target_environment.remove("OP_SERVICE_ACCOUNT_TOKEN");
    target_environment.remove(secret_broker::SOCKET_ENV);
    target_environment.remove(secret_broker::TOKEN_ENV);

    let mut resolved_secrets = Vec::new();
    for name in secret_environment_names {
        let Some(value) = target_environment.get(name) else {
            continue;
        };
        if !value.is_empty() && !resolved_secrets.iter().any(|known| known == value) {
            resolved_secrets.push(value.clone());
        }
    }
    Ok((target_environment, resolved_secrets))
}

fn parse_nul_environment(stdout: &str) -> Result<HashMap<String, String>> {
    let mut environment = HashMap::new();
    for entry in stdout.split('\0').filter(|entry| !entry.is_empty()) {
        let (name, value) = entry
            .split_once('=')
            .context("1Password service-account environment contained an invalid entry")?;
        anyhow::ensure!(
            valid_environment_name(name),
            "1Password service-account environment contained an invalid variable name"
        );
        environment.insert(name.to_owned(), value.to_owned());
    }
    Ok(environment)
}

fn service_account_secret_environment_names(
    env_files: &[PathBuf],
    environment_refs: &BTreeMap<String, String>,
) -> Result<BTreeSet<String>> {
    let mut names = environment_refs.keys().cloned().collect::<BTreeSet<_>>();
    for (name, value) in std::env::vars() {
        if value.contains("op://") && valid_environment_name(&name) {
            names.insert(name);
        }
    }
    for path in env_files {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to inspect 1Password env file {}", path.display()))?;
        for line in contents.lines() {
            let line = line.trim_start();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
            let (name, value) = line.split_once('=').with_context(|| {
                format!(
                    "unsupported 1Password env-file syntax in {}",
                    path.display()
                )
            })?;
            let name = name.trim();
            anyhow::ensure!(
                valid_environment_name(name),
                "invalid 1Password env-file variable name in {}",
                path.display()
            );
            if value.contains("op://") {
                names.insert(name.to_owned());
            }
        }
    }
    Ok(names)
}

fn redact_service_account_output(
    output: sandbox::Output,
    token: &str,
    capability_token: Option<&str>,
    resolved_secrets: &[String],
) -> Value {
    if output.truncated && (capability_token.is_some() || !resolved_secrets.is_empty()) {
        return json!({
            "exit_code": output.status,
            "stdout": REDACTED_TRUNCATED_SECRET_OUTPUT,
            "stderr": REDACTED_TRUNCATED_SECRET_OUTPUT,
            "truncated": true,
        });
    }

    let mut stdout = redact_token(&output.stdout, token);
    let mut stderr = redact_token(&output.stderr, token);
    if let Some(capability_token) = capability_token {
        stdout = redact_value(&stdout, capability_token, "[REDACTED_RESOLVER_CAPABILITY]");
        stderr = redact_value(&stderr, capability_token, "[REDACTED_RESOLVER_CAPABILITY]");
    }
    for secret in resolved_secrets {
        stdout = redact_value(&stdout, secret, "[REDACTED_SECRET]");
        stderr = redact_value(&stderr, secret, "[REDACTED_SECRET]");
    }
    json!({
        "exit_code": output.status,
        "stdout": stdout,
        "stderr": stderr,
        "truncated": output.truncated,
    })
}

async fn service_account_read_secret_with_op(
    session: &Session,
    token: &str,
    locator: &str,
    op_executable: &str,
) -> Result<String> {
    secret_broker::validate_locator(locator)?;
    let command = vec![
        op_executable.to_owned(),
        "read".to_owned(),
        "--no-newline".to_owned(),
        locator.to_owned(),
    ];
    let mut environment = HashMap::new();
    environment.insert("OP_SERVICE_ACCOUNT_TOKEN".to_owned(), token.to_owned());
    let output = sandbox::run_unrestricted_with_env(
        &command,
        &session.cwd,
        None,
        &environment,
        &["OP_SERVICE_ACCOUNT_TOKEN"],
    )
    .await
    .context("failed to invoke 1Password secret resolution")?;
    anyhow::ensure!(
        output.status == 0 && !output.truncated,
        "1Password secret resolution failed"
    );
    Ok(output.stdout)
}

pub(crate) fn validate_service_account_run_input(
    command: &[String],
    env_files: &[PathBuf],
    environment_refs: &BTreeMap<String, String>,
    allowed_locators: &[String],
) -> Result<()> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    anyhow::ensure!(
        !command[0].is_empty(),
        "command executable must not be empty"
    );
    anyhow::ensure!(
        command.len() <= MAX_SERVICE_ACCOUNT_COMMAND_ARGUMENTS,
        "command must contain at most {MAX_SERVICE_ACCOUNT_COMMAND_ARGUMENTS} arguments"
    );
    let mut command_bytes = 0usize;
    for argument in command {
        anyhow::ensure!(
            !argument.contains('\0'),
            "command arguments must not contain NUL bytes"
        );
        anyhow::ensure!(
            argument.len() <= MAX_SERVICE_ACCOUNT_COMMAND_ARGUMENT_BYTES,
            "command argument exceeds {MAX_SERVICE_ACCOUNT_COMMAND_ARGUMENT_BYTES} bytes"
        );
        command_bytes = command_bytes
            .checked_add(argument.len())
            .context("command argument size overflow")?;
        anyhow::ensure!(
            command_bytes <= MAX_SERVICE_ACCOUNT_COMMAND_TOTAL_BYTES,
            "command arguments exceed {MAX_SERVICE_ACCOUNT_COMMAND_TOTAL_BYTES} bytes in total"
        );
    }

    anyhow::ensure!(
        env_files.len() <= MAX_SERVICE_ACCOUNT_ENV_FILES,
        "at most {MAX_SERVICE_ACCOUNT_ENV_FILES} 1Password env files are allowed"
    );
    for path in env_files {
        let bytes = path.as_os_str().as_encoded_bytes();
        anyhow::ensure!(
            !bytes.contains(&0),
            "1Password env file paths must not contain NUL bytes"
        );
        anyhow::ensure!(
            bytes.len() <= MAX_SERVICE_ACCOUNT_ENV_FILE_PATH_BYTES,
            "1Password env file path exceeds {MAX_SERVICE_ACCOUNT_ENV_FILE_PATH_BYTES} bytes"
        );
    }

    anyhow::ensure!(
        environment_refs.len() <= MAX_SERVICE_ACCOUNT_ENV_VARS,
        "at most {MAX_SERVICE_ACCOUNT_ENV_VARS} 1Password secret environment variables are allowed"
    );
    for (name, reference) in environment_refs {
        anyhow::ensure!(
            name.len() <= MAX_SERVICE_ACCOUNT_ENV_NAME_BYTES && valid_environment_name(name),
            "invalid environment variable name: {name}"
        );
        anyhow::ensure!(
            !name.starts_with("OP_"),
            "environment variables beginning with OP_ are reserved for 1Password CLI"
        );
        anyhow::ensure!(
            name != secret_broker::SOCKET_ENV && name != secret_broker::TOKEN_ENV,
            "nested secret resolver environment variables are reserved by Temote MCP"
        );
        anyhow::ensure!(
            reference.len() <= MAX_SERVICE_ACCOUNT_ENV_REF_BYTES,
            "1Password secret reference exceeds {MAX_SERVICE_ACCOUNT_ENV_REF_BYTES} bytes"
        );
        anyhow::ensure!(
            !reference.contains('\0') && reference.starts_with("op://"),
            "1Password environment values must be op:// secret references"
        );
    }

    anyhow::ensure!(
        allowed_locators.len() <= MAX_SERVICE_ACCOUNT_ALLOWED_LOCATORS,
        "at most {MAX_SERVICE_ACCOUNT_ALLOWED_LOCATORS} nested 1Password locators are allowed"
    );
    for locator in allowed_locators {
        secret_broker::validate_locator(locator)?;
    }
    Ok(())
}

fn valid_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn redact_token(text: &str, token: &str) -> String {
    redact_value(text, token, "[REDACTED_SERVICE_ACCOUNT_TOKEN]")
}

fn redact_value(text: &str, value: &str, replacement: &str) -> String {
    if value.is_empty() {
        text.to_owned()
    } else {
        text.replace(value, replacement)
    }
}

fn bounded_console_text_with_layout(
    value: &str,
    max_bytes: usize,
    preserve_layout: bool,
) -> std::borrow::Cow<'_, str> {
    let is_preserved_layout = |character| preserve_layout && matches!(character, '\n' | '\t');
    let console_safe = value
        .chars()
        .all(|character| !character.is_control() || is_preserved_layout(character));
    if console_safe && value.len() <= max_bytes {
        return std::borrow::Cow::Borrowed(value);
    }

    const SUFFIX: &str = "… [truncated]";
    let suffix = if max_bytes >= SUFFIX.len() {
        SUFFIX
    } else {
        ""
    };
    let budget = max_bytes.saturating_sub(suffix.len());
    let mut bounded = String::with_capacity(max_bytes);
    let mut consumed = 0usize;

    for character in value.chars() {
        let escaped = if character.is_control() && !is_preserved_layout(character) {
            Some(if character.is_ascii() {
                format!("\\x{:02x}", character as u32)
            } else {
                format!("\\u{{{:x}}}", character as u32)
            })
        } else {
            None
        };
        let rendered_bytes = escaped
            .as_ref()
            .map_or_else(|| character.len_utf8(), String::len);
        if bounded.len().saturating_add(rendered_bytes) > budget {
            break;
        }
        if let Some(escaped) = escaped {
            bounded.push_str(&escaped);
        } else {
            bounded.push(character);
        }
        consumed += character.len_utf8();
    }

    if consumed < value.len() {
        bounded.push_str(suffix);
    }
    std::borrow::Cow::Owned(bounded)
}

fn bounded_console_text(value: &str, max_bytes: usize) -> std::borrow::Cow<'_, str> {
    bounded_console_text_with_layout(value, max_bytes, true)
}

#[cfg(test)]
fn bounded_console_path(path: &Path) -> String {
    let rendered = path.to_string_lossy();
    bounded_console_text_with_layout(&rendered, MAX_CONSOLE_PATH_BYTES, false).into_owned()
}

fn show_activity_for_session(session_id: &str, title: &str, detail: Option<&str>) {
    let title = bounded_console_text(title, MAX_ACTIVITY_TITLE_BYTES);
    eprintln!("\n[session {session_id}] • {title}");
    if let Some(detail) = detail.filter(|value| !value.is_empty()) {
        let detail = bounded_console_text(detail, MAX_ACTIVITY_DETAIL_BYTES);
        for line in detail.lines() {
            eprintln!("  {line}");
        }
    }
}

#[cfg(test)]
fn permission_arg<'a>(command: &'a str, action: &str) -> Option<&'a str> {
    ["/permission", "/permissions"]
        .into_iter()
        .find_map(|prefix| {
            command
                .strip_prefix(&format!("{prefix} {action} "))
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;

    use super::*;
    use crate::test_support;

    fn test_session(root: &Path) -> Session {
        let root = config::canonical_directory(root).unwrap();
        Session {
            id: "service-account-test".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root],
            started_at: 0,
            process_id: 0,
            yolo: false,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn service_account_cli_metadata_requires_kernel_credential_transition() {
        assert!(
            validate_service_account_cli_metadata(0, 1002, 0o102755, 1000, 1000, &[4, 24, 100], 2)
                .is_ok()
        );
        for invalid in [
            validate_service_account_cli_metadata(1000, 1002, 0o102755, 1000, 1000, &[], 2),
            validate_service_account_cli_metadata(0, 1002, 0o100755, 1000, 1000, &[], 2),
            validate_service_account_cli_metadata(0, 1002, 0o102644, 1000, 1000, &[], 2),
            validate_service_account_cli_metadata(0, 1002, 0o102775, 1000, 1000, &[], 2),
            validate_service_account_cli_metadata(0, 1000, 0o102755, 1000, 1000, &[], 2),
            validate_service_account_cli_metadata(0, 1002, 0o102755, 1000, 1000, &[1002], 2),
            validate_service_account_cli_metadata(0, 1002, 0o102755, 0, 0, &[], 2),
            validate_service_account_cli_metadata(0, 1002, 0o102755, 1000, 1000, &[], 1),
            validate_service_account_cli_metadata(0, 1002, 0o102755, 1000, 1000, &[], 3),
        ] {
            assert!(invalid.is_err());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn upgrade_exec_handoff_never_places_raw_service_account_token_in_environment() {
        use std::ffi::OsStr;

        let environment = CapturedStartEnvironment::from_values(BTreeMap::from([
            (
                "OP_SERVICE_ACCOUNT_TOKEN".to_owned(),
                "fabricated-upgrade-token-42".to_owned(),
            ),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ]))
        .unwrap();
        let mut command = std::process::Command::new("/bin/true");
        command.env("OP_SERVICE_ACCOUNT_TOKEN", "ambient-token");
        let handoff = environment.apply_to_command(&mut command).unwrap();
        let envs = command.get_envs().collect::<Vec<_>>();

        let raw_token = envs
            .iter()
            .find(|(name, _)| *name == OsStr::new("OP_SERVICE_ACCOUNT_TOKEN"))
            .map(|(_, value)| *value);
        assert_eq!(raw_token, Some(None));

        let fd_value = envs
            .iter()
            .find(|(name, _)| *name == OsStr::new(SERVICE_ACCOUNT_TOKEN_FD_ENV))
            .and_then(|(_, value)| *value)
            .expect("credential handoff FD must be present")
            .to_string_lossy()
            .parse::<libc::c_int>()
            .unwrap();
        assert!(fd_value >= 3);
        let seals = unsafe { libc::fcntl(fd_value, libc::F_GET_SEALS) };
        let required_seals =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        assert_eq!(seals & required_seals, required_seals);

        drop(handoff);
    }

    #[test]
    fn restart_context_comparison_reports_keys_without_values() {
        let captured = CapturedStartEnvironment::from_values(BTreeMap::from([
            ("PATH".to_owned(), "/bin".to_owned()),
            ("KINTONE_PASSWORD".to_owned(), "secret-a".to_owned()),
        ]))
        .unwrap();
        let available = CapturedStartEnvironment::from_values(BTreeMap::from([
            ("PATH".to_owned(), "/bin".to_owned()),
            ("KINTONE_PASSWORD".to_owned(), "secret-b".to_owned()),
            ("HOME".to_owned(), "/tmp/home".to_owned()),
        ]))
        .unwrap();
        let mismatches = captured.restart_context_mismatches(&available);
        assert_eq!(mismatches, ["KINTONE_PASSWORD"]);
        assert_eq!(
            captured.restart_context_keys(),
            ["KINTONE_PASSWORD", "PATH"]
        );
        let selected = available
            .select_restart_context(&["PATH".to_owned()])
            .unwrap();
        assert_eq!(selected.restart_context_keys(), ["PATH"]);
        let rendered = format!("{mismatches:?}");
        assert!(!rendered.contains("secret-a"));
        assert!(!rendered.contains("secret-b"));
    }

    #[test]
    fn generated_console_text_bounds_are_utf8_safe() -> noprop::TestResult {
        test_support::run(0x434f_4e53_4f4c_4501, 1024, |ctx| {
            let max_bytes = noprop::sample_usize_in(ctx, 0..=1024);
            let count = noprop::sample_usize_in(ctx, 0..=1024);
            let character = match noprop::sample_usize_in(ctx, 0..4) {
                0 => "x",
                1 => "é",
                2 => "界",
                _ => "😀",
            };
            let value = character.repeat(count);
            let bounded = bounded_console_text(&value, max_bytes);
            assert!(
                bounded.len() <= max_bytes,
                "max={max_bytes} actual={}",
                bounded.len()
            );
            if value.len() <= max_bytes {
                assert_eq!(bounded, value);
            } else {
                const SUFFIX: &str = "… [truncated]";
                if max_bytes >= SUFFIX.len() {
                    assert!(bounded.ends_with(SUFFIX));
                    let prefix = bounded.strip_suffix(SUFFIX).unwrap();
                    assert!(value.starts_with(prefix));
                } else {
                    assert!(value.starts_with(bounded.as_ref()));
                }
            }
            Ok(())
        })
    }

    #[test]
    fn console_text_escapes_terminal_controls_but_preserves_layout() {
        let value = "ok\x1b[31m\r\n\tend\u{0085}";
        let bounded = bounded_console_text(value, 1024);
        assert_eq!(bounded, "ok\\x1b[31m\\x0d\n\tend\\u{85}");
        assert!(!bounded.contains('\x1b'));
        assert!(!bounded.contains('\r'));
        assert!(bounded.contains('\n'));
        assert!(bounded.contains('\t'));
    }

    #[test]
    fn generated_console_paths_are_single_line_and_control_free() -> noprop::TestResult {
        test_support::run(0x434f_4e53_5041_5448, test_support::DEFAULT_CASES, |ctx| {
            let len = noprop::sample_usize_in(ctx, 0..=512);
            let value = (0..len)
                .map(|_| char::from(noprop::sample_u8(ctx) & 0x7f))
                .collect::<String>();
            let rendered = bounded_console_path(Path::new(&value));
            assert!(rendered.len() <= MAX_CONSOLE_PATH_BYTES);
            assert!(
                !rendered.chars().any(char::is_control),
                "console path retained terminal control characters: {rendered:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn console_paths_escape_terminal_controls() {
        let path = Path::new("safe/\x1b[31m\rname");
        let rendered = bounded_console_path(path);
        assert_eq!(rendered, "safe/\\x1b[31m\\x0dname");
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\r'));
        assert!(rendered.len() <= MAX_CONSOLE_PATH_BYTES);
    }

    #[test]
    fn generated_session_json_encoding_matches_wire_limit() -> noprop::TestResult {
        test_support::run(0x5345_5353_4c49_4e45, 512, |ctx| {
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
            let message = Message::Activity {
                title: payload,
                detail: None,
            };
            let serialized = serde_json::to_vec(&message).unwrap();
            let expected = serialized
                .len()
                .checked_add(1)
                .is_some_and(|wire| wire <= max_bytes);
            let actual = encode_json_line_with_limit(&message, max_bytes);
            assert_eq!(
                actual.is_ok(),
                expected,
                "serialized={} max={max_bytes}",
                serialized.len()
            );
            if let Ok(line) = actual {
                assert_eq!(line.len(), serialized.len() + 1);
                assert_eq!(line.last(), Some(&b'\n'));
            }
            Ok(())
        })
    }

    #[test]
    fn oversized_session_request_is_rejected_before_socket_write() {
        let message = Message::KintoneMcp {
            request: KintoneMcpRequest::Call {
                tool_name: "oversized".to_owned(),
                arguments: json!({"payload": "x".repeat(MAX_SESSION_MESSAGE_BYTES)}),
            },
        };
        let error = encode_session_json_line(&message).unwrap_err();
        assert!(error.to_string().contains("session message exceeds"));
    }

    #[test]
    fn oversized_session_result_degrades_to_bounded_error_response() {
        let bytes = encode_session_result(
            Ok(json!({"payload": "x".repeat(MAX_SESSION_MESSAGE_BYTES)})),
            "generated response",
        );
        assert!(bytes.len() <= MAX_SESSION_MESSAGE_BYTES);
        assert_eq!(bytes.last(), Some(&b'\n'));
        let response: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert!(response["result"].is_null());
        assert!(
            response["error"]
                .as_str()
                .is_some_and(|error| error.contains("generated response exceeds"))
        );
    }

    #[test]
    fn legacy_service_account_run_request_defaults_nested_locator_scope() {
        let request: ServiceAccountRequest = serde_json::from_value(json!({
            "operation": "run",
            "cwd": "/tmp",
            "command": ["true"],
            "env_files": [],
            "environment": {}
        }))
        .unwrap();
        match request {
            ServiceAccountRequest::Run {
                allowed_locators, ..
            } => {
                assert!(allowed_locators.is_empty());
            }
            ServiceAccountRequest::Status => panic!("expected run request"),
        }
    }

    #[test]
    fn truncated_output_after_secret_resolution_is_fully_redacted() {
        let secret = "resolved-secret-sensitive-value".to_owned();
        let prefix = &secret[..8];
        let stdout = format!(
            "{}{}",
            "x".repeat(sandbox::MAX_COMMAND_OUTPUT_BYTES - prefix.len()),
            prefix
        );
        assert_eq!(stdout.len(), sandbox::MAX_COMMAND_OUTPUT_BYTES);
        assert!(stdout.ends_with(prefix));

        let result = redact_service_account_output(
            sandbox::Output {
                status: 0,
                stdout,
                stderr: String::new(),
                truncated: true,
            },
            "service-account-token",
            Some("resolver-capability-token"),
            std::slice::from_ref(&secret),
        );
        let rendered = serde_json::to_string(&result).unwrap();
        assert_eq!(result["stdout"], REDACTED_TRUNCATED_SECRET_OUTPUT);
        assert_eq!(result["stderr"], REDACTED_TRUNCATED_SECRET_OUTPUT);
        assert_eq!(result["truncated"], true);
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains("resolved-secret"));
        assert!(!rendered.contains(prefix));
        assert!(!rendered.contains("service-account-token"));
        assert!(!rendered.contains("resolver-capability-token"));
    }

    #[test]
    fn truncated_output_with_unresolved_capability_is_fully_redacted() {
        let capability = "resolver-capability-sensitive-value";
        let prefix = &capability[..10];
        let result = redact_service_account_output(
            sandbox::Output {
                status: 0,
                stdout: format!("{}{}", "x".repeat(128), prefix),
                stderr: String::new(),
                truncated: true,
            },
            "service-account-token",
            Some(capability),
            &[],
        );
        let rendered = serde_json::to_string(&result).unwrap();
        assert_eq!(result["stdout"], REDACTED_TRUNCATED_SECRET_OUTPUT);
        assert_eq!(result["stderr"], REDACTED_TRUNCATED_SECRET_OUTPUT);
        assert!(!rendered.contains(prefix));
        assert!(!rendered.contains(capability));
    }

    #[test]
    fn truncated_output_without_resolved_nested_secret_keeps_existing_capture_semantics() {
        let result = redact_service_account_output(
            sandbox::Output {
                status: 7,
                stdout: "ordinary-truncated-output".to_owned(),
                stderr: "ordinary-error".to_owned(),
                truncated: true,
            },
            "service-account-token",
            None,
            &[],
        );
        assert_eq!(result["stdout"], "ordinary-truncated-output");
        assert_eq!(result["stderr"], "ordinary-error");
        assert_eq!(result["truncated"], true);
    }

    #[tokio::test]
    async fn service_account_status_without_token_is_non_secret_and_inactive() {
        let root = tempfile::tempdir().unwrap();
        let session = test_session(root.path());
        let result = handle_service_account_request(&session, None, ServiceAccountRequest::Status)
            .await
            .unwrap();
        assert_eq!(result["configured"], false);
        assert_eq!(result["authenticated"], false);
    }

    #[tokio::test]
    async fn service_account_run_without_token_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let session = test_session(root.path());
        let error = handle_service_account_request(
            &session,
            None,
            ServiceAccountRequest::Run {
                cwd: session.cwd.clone(),
                command: vec!["true".to_owned()],
                env_files: Vec::new(),
                environment: BTreeMap::new(),
                allowed_locators: vec!["op://vault/item/field".to_owned()],
            },
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("service account is not configured")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nested_resolver_process_client_helper() {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::os::unix::net::UnixStream;

        let Ok(socket) = std::env::var(secret_broker::SOCKET_ENV) else {
            return;
        };
        let token =
            std::env::var(secret_broker::TOKEN_ENV).expect("resolver capability is present");
        assert!(std::env::var_os("OP_SERVICE_ACCOUNT_TOKEN").is_none());
        let raw_token_marker = b"service-account-token-must-not-leak";
        for entry in std::fs::read_dir("/proc").expect("child can inspect proc") {
            let entry = entry.expect("proc entry");
            if !entry
                .file_name()
                .to_string_lossy()
                .chars()
                .all(|character| character.is_ascii_digit())
            {
                continue;
            }
            let environ = std::fs::read(entry.path().join("environ")).unwrap_or_default();
            assert!(
                !environ
                    .windows(raw_token_marker.len())
                    .any(|window| window == raw_token_marker),
                "raw service-account token was observable through /proc"
            );
        }
        let mut stream = UnixStream::connect(socket).expect("child connects to nested resolver");
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "token": token,
                "locator": "op://vault/item/field",
            })
        )
        .unwrap();
        stream.flush().unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        let response: Value = serde_json::from_str(response.trim_end()).unwrap();
        assert!(response["error"].is_null());
        let value = response["value"].as_str().expect("resolved secret value");
        assert_eq!(value, "resolved-secret");
        println!("{value}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn nested_resolver_child_resolves_without_service_account_token() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let fake_op = root.path().join("op");
        std::fs::write(
            &fake_op,
            r#"#!/bin/sh
set -eu
operation="$1"
shift
case "$operation" in
  run)
    while [ "$1" != "--" ]; do shift; done
    shift
    exec "$@"
    ;;
  read)
    if [ "${1:-}" = "--no-newline" ]; then shift; fi
    sleep 0.2
    case "${1:-}" in
      op://vault/item/field) printf '%s' 'resolved-secret' ;;
      *) exit 7 ;;
    esac
    ;;
  *) exit 8 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fake_op, std::fs::Permissions::from_mode(0o700)).unwrap();
        let session = test_session(root.path());
        let current_exe = std::env::current_exe().unwrap();
        let command = vec![
            current_exe.display().to_string(),
            "--exact".to_owned(),
            "approvals::tests::nested_resolver_process_client_helper".to_owned(),
            "--nocapture".to_owned(),
        ];
        let result = service_account_run_with_op(
            &session,
            "service-account-token-must-not-leak",
            ServiceAccountRunSpec {
                cwd: session.cwd.clone(),
                command,
                env_files: Vec::new(),
                environment_refs: BTreeMap::new(),
                allowed_locators: vec!["op://vault/item/field".to_owned()],
            },
            fake_op.to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result["exit_code"], 0, "{}", result["stderr"]);
        let stdout = result["stdout"].as_str().unwrap();
        assert!(stdout.contains("[REDACTED_SECRET]"));
        assert!(!stdout.contains("resolved-secret"));
        assert!(!stdout.contains("service-account-token-must-not-leak"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn direct_environment_is_resolved_before_target_spawn_and_redacted() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let fake_op = root.path().join("op");
        std::fs::write(
            &fake_op,
            r#"#!/bin/sh
set -eu
operation="$1"
shift
case "$operation" in
  run)
    while [ "$1" != "--" ]; do shift; done
    shift
    export DIRECT_SECRET='resolved-direct-secret'
    exec "$@"
    ;;
  *) exit 8 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fake_op, std::fs::Permissions::from_mode(0o700)).unwrap();
        let session = test_session(root.path());
        let result = service_account_run_with_op(
            &session,
            "service-account-token-must-not-leak",
            ServiceAccountRunSpec {
                cwd: session.cwd.clone(),
                command: vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "printf '%s' \"$DIRECT_SECRET\"".to_owned(),
                ],
                env_files: Vec::new(),
                environment_refs: BTreeMap::from([(
                    "DIRECT_SECRET".to_owned(),
                    "op://vault/item/direct".to_owned(),
                )]),
                allowed_locators: Vec::new(),
            },
            fake_op.to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result["exit_code"], 0, "{}", result["stderr"]);
        assert_eq!(result["stdout"], "[REDACTED_SECRET]");
        let rendered = serde_json::to_string(&result).unwrap();
        assert!(!rendered.contains("resolved-direct-secret"));
        assert!(!rendered.contains("service-account-token-must-not-leak"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn mixed_env_file_redacts_only_resolved_secret_values() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let env_file = root.path().join("mixed.env");
        std::fs::write(
            &env_file,
            "MODE=production\nRETRY=1\nPASSWORD=op://vault/item/password\n",
        )
        .unwrap();
        let fake_op = root.path().join("op");
        std::fs::write(
            &fake_op,
            r#"#!/bin/sh
set -eu
operation="$1"
shift
case "$operation" in
  run)
    while [ "$1" != "--" ]; do shift; done
    shift
    export MODE='production'
    export RETRY='1'
    export PASSWORD='resolved-password'
    exec "$@"
    ;;
  *) exit 8 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fake_op, std::fs::Permissions::from_mode(0o700)).unwrap();
        let session = test_session(root.path());
        let result = service_account_run_with_op(
            &session,
            "service-account-token-must-not-leak",
            ServiceAccountRunSpec {
                cwd: session.cwd.clone(),
                command: vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    r#"printf '%s\n%s\n%s\n' "$MODE" "$RETRY" "$PASSWORD""#.to_owned(),
                ],
                env_files: vec![env_file],
                environment_refs: BTreeMap::new(),
                allowed_locators: Vec::new(),
            },
            fake_op.to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result["exit_code"], 0, "{}", result["stderr"]);
        assert_eq!(result["stdout"], "production\n1\n[REDACTED_SECRET]\n");
    }

    #[test]
    fn ambiguous_env_file_syntax_fails_closed_before_redaction_scope_is_built() {
        let root = tempfile::tempdir().unwrap();
        let env_file = root.path().join("ambiguous.env");
        std::fs::write(
            &env_file,
            "PASSWORD=op://vault/item/password\ncontinued-without-assignment\n",
        )
        .unwrap();
        let error =
            service_account_secret_environment_names(&[env_file], &BTreeMap::new()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported 1Password env-file syntax")
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn service_account_status_runs_while_long_lived_target_is_active() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let active = root.path().join("target-active");
        let observed = root.path().join("status-during-active");
        let fake_op = root.path().join("op");
        let script = format!(
            r#"#!/bin/sh
set -eu
operation="$1"
shift
case "$operation" in
  run)
    while [ "$1" != "--" ]; do shift; done
    shift
    exec "$@"
    ;;
  whoami)
    if [ -e '{}' ]; then
      touch '{}'
    fi
    printf '%s' '{{"account_uuid":"a","account_url":"u","user_uuid":"x"}}'
    ;;
  *) exit 8 ;;
esac
"#,
            active.display(),
            observed.display()
        );
        std::fs::write(&fake_op, script).unwrap();
        std::fs::set_permissions(&fake_op, std::fs::Permissions::from_mode(0o700)).unwrap();
        let session = test_session(root.path());
        let target_command = format!(
            "touch '{}'; sleep 0.30; rm -f '{}'",
            active.display(),
            active.display()
        );
        let run = service_account_run_with_op(
            &session,
            "service-account-token-must-not-leak",
            ServiceAccountRunSpec {
                cwd: session.cwd.clone(),
                command: vec!["/bin/sh".to_owned(), "-c".to_owned(), target_command],
                env_files: Vec::new(),
                environment_refs: BTreeMap::new(),
                allowed_locators: Vec::new(),
            },
            fake_op.to_str().unwrap(),
        );
        let status = async {
            for _ in 0..100 {
                if active.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(active.exists(), "target never entered active phase");
            service_account_status_with_op(
                &session,
                "service-account-token-must-not-leak",
                fake_op.to_str().unwrap(),
            )
            .await
        };
        let (run, status) = tokio::join!(run, status);
        assert_eq!(run.unwrap()["exit_code"], 0);
        let status = status.unwrap();
        assert_eq!(status["authenticated"], true);
        assert!(
            observed.exists(),
            "status call waited for target completion"
        );
        assert!(!active.exists());
    }

    #[tokio::test]
    async fn service_account_run_rejects_plaintext_environment_values_before_op_exec() {
        let root = tempfile::tempdir().unwrap();
        let session = test_session(root.path());
        let error = service_account_run(
            &session,
            "fake-token-that-must-never-run",
            session.cwd.clone(),
            vec!["true".to_owned()],
            Vec::new(),
            BTreeMap::from([("API_TOKEN".to_owned(), "plaintext-secret".to_owned())]),
            Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("op:// secret references"));
    }

    #[test]
    fn generated_service_account_input_budget_matches_reference_model() -> noprop::TestResult {
        test_support::run(0x4f50_5341_4255_4447, 256, |ctx| {
            let command_count =
                noprop::sample_usize_in(ctx, 1..=MAX_SERVICE_ACCOUNT_COMMAND_ARGUMENTS + 4);
            let env_file_count =
                noprop::sample_usize_in(ctx, 0..=MAX_SERVICE_ACCOUNT_ENV_FILES + 4);
            let env_var_count = noprop::sample_usize_in(ctx, 0..=MAX_SERVICE_ACCOUNT_ENV_VARS + 4);
            let mutation = noprop::sample_usize_in(ctx, 0..=9);

            let mut command = (0..command_count)
                .map(|index| format!("arg-{index}"))
                .collect::<Vec<_>>();
            let mut env_files = (0..env_file_count)
                .map(|index| PathBuf::from(format!("env-{index}.env")))
                .collect::<Vec<_>>();
            let mut environment_refs = (0..env_var_count)
                .map(|index| {
                    (
                        format!("SECRET_{index}"),
                        format!("op://vault/item/field-{index}"),
                    )
                })
                .collect::<BTreeMap<_, _>>();

            match mutation {
                1 => command[0].clear(),
                2 => command[0].push('\0'),
                3 => command[0] = "x".repeat(MAX_SERVICE_ACCOUNT_COMMAND_ARGUMENT_BYTES + 1),
                4 => {
                    command = (0..5)
                        .map(|index| format!("{index}{}", "x".repeat(30 * 1024)))
                        .collect();
                }
                5 => env_files.push(PathBuf::from(
                    "x".repeat(MAX_SERVICE_ACCOUNT_ENV_FILE_PATH_BYTES + 1),
                )),
                6 => {
                    environment_refs.insert(
                        "OP_ACCOUNT".to_owned(),
                        "op://vault/item/account".to_owned(),
                    );
                }
                7 => {
                    environment_refs.insert("BAD-NAME".to_owned(), "op://vault/item/x".to_owned());
                }
                8 => {
                    environment_refs.insert("SECRET_BAD".to_owned(), "plaintext".to_owned());
                }
                9 => {
                    environment_refs.insert(
                        "SECRET_LONG".to_owned(),
                        format!("op://{}", "x".repeat(MAX_SERVICE_ACCOUNT_ENV_REF_BYTES + 1)),
                    );
                }
                _ => {}
            }

            let command_total = command
                .iter()
                .try_fold(0usize, |total, argument| total.checked_add(argument.len()));
            let expected_command = !command.is_empty()
                && !command[0].is_empty()
                && command.len() <= MAX_SERVICE_ACCOUNT_COMMAND_ARGUMENTS
                && command.iter().all(|argument| {
                    !argument.contains('\0')
                        && argument.len() <= MAX_SERVICE_ACCOUNT_COMMAND_ARGUMENT_BYTES
                })
                && command_total
                    .is_some_and(|bytes| bytes <= MAX_SERVICE_ACCOUNT_COMMAND_TOTAL_BYTES);
            let expected_files = env_files.len() <= MAX_SERVICE_ACCOUNT_ENV_FILES
                && env_files.iter().all(|path| {
                    let bytes = path.as_os_str().as_encoded_bytes();
                    !bytes.contains(&0) && bytes.len() <= MAX_SERVICE_ACCOUNT_ENV_FILE_PATH_BYTES
                });
            let expected_refs = environment_refs.len() <= MAX_SERVICE_ACCOUNT_ENV_VARS
                && environment_refs.iter().all(|(name, reference)| {
                    name.len() <= MAX_SERVICE_ACCOUNT_ENV_NAME_BYTES
                        && valid_environment_name(name)
                        && !name.starts_with("OP_")
                        && reference.len() <= MAX_SERVICE_ACCOUNT_ENV_REF_BYTES
                        && !reference.contains('\0')
                        && reference.starts_with("op://")
                });
            let result =
                validate_service_account_run_input(&command, &env_files, &environment_refs, &[]);
            assert_eq!(
                result.is_ok(),
                expected_command && expected_files && expected_refs,
                "command_count={} env_files={} env_vars={} mutation={mutation}",
                command.len(),
                env_files.len(),
                environment_refs.len()
            );
            Ok(())
        })
    }

    #[tokio::test]
    async fn service_account_run_rejects_reserved_op_environment_names() {
        let root = tempfile::tempdir().unwrap();
        let session = test_session(root.path());
        let error = service_account_run(
            &session,
            "fake-token-that-must-never-run",
            session.cwd.clone(),
            vec!["true".to_owned()],
            Vec::new(),
            BTreeMap::from([(
                "OP_ACCOUNT".to_owned(),
                "op://vault/item/account".to_owned(),
            )]),
            Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("reserved for 1Password CLI"));
    }

    #[test]
    fn service_account_helpers_validate_names_and_redact_tokens() {
        assert!(valid_environment_name("API_TOKEN"));
        assert!(valid_environment_name("_TOKEN2"));
        assert!(!valid_environment_name("2TOKEN"));
        assert!(!valid_environment_name("BAD-NAME"));
        assert_eq!(
            redact_token("before secret-token after", "secret-token"),
            "before [REDACTED_SERVICE_ACCOUNT_TOKEN] after"
        );
    }

    #[tokio::test]
    async fn malformed_ipc_message_does_not_stop_runtime() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("malformed-ipc-{}", Uuid::new_v4());
        let (sender, _receiver) = approval_channel();
        let handle = spawn_runtime(root.path(), Some(&id), false, sender)
            .await
            .unwrap();
        let path = config::socket_path(&id).unwrap();
        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream.write_all(b"{not-json}\n").await.unwrap();
        stream.shutdown().await.unwrap();
        let mut response = String::new();
        let read = BufReader::new(stream)
            .read_line(&mut response)
            .await
            .unwrap();
        assert_eq!(
            read, 0,
            "invalid messages should be closed without a response"
        );

        let snapshot = handle.snapshot().await.unwrap();
        assert_eq!(snapshot.id, id);
        assert!(!snapshot.yolo);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn disconnected_response_clients_do_not_stop_runtime() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("disconnect-ipc-{}", Uuid::new_v4());
        let (sender, _receiver) = approval_channel();
        let handle = spawn_runtime(root.path(), Some(&id), true, sender)
            .await
            .unwrap();
        let path = config::socket_path(&id).unwrap();

        let mut probe = UnixStream::connect(&path).await.unwrap();
        probe.write_all(b"{\"type\":\"probe\"}\n").await.unwrap();
        drop(probe);

        let request = Message::Approval {
            request: Request {
                id: Uuid::new_v4(),
                operation: "disconnect-test".to_owned(),
                detail: "client closes before allow response".to_owned(),
                cwd: root.path().to_path_buf(),
            },
        };
        let mut approval = UnixStream::connect(&path).await.unwrap();
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        approval.write_all(&bytes).await.unwrap();
        drop(approval);

        tokio::time::sleep(Duration::from_millis(50)).await;
        let snapshot = handle.snapshot().await.unwrap();
        assert_eq!(snapshot.id, id);
        assert!(snapshot.yolo);
        assert!(config::session_is_active(&id).await.unwrap());
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn graceful_shutdown_persists_stopped_lifecycle() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("graceful-lifecycle-{}", Uuid::new_v4());
        let (sender, _receiver) = approval_channel();
        let handle = spawn_runtime(root.path(), Some(&id), false, sender)
            .await
            .unwrap();

        handle.shutdown().await.unwrap();
        let lifecycle = config::read_session_lifecycle(&id).await.unwrap().unwrap();
        assert_eq!(lifecycle.status, config::LifecycleStatus::Stopped);
        assert!(lifecycle.stopped_at.is_some());
        assert_eq!(lifecycle.exit_reason.as_deref(), Some("graceful shutdown"));
        assert!(lifecycle.last_error.is_none());
    }

    #[tokio::test]
    async fn runtime_failure_persists_crashed_lifecycle() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("crashed-lifecycle-{}", Uuid::new_v4());
        let (sender, _receiver) = approval_channel();
        let handle = spawn_runtime(root.path(), Some(&id), false, sender)
            .await
            .unwrap();

        handle.crash_for_test().await.unwrap();
        let error = handle.wait().await.unwrap_err();
        assert!(format!("{error:#}").contains("injected runtime crash"));
        let lifecycle = config::read_session_lifecycle(&id).await.unwrap().unwrap();
        assert_eq!(lifecycle.status, config::LifecycleStatus::Crashed);
        assert!(lifecycle.stopped_at.is_some());
        assert_eq!(
            lifecycle.exit_reason.as_deref(),
            Some("unexpected runtime termination")
        );
        assert!(
            lifecycle
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("injected runtime crash"))
        );
        assert!(!config::session_is_active(&id).await.unwrap());
    }

    #[tokio::test]
    async fn shutdown_after_runtime_failure_does_not_overwrite_crashed_lifecycle() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("crashed-shutdown-race-{}", Uuid::new_v4());
        let (sender, _receiver) = approval_channel();
        let handle = spawn_runtime(root.path(), Some(&id), false, sender)
            .await
            .unwrap();

        handle.crash_for_test().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if handle.is_finished() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        handle.shutdown().await.unwrap();

        let lifecycle = config::read_session_lifecycle(&id).await.unwrap().unwrap();
        assert_eq!(lifecycle.status, config::LifecycleStatus::Crashed);
        assert_eq!(
            lifecycle.exit_reason.as_deref(),
            Some("unexpected runtime termination")
        );
    }

    #[test]
    fn generated_session_response_bounds_match_wire_limit() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        test_support::run(0x4150_5052_5253_504e, 256, |ctx| {
            const LIMIT: usize = 64;
            let payload_len = noprop::sample_usize_in(ctx, 0..=LIMIT + 16);
            runtime.block_on(async {
                let (mut writer, reader) = UnixStream::pair().unwrap();
                let mut bytes = vec![b'x'; payload_len];
                bytes.push(b'\n');
                writer.write_all(&bytes).await.unwrap();
                writer.shutdown().await.unwrap();
                let result = read_session_response(reader, LIMIT, "generated response").await;
                assert_eq!(
                    result.is_ok(),
                    payload_len < LIMIT,
                    "response bound mismatch for payload_len={payload_len}"
                );
            });
            Ok(())
        })
    }

    #[test]
    fn generated_incoming_queue_holds_read_permits_until_dequeue() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        test_support::run(0x4950_4351_5545_5545, 256, |ctx| {
            let limit = noprop::sample_usize_in(ctx, 1..=8);
            let attempts = noprop::sample_usize_in(ctx, 0..=limit + 8);
            runtime.block_on(async {
                let slots = Arc::new(Semaphore::new(limit));
                let (sender, mut receiver) = mpsc::channel(limit);
                let mut peers = Vec::new();
                let mut queued = 0usize;

                for _ in 0..attempts {
                    let permit = match Arc::clone(&slots).try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => break,
                    };
                    let (stream, peer) = UnixStream::pair().unwrap();
                    peers.push(peer);
                    assert!(queue_incoming_session_message(
                        &sender,
                        stream,
                        Message::Probe,
                        permit,
                    ));
                    queued += 1;
                }

                assert_eq!(queued, attempts.min(limit));
                assert_eq!(slots.available_permits(), limit - queued);
                if queued > 0 {
                    let incoming = receiver.try_recv().unwrap();
                    drop(incoming);
                    assert_eq!(slots.available_permits(), limit - queued + 1);
                }
                drop(peers);
            });
            Ok(())
        })
    }

    #[tokio::test]
    async fn idle_ipc_connection_does_not_block_runtime_control_or_probe() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("idle-ipc-{}", Uuid::new_v4());
        let (sender, _receiver) = approval_channel();
        let handle = spawn_runtime(root.path(), Some(&id), false, sender)
            .await
            .unwrap();
        let path = config::socket_path(&id).unwrap();
        let _idle = UnixStream::connect(&path).await.unwrap();

        let snapshot = tokio::time::timeout(Duration::from_millis(500), handle.snapshot())
            .await
            .expect("idle IPC client blocked runtime snapshot")
            .unwrap();
        assert_eq!(snapshot.id, id);
        assert!(
            tokio::time::timeout(Duration::from_millis(500), config::session_is_active(&id))
                .await
                .expect("idle IPC client blocked session probe")
                .unwrap()
        );
        tokio::time::timeout(Duration::from_millis(500), handle.shutdown())
            .await
            .expect("idle IPC client blocked runtime shutdown")
            .unwrap();
        assert!(!config::socket_path(&id).unwrap().exists());
    }

    #[tokio::test]
    async fn oversized_ipc_message_does_not_stop_runtime() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("oversized-ipc-{}", Uuid::new_v4());
        let (sender, _receiver) = approval_channel();
        let handle = spawn_runtime(root.path(), Some(&id), false, sender)
            .await
            .unwrap();
        let path = config::socket_path(&id).unwrap();
        let mut stream = UnixStream::connect(&path).await.unwrap();
        let oversized = vec![b'x'; MAX_SESSION_MESSAGE_BYTES + 1];
        let _ = stream.write_all(&oversized).await;
        let _ = stream.shutdown().await;
        let mut response = String::new();
        let _ = BufReader::new(stream).read_line(&mut response).await;

        let snapshot = handle.snapshot().await.unwrap();
        assert_eq!(snapshot.id, id);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn pending_approval_capacity_denies_excess_without_queueing() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("approval-cap-{}", Uuid::new_v4());
        let (sender, mut receiver) = approval_channel();
        let handle = spawn_runtime(root.path(), Some(&id), false, sender)
            .await
            .unwrap();
        let mut requests = Vec::with_capacity(MAX_PENDING_APPROVALS);
        let mut prompts = Vec::with_capacity(MAX_PENDING_APPROVALS);
        for index in 0..MAX_PENDING_APPROVALS {
            let request_id = id.clone();
            let cwd = root.path().to_path_buf();
            requests.push(tokio::spawn(async move {
                request(&request_id, "capacity", format!("request-{index}"), cwd).await
            }));
            prompts.push(
                tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                    .await
                    .expect("approval prompt was not delivered")
                    .expect("approval channel closed unexpectedly"),
            );
        }

        let request_id = id.clone();
        let cwd = root.path().to_path_buf();
        let excess = tokio::spawn(async move {
            request(&request_id, "capacity", "excess".to_owned(), cwd).await
        });
        let allowed = tokio::time::timeout(Duration::from_secs(1), excess)
            .await
            .expect("over-capacity approval did not resolve immediately")
            .unwrap()
            .unwrap();
        assert!(!allowed, "over-capacity approval was allowed");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), receiver.recv())
                .await
                .is_err(),
            "over-capacity approval was queued for human review"
        );

        handle.shutdown().await.unwrap();
        drop(prompts);
        for request in requests {
            let allowed = tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .expect("approval request did not resolve")
                .unwrap()
                .unwrap();
            assert!(!allowed);
        }
    }

    #[test]
    fn generated_approval_channel_matches_bounded_queue_model() -> noprop::TestResult {
        test_support::run(0x4150_5052_5142_4f55, 256, |ctx| {
            let capacity = noprop::sample_usize_in(ctx, 1..=16);
            let steps = noprop::sample_usize_in(ctx, 1..=128);
            let (sender, mut receiver) = approval_channel_with_capacity(capacity);
            let mut expected = VecDeque::<String>::new();

            for index in 0..steps {
                let should_send = expected.is_empty() || noprop::sample_bool(ctx);
                if should_send {
                    let operation = format!("generated-{index}");
                    let (response, _receiver) = oneshot::channel();
                    let prompt = ApprovalPrompt {
                        session_id: "generated".to_owned(),
                        request: Request {
                            id: Uuid::new_v4(),
                            operation: operation.clone(),
                            detail: String::new(),
                            cwd: PathBuf::from("."),
                        },
                        response,
                    };
                    let result = sender.try_send(prompt);
                    let accepted = expected.len() < capacity;
                    assert_eq!(result.is_ok(), accepted);
                    if accepted {
                        expected.push_back(operation);
                    } else {
                        result.unwrap_err().into_inner().respond(false);
                    }
                } else {
                    let prompt = receiver.try_recv().expect("model expected queued approval");
                    assert_eq!(
                        prompt.request.operation,
                        expected.pop_front().expect("model queue was empty")
                    );
                    prompt.respond(false);
                }
                assert!(expected.len() <= capacity);
            }

            while let Some(operation) = expected.pop_front() {
                let prompt = receiver.try_recv().expect("queued approval disappeared");
                assert_eq!(prompt.request.operation, operation);
                prompt.respond(false);
            }
            assert!(matches!(
                receiver.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            Ok(())
        })
    }

    #[test]
    fn generated_shutdown_denies_all_pending_approvals() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        test_support::run(0x4150_5052_5348_5554, 64, |ctx| {
            let count = noprop::sample_usize_in(ctx, 1..=8);
            let nonce = noprop::sample_u64(ctx);
            runtime.block_on(async {
                let root = tempfile::tempdir().unwrap();
                let id = format!("approval-shutdown-{nonce:x}");
                let (sender, mut receiver) = approval_channel();
                let handle = spawn_runtime(root.path(), Some(&id), false, sender)
                    .await
                    .unwrap();
                let mut requests = Vec::with_capacity(count);
                for index in 0..count {
                    let id = id.clone();
                    let cwd = root.path().to_path_buf();
                    requests.push(tokio::spawn(async move {
                        request(&id, "generated", format!("request-{index}"), cwd).await
                    }));
                }

                for _ in 0..count {
                    tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                        .await
                        .expect("pending approval was not delivered")
                        .expect("approval channel closed unexpectedly");
                }

                handle.shutdown().await.unwrap();
                for request in requests {
                    let allowed = tokio::time::timeout(Duration::from_secs(1), request)
                        .await
                        .expect("pending approval did not resolve after shutdown")
                        .unwrap()
                        .unwrap();
                    assert!(!allowed, "shutdown allowed a pending approval");
                }
                assert!(!config::session_is_active(&id).await.unwrap());
            });
            Ok(())
        })
    }

    #[test]
    fn generated_runtime_permission_sequences_match_reference_model() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        test_support::run(0x5045_524d_5354_4154, 64, |ctx| {
            let nonce = noprop::sample_u64(ctx);
            let steps = (0..16)
                .map(|_| {
                    (
                        noprop::sample_usize_in(ctx, 0..=4),
                        noprop::sample_usize_in(ctx, 0..3),
                        noprop::sample_bool(ctx),
                    )
                })
                .collect::<Vec<_>>();

            runtime.block_on(async {
                let fixture = tempfile::tempdir().unwrap();
                let cwd = fixture.path().join("cwd");
                let extras = [
                    fixture.path().join("extra-a"),
                    fixture.path().join("extra-b"),
                    fixture.path().join("extra-c"),
                ];
                std::fs::create_dir(&cwd).unwrap();
                for extra in &extras {
                    std::fs::create_dir(extra).unwrap();
                }
                let cwd = config::canonical_directory(&cwd).unwrap();
                let extras = extras
                    .iter()
                    .map(|path| config::canonical_directory(path).unwrap())
                    .collect::<Vec<_>>();
                let id = format!("permission-pbt-{nonce:x}");
                let (sender, _receiver) = approval_channel();
                let handle = spawn_runtime(&cwd, Some(&id), false, sender).await.unwrap();
                let mut expected_yolo = false;
                let mut expected_roots = vec![cwd.clone()];

                for (operation, index, value) in steps {
                    match operation {
                        0 => {
                            handle.set_yolo(value).await.unwrap();
                            expected_yolo = value;
                        }
                        1 | 2 => {
                            let path = extras[index].clone();
                            handle.allow_directory(path.clone()).await.unwrap();
                            if !expected_roots.contains(&path) {
                                expected_roots.push(path);
                                expected_roots.sort();
                            }
                        }
                        3 => {
                            let path = extras[index].clone();
                            handle.revoke_directory(path.clone()).await.unwrap();
                            expected_roots.retain(|root| root != &path);
                        }
                        _ => {
                            assert!(handle.revoke_directory(cwd.clone()).await.is_err());
                        }
                    }

                    let snapshot = handle.snapshot().await.unwrap();
                    assert_eq!(snapshot.yolo, expected_yolo);
                    assert_eq!(snapshot.permitted_directories, expected_roots);
                }

                handle.shutdown().await.unwrap();
                assert!(!config::session_is_active(&id).await.unwrap());
            });
            Ok(())
        })
    }

    #[test]
    fn generated_permission_arguments_match_command_grammar() -> noprop::TestResult {
        test_support::run(0x5045_524d_4152_4701, test_support::DEFAULT_CASES, |ctx| {
            let action = if noprop::sample_bool(ctx) {
                "allow"
            } else {
                "revoke"
            };
            let prefix = if noprop::sample_bool(ctx) {
                "/permission"
            } else {
                "/permissions"
            };
            let value = format!(
                "/{}/{}",
                test_support::safe_component(ctx),
                test_support::safe_component(ctx)
            );
            let command = format!("{prefix} {action}   {value}   ");
            assert_eq!(permission_arg(&command, action), Some(value.as_str()));

            let invalid = match noprop::sample_usize_in(ctx, 0..=3) {
                0 => format!("{prefix} {action}"),
                1 => format!("{prefix} other {value}"),
                2 => format!("permission {action} {value}"),
                _ => format!("{prefix}{action} {value}"),
            };
            assert_eq!(
                permission_arg(&invalid, action),
                None,
                "accepted {invalid:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn environment_name_validation_matches_shell_identifier_grammar() -> noprop::TestResult {
        test_support::run(0x454e_564e_414d_4501, test_support::DEFAULT_CASES, |ctx| {
            let name = test_support::ascii_string(ctx, 96);
            let mut chars = name.chars();
            let expected = matches!(
                chars.next(),
                Some(first) if first == '_' || first.is_ascii_alphabetic()
            ) && chars
                .all(|character| character == '_' || character.is_ascii_alphanumeric());
            assert_eq!(
                valid_environment_name(&name),
                expected,
                "environment name mismatch for {name:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_service_account_tokens_are_fully_redacted() -> noprop::TestResult {
        test_support::run(0x5245_4441_4354_0001, test_support::DEFAULT_CASES, |ctx| {
            let token = format!(
                "tok-{}-{}",
                test_support::safe_component(ctx),
                noprop::sample_u64(ctx)
            );
            let prefix = test_support::safe_component(ctx);
            let suffix = test_support::safe_component(ctx);
            let input = format!("{prefix}{token}{suffix}{token}");
            let redacted = redact_token(&input, &token);
            assert!(
                !redacted.contains(&token),
                "token survived redaction: token={token:?}, output={redacted:?}"
            );
            assert_eq!(
                redacted.matches("[REDACTED_SERVICE_ACCOUNT_TOKEN]").count(),
                2
            );
            Ok(())
        })
    }
}
