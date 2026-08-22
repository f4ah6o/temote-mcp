use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::{self, Session};
use crate::{kintone_cli, kintone_mcp, sandbox};

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
    let path = config::socket_path(session_id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `temote-mcp start`"))?;
    stream
        .write_all(&serde_json::to_vec(&Message::Approval { request })?)
        .await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    match response.trim() {
        "allow" => Ok(true),
        "deny" => Ok(false),
        value => anyhow::bail!("invalid response from session: {value:?}"),
    }
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
    let path = config::socket_path(session_id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `temote-mcp start`"))?;
    stream
        .write_all(&serde_json::to_vec(&Message::KintoneCli { request })?)
        .await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
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
    let path = config::socket_path(session_id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `temote-mcp start`"))?;
    stream
        .write_all(&serde_json::to_vec(&Message::KintoneMcp { request })?)
        .await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
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
    let path = config::socket_path(session_id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `temote-mcp start`"))?;
    stream
        .write_all(&serde_json::to_vec(&Message::OnePasswordServiceAccount {
            request,
        })?)
        .await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
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
    let Ok(bytes) = serde_json::to_vec(&message) else {
        return;
    };
    let _ = stream.write_all(&bytes).await;
    let _ = stream.write_all(b"\n").await;
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

pub type ApprovalReceiver = mpsc::UnboundedReceiver<ApprovalPrompt>;
pub type ApprovalSender = mpsc::UnboundedSender<ApprovalPrompt>;

pub fn approval_channel() -> (ApprovalSender, ApprovalReceiver) {
    mpsc::unbounded_channel()
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
    commands: mpsc::UnboundedSender<RuntimeCommand>,
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
            .map_err(|_| anyhow::anyhow!("session {} runtime stopped", self.id))?;
        receiver
            .await
            .context("session runtime stopped before snapshot")
    }

    pub async fn shutdown(self) -> Result<()> {
        let _ = self.commands.send(RuntimeCommand::Shutdown);
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
    let (commands, command_receiver) = mpsc::unbounded_channel();
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

async fn run_runtime(
    listener: UnixListener,
    session: &mut Session,
    mut commands: mpsc::UnboundedReceiver<RuntimeCommand>,
    approval_sender: ApprovalSender,
    service_account_token: Option<&str>,
    kintone_bridge: Arc<tokio::sync::Mutex<kintone_mcp::Bridge>>,
    kintone_cli_bridge: Arc<kintone_cli::Bridge>,
) -> Result<()> {
    let (approval_lifetime, _) = watch::channel(false);
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let (mut stream, _) = connection?;
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).await?;
                let message: Message = serde_json::from_str(&line).context("invalid session message")?;
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
                            let response = match response {
                                Ok(result) => ServiceAccountResponse { result: Some(result), error: None },
                                Err(error) => ServiceAccountResponse { result: None, error: Some(format!("{error:#}")) },
                            };
                            if let Ok(bytes) = serde_json::to_vec(&response) {
                                let _ = stream.write_all(&bytes).await;
                                let _ = stream.write_all(b"\n").await;
                                let _ = stream.shutdown().await;
                            }
                        });
                    }
                    Message::KintoneMcp { request } => {
                        let session = session.clone();
                        let bridge = Arc::clone(&kintone_bridge);
                        tokio::spawn(async move {
                            let response = handle_kintone_mcp_request(&session, bridge, request).await;
                            let response = match response {
                                Ok(result) => KintoneMcpResponse { result: Some(result), error: None },
                                Err(error) => KintoneMcpResponse { result: None, error: Some(format!("{error:#}")) },
                            };
                            if let Ok(bytes) = serde_json::to_vec(&response) {
                                let _ = stream.write_all(&bytes).await;
                                let _ = stream.write_all(b"\n").await;
                                let _ = stream.shutdown().await;
                            }
                        });
                    }
                    Message::KintoneCli { request } => {
                        let session = session.clone();
                        let bridge = Arc::clone(&kintone_cli_bridge);
                        tokio::spawn(async move {
                            let response = handle_kintone_cli_request(&session, bridge, request).await;
                            let response = match response {
                                Ok(result) => KintoneCliResponse { result: Some(result), error: None },
                                Err(error) => KintoneCliResponse { result: None, error: Some(format!("{error:#}")) },
                            };
                            if let Ok(bytes) = serde_json::to_vec(&response) {
                                let _ = stream.write_all(&bytes).await;
                                let _ = stream.write_all(b"\n").await;
                                let _ = stream.shutdown().await;
                            }
                        });
                    }
                    Message::Approval { request } if session.yolo => {
                        eprintln!("[session {}] [yolo] allowing {}: {}", session.id, request.operation, request.detail);
                        stream.write_all(b"allow\n").await?;
                    }
                    Message::Approval { request } => {
                        let (response, receiver) = oneshot::channel();
                        let prompt = ApprovalPrompt {
                            session_id: session.id.clone(),
                            request,
                            response,
                        };
                        if let Err(error) = approval_sender.send(prompt) {
                            error.0.respond(false);
                            continue;
                        }
                        let mut runtime_alive = approval_lifetime.subscribe();
                        tokio::spawn(async move {
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
        prompt.request.cwd.display(),
        prompt.request.operation,
        prompt.request.detail
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
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
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
    for (name, reference) in &environment_refs {
        anyhow::ensure!(
            valid_environment_name(name),
            "invalid environment variable name: {name}"
        );
        anyhow::ensure!(
            !name.starts_with("OP_"),
            "environment variables beginning with OP_ are reserved for 1Password CLI"
        );
        anyhow::ensure!(
            reference.starts_with("op://"),
            "1Password environment values must be op:// secret references"
        );
    }

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

fn show_activity_for_session(session_id: &str, title: &str, detail: Option<&str>) {
    eprintln!("\n[session {session_id}] • {title}");
    if let Some(detail) = detail.filter(|value| !value.is_empty()) {
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
        eprintln!("  {}", path.display());
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
