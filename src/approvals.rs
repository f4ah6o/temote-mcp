use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
use crate::{kintone_cli, kintone_mcp, sandbox};

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
const MAX_ACTIVITY_TITLE_BYTES: usize = 512;
const MAX_ACTIVITY_DETAIL_BYTES: usize = 16 * 1024;
const MAX_APPROVAL_OPERATION_BYTES: usize = 256;
const MAX_APPROVAL_DETAIL_BYTES: usize = 64 * 1024;
const MAX_PENDING_APPROVAL_PROMPTS: usize = 128;
const MAX_PENDING_RUNTIME_COMMANDS: usize = 64;
const MAX_CONSOLE_PATH_BYTES: usize = 4096;

#[derive(Serialize, Deserialize)]
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
) -> Result<Value> {
    service_account_request(
        session_id,
        ServiceAccountRequest::Run {
            cwd,
            command,
            env_files,
            environment,
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

pub async fn request_supervisor_approval(
    sender: &ApprovalSender,
    operation: &str,
    detail: String,
) -> Result<bool> {
    let (response, receiver) = oneshot::channel();
    let request = Request {
        id: Uuid::new_v4(),
        operation: operation.to_owned(),
        detail,
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let prompt = ApprovalPrompt {
        session_id: "oauth".to_owned(),
        request,
        response,
    };
    sender.try_send(prompt).map_err(|error| {
        error.into_inner().respond(false);
        anyhow::anyhow!("local approval console is unavailable or busy")
    })?;
    Ok(receiver.await.unwrap_or(false))
}

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
    Shutdown,
}

pub struct RuntimeHandle {
    id: String,
    cwd: PathBuf,
    service_account_configured: bool,
    kintone_mcp_configured: bool,
    kintone_cli_configured: bool,
    commands: mpsc::Sender<RuntimeCommand>,
    join: JoinHandle<Result<()>>,
}

impl RuntimeHandle {
    pub fn session_id(&self) -> &str {
        &self.id
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    async fn set_yolo(&self, value: bool) -> Result<()> {
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

    async fn allow_directory(&self, path: PathBuf) -> Result<()> {
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

    async fn revoke_directory(&self, path: PathBuf) -> Result<()> {
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

    async fn snapshot(&self) -> Result<Session> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(RuntimeCommand::Snapshot { response })
            .await
            .map_err(|_| anyhow::anyhow!("session {} runtime stopped", self.id))?;
        receiver
            .await
            .context("session runtime stopped before snapshot")
    }

    pub async fn shutdown(self) -> Result<()> {
        let _ = self.commands.send(RuntimeCommand::Shutdown).await;
        self.join
            .await
            .context("session runtime task failed to join")??;
        Ok(())
    }
}

pub async fn spawn_runtime(
    cwd: &Path,
    session_id: Option<&str>,
    yolo: bool,
    approval_sender: ApprovalSender,
) -> Result<RuntimeHandle> {
    let service_account_token = std::env::var("OP_SERVICE_ACCOUNT_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let kintone_bridge = Arc::new(tokio::sync::Mutex::new(kintone_mcp::Bridge::capture()));
    let kintone_cli_bridge = Arc::new(kintone_cli::Bridge::capture());
    let service_account_configured = service_account_token.is_some();
    let kintone_mcp_configured = kintone_bridge.lock().await.configured();
    let kintone_cli_configured = kintone_cli_bridge.configured();
    let id = config::session_id(session_id)?;
    config::remove_inactive_socket(&id).await?;
    let mut session = config::new_session(cwd, Some(&id), yolo)?;
    let path = config::socket_path(&session.id)?;
    let state_dir = path.parent().context("session socket has no parent")?;
    tokio::fs::create_dir_all(state_dir).await?;
    tokio::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).await?;

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to listen at {}", path.display()))?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    session.process_id = std::process::id();
    if let Err(error) = config::save_session(&session).await {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }

    let id_for_handle = session.id.clone();
    let cwd_for_handle = session.cwd.clone();
    let (commands, command_receiver) = mpsc::channel(MAX_PENDING_RUNTIME_COMMANDS);
    let join = tokio::spawn(async move {
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
        session.process_id = 0;
        if let Err(error) = config::save_session(&session).await {
            eprintln!("failed to mark session {} stopped: {error:#}", session.id);
        }
        let _ = tokio::fs::remove_file(&path).await;
        result
    });

    Ok(RuntimeHandle {
        id: id_for_handle,
        cwd: cwd_for_handle,
        service_account_configured,
        kintone_mcp_configured,
        kintone_cli_configured,
        commands,
        join,
    })
}

pub async fn start(session_id: Option<&str>, yolo: bool) -> Result<()> {
    let (approval_sender, approval_receiver) = approval_channel();
    let handle =
        spawn_runtime(&std::env::current_dir()?, session_id, yolo, approval_sender).await?;
    eprintln!(
        "temote-mcp session: {}\ncwd: {}\nmode: {}\n\
         Give this session ID to the agent so it can include it in temote-mcp tool calls.\n\
         Commands: /permission ask|yolo|allow <directory>|revoke <directory>|list|status\n\
         Press Ctrl-C to stop.",
        handle.session_id(),
        handle.cwd().display(),
        if yolo { "yolo" } else { "ask" }
    );
    if yolo {
        eprintln!(
            "WARNING: YOLO mode grants MCP tools this user's full filesystem, process, environment, and network permissions without local approval."
        );
    }
    eprintln!(
        "1Password service account: {}",
        if handle.service_account_configured {
            "configured (token kept only by this session process)"
        } else {
            "not configured"
        }
    );
    eprintln!(
        "kintone MCP: {}",
        if handle.kintone_mcp_configured {
            "configured (credentials kept only by this session process)"
        } else {
            "not configured"
        }
    );
    eprintln!(
        "cli-kintone: {}",
        if handle.kintone_cli_configured {
            "configured (credentials kept only by this session process)"
        } else {
            "not configured"
        }
    );
    run_cli_console(&handle, approval_receiver).await?;
    handle.shutdown().await
}

pub async fn run_supervisor_console(mut approvals: ApprovalReceiver) -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut pending = VecDeque::<ApprovalPrompt>::new();
    loop {
        tokio::select! {
            prompt = approvals.recv() => {
                let Some(prompt) = prompt else { return Ok(()) };
                let show_now = pending.is_empty();
                pending.push_back(prompt);
                if show_now {
                    show_supervisor_prompt(pending.front().unwrap())?;
                }
            }
            line = input.next_line() => {
                let Some(line) = line? else {
                    deny_all(&mut pending);
                    return Ok(());
                };
                handle_supervisor_input(line.trim(), &mut pending)?;
            }
        }
    }
}

async fn run_cli_console(handle: &RuntimeHandle, mut approvals: ApprovalReceiver) -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut pending = VecDeque::<ApprovalPrompt>::new();
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        tokio::select! {
            prompt = approvals.recv() => {
                let Some(prompt) = prompt else { return Ok(()) };
                let show_now = pending.is_empty();
                pending.push_back(prompt);
                if show_now {
                    show_supervisor_prompt(pending.front().unwrap())?;
                }
            }
            line = input.next_line() => {
                let Some(line) = line? else {
                    deny_all(&mut pending);
                    return Ok(());
                };
                handle_cli_input(line.trim(), handle, &mut pending).await?;
            }
            signal = &mut ctrl_c => {
                signal.context("failed to receive Ctrl-C")?;
                eprintln!("Stopping temote-mcp session {}", handle.session_id());
                deny_all(&mut pending);
                return Ok(());
            }
        }
    }
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
                match message {
                    Message::Probe => {
                        stream.write_all(b"active\n").await?;
                    }
                    Message::Activity { title, detail } => {
                        show_activity_for_session(&session.id, &title, detail.as_deref());
                    }
                    Message::OnePasswordServiceAccount { request } => {
                        let session = session.clone();
                        let token = service_account_token.map(str::to_owned);
                        tokio::spawn(async move {
                            let response = handle_service_account_request(&session, token.as_deref(), request).await;
                            let bytes = encode_session_result(response, "1Password service-account response");
                            let _ = stream.write_all(&bytes).await;
                            let _ = stream.shutdown().await;
                        });
                    }
                    Message::KintoneMcp { request } => {
                        let session = session.clone();
                        let bridge = Arc::clone(&kintone_bridge);
                        tokio::spawn(async move {
                            let response = handle_kintone_mcp_request(&session, bridge, request).await;
                            let bytes = encode_session_result(response, "kintone MCP response");
                            let _ = stream.write_all(&bytes).await;
                            let _ = stream.shutdown().await;
                        });
                    }
                    Message::KintoneCli { request } => {
                        let session = session.clone();
                        let bridge = Arc::clone(&kintone_cli_bridge);
                        tokio::spawn(async move {
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
                        stream.write_all(b"allow\n").await?;
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
                        tokio::spawn(async move {
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
                let Some(command) = command else { return Ok(()) };
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
                    RuntimeCommand::Shutdown => {
                        let _ = approval_lifetime.send(true);
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn handle_cli_input(
    input: &str,
    handle: &RuntimeHandle,
    pending: &mut VecDeque<ApprovalPrompt>,
) -> Result<()> {
    match input {
        "/permissions yolo" | "/permission yolo" => {
            handle.set_yolo(true).await?;
            eprintln!("Permissions: yolo (full host permissions; no local approvals)");
            while let Some(prompt) = pending.pop_front() {
                eprintln!(
                    "[session {}] [yolo] allowing {}: {}",
                    prompt.session_id, prompt.request.operation, prompt.request.detail
                );
                prompt.respond(true);
            }
        }
        "/permissions ask" | "/permission ask" => {
            handle.set_yolo(false).await?;
            eprintln!("Permissions: ask");
        }
        "y" | "Y" | "yes" | "YES" if !pending.is_empty() => {
            pending.pop_front().unwrap().respond(true);
            show_next_prompt(pending)?;
        }
        "n" | "N" | "no" | "NO" if !pending.is_empty() => {
            pending.pop_front().unwrap().respond(false);
            show_next_prompt(pending)?;
        }
        "/permission list" | "/permissions list" => {
            show_permissions(&handle.snapshot().await?);
        }
        "/permission status" | "/permissions status" => {
            let session = handle.snapshot().await?;
            eprintln!("Permissions: {}", if session.yolo { "yolo" } else { "ask" });
            show_permissions(&session);
        }
        command if permission_arg(command, "allow").is_some() => {
            let directory = PathBuf::from(permission_arg(command, "allow").unwrap());
            handle.allow_directory(directory.clone()).await?;
            eprintln!(
                "Allowed sandbox root: {}",
                config::canonical_directory(&directory)?.display()
            );
        }
        command if permission_arg(command, "revoke").is_some() => {
            let directory = PathBuf::from(permission_arg(command, "revoke").unwrap());
            let canonical = config::canonical_directory(&directory)?;
            handle.revoke_directory(directory).await?;
            eprintln!("Revoked sandbox root: {}", canonical.display());
        }
        "/permission" | "/permissions" | "/permission help" | "/permissions help" => {
            eprintln!("/permission ask|yolo|allow <directory>|revoke <directory>|list|status");
        }
        "" => {}
        command if !pending.is_empty() => {
            pending.pop_front().unwrap().respond(false);
            eprintln!("Denied request (unrecognized response: {command})");
            show_next_prompt(pending)?;
        }
        command => eprintln!("Unknown command: {command}"),
    }
    Ok(())
}

fn handle_supervisor_input(input: &str, pending: &mut VecDeque<ApprovalPrompt>) -> Result<()> {
    match input {
        "y" | "Y" | "yes" | "YES" if !pending.is_empty() => {
            pending.pop_front().unwrap().respond(true);
            show_next_prompt(pending)?;
        }
        "n" | "N" | "no" | "NO" if !pending.is_empty() => {
            pending.pop_front().unwrap().respond(false);
            show_next_prompt(pending)?;
        }
        "" => {}
        command if !pending.is_empty() => {
            pending.pop_front().unwrap().respond(false);
            eprintln!("Denied request (unrecognized response: {command})");
            show_next_prompt(pending)?;
        }
        command => eprintln!(
            "Unknown supervisor command: {command}. Managed-session approvals use y/yes or n/no."
        ),
    }
    Ok(())
}

fn deny_all(pending: &mut VecDeque<ApprovalPrompt>) {
    while let Some(prompt) = pending.pop_front() {
        prompt.respond(false);
    }
}

fn show_supervisor_prompt(prompt: &ApprovalPrompt) -> Result<()> {
    eprintln!(
        "\n[session {}] approval {}\ncwd: {}\noperation: {}\n{}",
        prompt.session_id,
        prompt.request.id,
        bounded_console_path(&prompt.request.cwd),
        bounded_console_text(&prompt.request.operation, MAX_APPROVAL_OPERATION_BYTES),
        bounded_console_text(&prompt.request.detail, MAX_APPROVAL_DETAIL_BYTES),
    );
    eprint!("Allow operation? [y/N] ");
    std::io::stderr().flush()?;
    Ok(())
}

fn show_next_prompt(pending: &VecDeque<ApprovalPrompt>) -> Result<()> {
    if let Some(prompt) = pending.front() {
        show_supervisor_prompt(prompt)?;
    }
    Ok(())
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
        } => service_account_run(session, token, cwd, command, env_files, environment).await,
    }
}

async fn service_account_status(session: &Session, token: &str) -> Result<Value> {
    let command = vec![
        "op".to_owned(),
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

async fn service_account_run(
    session: &Session,
    token: &str,
    cwd: PathBuf,
    command: Vec<String>,
    env_files: Vec<PathBuf>,
    environment_refs: BTreeMap<String, String>,
) -> Result<Value> {
    validate_service_account_run_input(&command, &env_files, &environment_refs)?;
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
    let mut op_command = vec!["op".to_owned(), "run".to_owned()];
    for path in &env_files {
        op_command.push(format!("--env-file={}", path.display()));
    }
    op_command.push("--".to_owned());
    op_command.push("/usr/bin/env".to_owned());
    op_command.push("-u".to_owned());
    op_command.push("OP_SERVICE_ACCOUNT_TOKEN".to_owned());
    op_command.extend(command);

    let mut environment = environment_refs.into_iter().collect::<HashMap<_, _>>();
    environment.insert("OP_SERVICE_ACCOUNT_TOKEN".to_owned(), token.to_owned());
    let output = sandbox::run_unrestricted_with_env(
        &op_command,
        &cwd,
        None,
        &environment,
        &["OP_SERVICE_ACCOUNT_TOKEN"],
    )
    .await
    .context("failed to run command through 1Password service account")?;
    let stdout = redact_token(&output.stdout, token);
    let stderr = redact_token(&output.stderr, token);
    Ok(json!({
        "exit_code": output.status,
        "stdout": stdout,
        "stderr": stderr,
        "truncated": output.truncated,
    }))
}

pub(crate) fn validate_service_account_run_input(
    command: &[String],
    env_files: &[PathBuf],
    environment_refs: &BTreeMap<String, String>,
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
            reference.len() <= MAX_SERVICE_ACCOUNT_ENV_REF_BYTES,
            "1Password secret reference exceeds {MAX_SERVICE_ACCOUNT_ENV_REF_BYTES} bytes"
        );
        anyhow::ensure!(
            !reference.contains('\0') && reference.starts_with("op://"),
            "1Password environment values must be op:// secret references"
        );
    }
    Ok(())
}

fn valid_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn redact_token(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_owned()
    } else {
        text.replace(token, "[REDACTED_SERVICE_ACCOUNT_TOKEN]")
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

fn show_permissions(session: &Session) {
    eprintln!("Sandbox roots:");
    for path in &session.permitted_directories {
        eprintln!("  {}", bounded_console_path(path));
    }
}

#[cfg(test)]
mod tests {
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
                validate_service_account_run_input(&command, &env_files, &environment_refs);
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
