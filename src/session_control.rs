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
use tokio::sync::mpsc;

use crate::approvals::{
    self, ApprovalPrompt, ApprovalReceiver, ApprovalSender, CapturedStartEnvironment, Request,
};
use crate::config::{self, LifecycleStatus, SessionLifecycle};
use crate::host_identity;
use crate::named_roots::NamedRoots;
use crate::supervisor::SessionSupervisor;

const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;
const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONSOLE_QUEUE: usize = 1;
const CONTROL_PROTOCOL_VERSION: u64 = 1;
const RESTART_NOT_RESUMED_AFTER_SUPERVISOR_RESTART: &str = "automatic restart was not resumed after supervisor restart because captured start credentials are intentionally memory-only; use `temote-mcp session restart <id>`";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum ControlRequest {
    Ping,
    Approval {
        session_id: String,
        request: Request,
    },
    Start {
        path: String,
        session_id: String,
        #[serde(default)]
        environment: CapturedStartEnvironment,
        #[serde(default)]
        public: bool,
    },
    StartLocal {
        cwd: PathBuf,
        session_id: Option<String>,
        yolo: bool,
        #[serde(default)]
        environment: CapturedStartEnvironment,
    },
    List,
    Info {
        session_id: String,
    },
    Stop {
        session_id: String,
        #[serde(default)]
        public: bool,
    },
    Restart {
        session_id: String,
        #[serde(default)]
        environment: CapturedStartEnvironment,
    },
    RestartPolicy {
        session_id: String,
        policy: String,
    },
    PermissionStatus {
        session_id: String,
    },
    PermissionMode {
        session_id: String,
        yolo: bool,
    },
    PermissionAllow {
        session_id: String,
        path: PathBuf,
    },
    PermissionRevoke {
        session_id: String,
        path: PathBuf,
    },
    AttachConsole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub host_id: String,
    pub id: String,
    pub session_id: String,
    pub status: String,
    pub pid: Option<u32>,
    pub process_id: u32,
    pub cwd: PathBuf,
    pub permitted_directories: Vec<PathBuf>,
    pub started_at: u64,
    pub stopped_at: Option<u64>,
    pub exit_reason: Option<String>,
    pub last_error: Option<String>,
    pub permission_mode: String,
    pub yolo: bool,
    pub logical_path: Option<String>,
    pub restart_policy: String,
    pub restart_count: u32,
    pub last_restart_at: Option<u64>,
    pub next_restart_at: Option<u64>,
    pub restart_limit_reason: Option<String>,
}

#[derive(Clone)]
pub enum SessionBackend {
    #[cfg(test)]
    InProcess(Arc<SessionSupervisor>),
    LocalControl,
}

impl SessionBackend {
    #[cfg(test)]
    pub fn in_process(supervisor: Arc<SessionSupervisor>) -> Self {
        Self::InProcess(supervisor)
    }

    pub async fn local_control() -> Result<Self> {
        let status = request(ControlRequest::Ping).await?;
        anyhow::ensure!(
            status.get("status").and_then(Value::as_str) == Some("active"),
            "Temote session supervisor did not report active status"
        );
        anyhow::ensure!(
            status.get("control_protocol").and_then(Value::as_u64)
                == Some(CONTROL_PROTOCOL_VERSION),
            "Temote session supervisor control protocol is incompatible; upgrade/restart the lifecycle supervisor before serve/up"
        );
        Ok(Self::LocalControl)
    }

    pub async fn roots_configured(&self) -> Result<bool> {
        match self {
            #[cfg(test)]
            Self::InProcess(supervisor) => Ok(supervisor.roots_configured()),
            Self::LocalControl => Ok(request(ControlRequest::Ping)
                .await?
                .get("roots_configured")
                .and_then(Value::as_bool)
                .unwrap_or(false)),
        }
    }

    pub async fn start(&self, path: &str, session_id: Option<&str>) -> Result<Value> {
        match self {
            #[cfg(test)]
            Self::InProcess(supervisor) => Ok(serde_json::to_value(
                supervisor
                    .start_public_with_environment(
                        path,
                        session_id,
                        CapturedStartEnvironment::default(),
                    )
                    .await?,
            )?),
            Self::LocalControl => {
                let session_id = config::session_id(session_id)?;
                let result = request(ControlRequest::Start {
                    path: path.to_owned(),
                    session_id,
                    environment: CapturedStartEnvironment::default(),
                    public: true,
                })
                .await?;
                Ok(json!({
                    "session_id": result.get("session_id").cloned().unwrap_or(Value::Null),
                    "cwd": result.get("cwd").cloned().unwrap_or(Value::Null),
                    "status": result.get("status").cloned().unwrap_or(Value::Null),
                    "yolo": result.get("yolo").cloned().unwrap_or(Value::Bool(false)),
                }))
            }
        }
    }

    pub async fn stop(&self, session_id: &str) -> Result<()> {
        match self {
            #[cfg(test)]
            Self::InProcess(supervisor) => supervisor.stop_public(session_id).await,
            Self::LocalControl => {
                request(ControlRequest::Stop {
                    session_id: session_id.to_owned(),
                    public: true,
                })
                .await?;
                Ok(())
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ControlResponse {
    ok: bool,
    result: Option<Value>,
    error: Option<String>,
}

pub async fn run_supervisor() -> Result<()> {
    let path = config::supervisor_socket_path()?;
    prepare_supervisor_socket(&path).await?;
    let parent = path.parent().context("supervisor socket has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;

    reconcile_stale_sessions().await?;

    let roots = NamedRoots::from_env()?;
    let (supervisor, approvals) = SessionSupervisor::new(roots);
    let (console_registration, console_registrations) = mpsc::channel(8);
    let approval_broker = tokio::spawn(run_approval_broker(approvals, console_registrations));

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to listen at {}", path.display()))?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;

    eprintln!("Temote session supervisor: {}", path.display());
    eprintln!("Use `temote-mcp session console` to attach the approval console.");
    eprintln!("Press Ctrl-C to stop the supervisor and gracefully stop owned sessions.");

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut maintenance = tokio::time::interval(Duration::from_millis(250));
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let serve_result: Result<()> = loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let supervisor = Arc::clone(&supervisor);
                        let registration = console_registration.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle_control_connection(stream, supervisor, registration).await {
                                eprintln!("session supervisor client error: {error:#}");
                            }
                        });
                    }
                    Err(error) => break Err(error).context("session supervisor listener failed"),
                }
            }
            _ = maintenance.tick() => {
                supervisor.reap_finished().await;
            }
            signal = &mut ctrl_c => {
                match signal {
                    Ok(()) => break Ok(()),
                    Err(error) => break Err(error).context("failed to receive Ctrl-C"),
                }
            }
        }
    };

    let shutdown = supervisor.shutdown().await;
    approval_broker.abort();
    let _ = approval_broker.await;
    if let Err(error) = tokio::fs::remove_file(&path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "failed to remove supervisor socket {}: {error}",
            path.display()
        );
    }
    serve_result?;
    shutdown
}

pub async fn start_named(session_id: String, path: String) -> Result<()> {
    let result = request(ControlRequest::Start {
        path,
        session_id,
        environment: CapturedStartEnvironment::capture(),
        public: false,
    })
    .await?;
    print_json(&result)
}

pub async fn start_legacy(session_id: Option<String>, yolo: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    let result = request(ControlRequest::StartLocal {
        cwd,
        session_id,
        yolo,
        environment: CapturedStartEnvironment::capture(),
    })
    .await?;
    print_json(&result)
}

pub async fn list() -> Result<()> {
    let result = request(ControlRequest::List).await?;
    let sessions: Vec<SessionView> = serde_json::from_value(result)?;
    println!("SESSION\tSTATUS\tPID\tCWD");
    for session in sessions {
        let pid = session
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "{}\t{}\t{}\t{}",
            session.session_id,
            session.status,
            pid,
            session.cwd.display()
        );
    }
    Ok(())
}

pub async fn info(session_id: String) -> Result<()> {
    let result = request(ControlRequest::Info { session_id }).await?;
    print_json(&result)
}

pub async fn stop(session_id: String) -> Result<()> {
    let result = request(ControlRequest::Stop {
        session_id,
        public: false,
    })
    .await?;
    print_json(&result)
}

pub async fn restart(session_id: String) -> Result<()> {
    let result = request(ControlRequest::Restart {
        session_id,
        environment: CapturedStartEnvironment::capture(),
    })
    .await?;
    print_json(&result)
}

pub async fn restart_policy(session_id: String, policy: String) -> Result<()> {
    let result = request(ControlRequest::RestartPolicy { session_id, policy }).await?;
    print_json(&result)
}

pub async fn permission(
    session_id: String,
    command: crate::cli::SessionPermissionCommand,
) -> Result<()> {
    let control_request = match command {
        crate::cli::SessionPermissionCommand::Status => {
            ControlRequest::PermissionStatus { session_id }
        }
        crate::cli::SessionPermissionCommand::Ask => ControlRequest::PermissionMode {
            session_id,
            yolo: false,
        },
        crate::cli::SessionPermissionCommand::Yolo => ControlRequest::PermissionMode {
            session_id,
            yolo: true,
        },
        crate::cli::SessionPermissionCommand::Allow { path } => {
            ControlRequest::PermissionAllow { session_id, path }
        }
        crate::cli::SessionPermissionCommand::Revoke { path } => {
            ControlRequest::PermissionRevoke { session_id, path }
        }
    };
    let result = request(control_request).await?;
    print_json(&result)
}

pub fn approval_proxy_sender() -> ApprovalSender {
    let (sender, mut receiver) = approvals::approval_channel();
    tokio::spawn(async move {
        while let Some(prompt) = receiver.recv().await {
            let control_request = ControlRequest::Approval {
                session_id: prompt.session_id.clone(),
                request: prompt.request.clone(),
            };
            let allowed = request(control_request)
                .await
                .ok()
                .and_then(|value| value.get("allow").and_then(Value::as_bool))
                .unwrap_or(false);
            prompt.respond(allowed);
        }
    });
    sender
}

pub async fn run_console() -> Result<()> {
    let stream = connect_supervisor().await?;
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(&encode_line(&ControlRequest::AttachConsole)?)
        .await?;
    let mut reader = BufReader::new(reader);
    let response = read_line_limited(&mut reader, "supervisor attach response").await?;
    let response: ControlResponse = serde_json::from_str(response.trim())?;
    ensure_response_ok(response)?;

    eprintln!(
        "Attached to Temote approval console. Ctrl-C or stdin EOF detaches only the console."
    );
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        tokio::select! {
            line = read_line_limited(&mut reader, "approval prompt") => {
                let line = line?;
                if line.is_empty() {
                    return Ok(());
                }
                let prompt: Value = serde_json::from_str(line.trim()).context("invalid approval prompt")?;
                anyhow::ensure!(prompt["type"] == "approval", "unexpected console event");
                eprintln!(
                    "\n[session {}] approval {}\ncwd: {}\noperation: {}\n{}",
                    prompt["session_id"].as_str().unwrap_or("?"),
                    prompt["id"].as_str().unwrap_or("?"),
                    prompt["cwd"].as_str().unwrap_or("?"),
                    prompt["operation"].as_str().unwrap_or("?"),
                    prompt["detail"].as_str().unwrap_or("")
                );
                eprint!("Allow operation? [y/N] ");
                std::io::stderr().flush()?;
                let Some(answer) = input.next_line().await? else {
                    return Ok(());
                };
                let allowed = matches!(answer.trim(), "y" | "Y" | "yes" | "YES");
                writer
                    .write_all(&encode_line(&json!({"allow": allowed}))?)
                    .await?;
            }
            line = input.next_line() => {
                match line? {
                    None => return Ok(()),
                    Some(line) if line.trim().is_empty() => {},
                    Some(_) => eprintln!("No approval is pending."),
                }
            }
            signal = &mut ctrl_c => {
                signal.context("failed to receive Ctrl-C")?;
                return Ok(());
            }
        }
    }
}

async fn handle_control_connection(
    mut stream: UnixStream,
    supervisor: Arc<SessionSupervisor>,
    console_registration: mpsc::Sender<mpsc::Sender<ApprovalPrompt>>,
) -> Result<()> {
    let line = tokio::time::timeout(
        CONTROL_READ_TIMEOUT,
        read_stream_line(&mut stream, "supervisor control request"),
    )
    .await
    .context("timed out waiting for supervisor control request")??;
    let request: ControlRequest =
        serde_json::from_str(line.trim()).context("invalid control request")?;

    if matches!(request, ControlRequest::AttachConsole) {
        return handle_console_attachment(stream, console_registration).await;
    }

    let result = dispatch_request(request, &supervisor).await;
    let response = match result {
        Ok(result) => json!({"ok": true, "result": result, "error": Value::Null}),
        Err(error) => json!({"ok": false, "result": Value::Null, "error": format!("{error:#}")}),
    };
    stream.write_all(&encode_line(&response)?).await?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn dispatch_request(
    request: ControlRequest,
    supervisor: &Arc<SessionSupervisor>,
) -> Result<Value> {
    supervisor.reap_finished().await;
    match request {
        ControlRequest::Ping => Ok(json!({
            "status": "active",
            "host_id": host_identity::resolve()?,
            "control_protocol": CONTROL_PROTOCOL_VERSION,
            "roots_configured": supervisor.roots_configured(),
        })),
        ControlRequest::Approval {
            session_id,
            request,
        } => {
            let allowed =
                approvals::request_approval(&supervisor.approval_sender(), session_id, request)
                    .await?;
            Ok(json!({"allow": allowed}))
        }
        ControlRequest::Start {
            path,
            session_id,
            environment,
            public,
        } => {
            environment.validate()?;
            if public {
                supervisor
                    .start_public_with_environment(&path, Some(&session_id), environment)
                    .await?;
            } else {
                supervisor
                    .start_with_environment(&path, Some(&session_id), environment)
                    .await?;
            }
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::StartLocal {
            cwd,
            session_id,
            yolo,
            environment,
        } => {
            environment.validate()?;
            let info = supervisor
                .start_local_with_environment(&cwd, session_id.as_deref(), yolo, environment)
                .await?;
            Ok(serde_json::to_value(
                inspect_session(&info.session_id).await?,
            )?)
        }
        ControlRequest::List => Ok(serde_json::to_value(list_session_views().await?)?),
        ControlRequest::Info { session_id } => {
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::Stop { session_id, public } => {
            if public {
                supervisor.stop_public(&session_id).await?;
            } else {
                supervisor.stop(&session_id).await?;
            }
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::Restart {
            session_id,
            environment,
        } => {
            environment.validate()?;
            restart_session(supervisor, &session_id, environment).await?;
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::RestartPolicy { session_id, policy } => {
            supervisor.set_restart_policy(&session_id, &policy).await?;
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::PermissionStatus { session_id } => {
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::PermissionMode { session_id, yolo } => {
            supervisor.set_permission_yolo(&session_id, yolo).await?;
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::PermissionAllow { session_id, path } => {
            supervisor.allow_directory(&session_id, path).await?;
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::PermissionRevoke { session_id, path } => {
            supervisor.revoke_directory(&session_id, path).await?;
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::AttachConsole => unreachable!("handled before dispatch"),
    }
}

async fn restart_session(
    supervisor: &Arc<SessionSupervisor>,
    session_id: &str,
    environment: CapturedStartEnvironment,
) -> Result<()> {
    config::validate_session_id(session_id)?;
    supervisor.reap_finished().await;
    let session = config::read_session_metadata(session_id).await?;
    let lifecycle = config::read_session_lifecycle(session_id).await?;
    if config::session_is_active(session_id).await? {
        supervisor.stop(session_id).await?;
    }
    if let Some(path) = lifecycle
        .as_ref()
        .and_then(|state| state.logical_path.as_deref())
    {
        supervisor
            .start_with_environment(path, Some(session_id), environment)
            .await?;
    } else {
        supervisor
            .start_local_with_environment(&session.cwd, Some(session_id), session.yolo, environment)
            .await?;
    }
    Ok(())
}

async fn handle_console_attachment(
    stream: UnixStream,
    console_registration: mpsc::Sender<mpsc::Sender<ApprovalPrompt>>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(&encode_line(&json!({
            "ok": true,
            "result": {"status": "attached"},
            "error": Value::Null
        }))?)
        .await?;
    let (sender, mut receiver) = mpsc::channel(MAX_CONSOLE_QUEUE);
    console_registration
        .send(sender)
        .await
        .context("approval broker is unavailable")?;
    let mut reader = BufReader::new(reader);

    while let Some(prompt) = receiver.recv().await {
        let event = json!({
            "type": "approval",
            "session_id": prompt.session_id,
            "id": prompt.request.id,
            "cwd": prompt.request.cwd,
            "operation": prompt.request.operation,
            "detail": prompt.request.detail,
        });
        if let Err(error) = writer.write_all(&encode_line(&event)?).await {
            prompt.respond(false);
            return Err(error).context("approval console disconnected while writing prompt");
        }
        let line = match read_line_limited(&mut reader, "approval response").await {
            Ok(line) if !line.is_empty() => line,
            Ok(_) => {
                prompt.respond(false);
                return Ok(());
            }
            Err(error) => {
                prompt.respond(false);
                return Err(error);
            }
        };
        let allowed = serde_json::from_str::<Value>(line.trim())
            .ok()
            .and_then(|value| value.get("allow").and_then(Value::as_bool))
            .unwrap_or(false);
        prompt.respond(allowed);
    }
    Ok(())
}

async fn run_approval_broker(
    mut approvals: ApprovalReceiver,
    mut registrations: mpsc::Receiver<mpsc::Sender<ApprovalPrompt>>,
) {
    let mut console: Option<mpsc::Sender<ApprovalPrompt>> = None;
    loop {
        tokio::select! {
            registration = registrations.recv() => {
                let Some(registration) = registration else {
                    while let Some(prompt) = approvals.recv().await {
                        prompt.respond(false);
                    }
                    return;
                };
                console = Some(registration);
            }
            prompt = approvals.recv() => {
                let Some(prompt) = prompt else { return };
                let Some(sender) = console.as_ref() else {
                    prompt.respond(false);
                    continue;
                };
                if let Err(error) = sender.try_send(prompt) {
                    error.into_inner().respond(false);
                    console = None;
                }
            }
        }
    }
}

async fn request(request: ControlRequest) -> Result<Value> {
    let mut stream = connect_supervisor().await?;
    stream.write_all(&encode_line(&request)?).await?;
    stream.shutdown().await?;
    let mut reader = BufReader::new(stream);
    let line = read_line_limited(&mut reader, "supervisor response").await?;
    let response: ControlResponse =
        serde_json::from_str(line.trim()).context("invalid supervisor response")?;
    ensure_response_ok(response)
}

fn ensure_response_ok(response: ControlResponse) -> Result<Value> {
    if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        anyhow::bail!(
            response
                .error
                .unwrap_or_else(|| "session supervisor request failed".to_owned())
        )
    }
}

async fn connect_supervisor() -> Result<UnixStream> {
    let path = config::supervisor_socket_path()?;
    UnixStream::connect(&path).await.with_context(|| {
        format!(
            "Temote session supervisor is not running at {}; run `temote-mcp supervisor` first",
            path.display()
        )
    })
}

async fn prepare_supervisor_socket(path: &Path) -> Result<()> {
    match UnixStream::connect(path).await {
        Ok(_) => anyhow::bail!(
            "Temote session supervisor is already running at {}",
            path.display()
        ),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionAborted
            ) => {}
        Err(error) => return Err(error).context("failed to inspect supervisor socket"),
    }
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to remove stale supervisor socket"),
    }
}

async fn reconcile_stale_sessions() -> Result<()> {
    let directory = config::sessions_dir()?;
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("failed to read session metadata directory"),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if config::validate_session_id(id).is_err() {
            continue;
        }
        let session = match config::read_session_metadata(id).await {
            Ok(session) => session,
            Err(error) => {
                eprintln!("cannot reconcile session {id}: {error:#}");
                continue;
            }
        };
        match config::session_is_active(id).await {
            Ok(true) => {}
            Ok(false) => {
                let _ = config::remove_inactive_socket(id).await;
                let lifecycle = config::read_session_lifecycle(id).await?;
                match lifecycle.as_ref().map(|state| state.status) {
                    Some(LifecycleStatus::Stopped) => {}
                    Some(LifecycleStatus::Crashed) => {
                        if let Some(state) = lifecycle
                            && state.restart_policy == "on-failure"
                            && state.next_restart_at.is_some()
                        {
                            mark_restart_not_resumed(id, state).await?;
                        }
                    }
                    Some(_) => {
                        let on_failure = lifecycle
                            .as_ref()
                            .is_some_and(|state| state.restart_policy == "on-failure");
                        persist_crash(
                            id,
                            lifecycle,
                            session.started_at,
                            "session was active when its owning supervisor stopped",
                        )
                        .await?;
                        if on_failure
                            && let Some(state) = config::read_session_lifecycle(id).await?
                        {
                            mark_restart_not_resumed(id, state).await?;
                        }
                    }
                    None if session.process_id == 0 => {
                        let mut state = SessionLifecycle::starting(session.started_at, None);
                        state.status = LifecycleStatus::Stopped;
                        state.stopped_at = Some(config::unix_time());
                        state.exit_reason = Some("legacy inactive session metadata".to_owned());
                        config::save_session_lifecycle(id, &state).await?;
                    }
                    None => {
                        persist_crash(
                            id,
                            None,
                            session.started_at,
                            "session socket was not active after supervisor restart",
                        )
                        .await?;
                    }
                }
            }
            Err(error) => {
                eprintln!("cannot determine liveness for session {id}: {error:#}");
            }
        }
    }
    Ok(())
}

async fn mark_restart_not_resumed(id: &str, mut state: SessionLifecycle) -> Result<()> {
    state.next_restart_at = None;
    state.restart_limit_reason = Some(RESTART_NOT_RESUMED_AFTER_SUPERVISOR_RESTART.to_owned());
    state.exit_reason = state.restart_limit_reason.clone();
    config::save_session_lifecycle(id, &state).await
}

async fn persist_crash(
    id: &str,
    lifecycle: Option<SessionLifecycle>,
    started_at: u64,
    error: &str,
) -> Result<()> {
    let mut lifecycle = lifecycle.unwrap_or_else(|| SessionLifecycle::starting(started_at, None));
    lifecycle.status = LifecycleStatus::Crashed;
    lifecycle.stopped_at = Some(config::unix_time());
    lifecycle.exit_reason = Some("unexpected runtime termination".to_owned());
    lifecycle.last_error = Some(error.to_owned());
    config::save_session_lifecycle(id, &lifecycle).await
}

pub(crate) async fn inspect_session(id: &str) -> Result<SessionView> {
    config::validate_session_id(id)?;
    let session = config::read_session_metadata(id).await?;
    let mut lifecycle = config::read_session_lifecycle(id).await?;
    let liveness = config::session_is_active(id).await;

    let claims_live_runtime = lifecycle.as_ref().map_or(session.process_id != 0, |state| {
        matches!(
            state.status,
            LifecycleStatus::Starting | LifecycleStatus::Active | LifecycleStatus::Stopping
        )
    });
    if matches!(liveness, Ok(false)) && claims_live_runtime {
        persist_crash(
            id,
            lifecycle.take(),
            session.started_at,
            "session metadata claimed a live runtime but its socket is not active",
        )
        .await?;
        lifecycle = config::read_session_lifecycle(id).await?;
    }

    let inferred = lifecycle.unwrap_or_else(|| {
        let mut state = SessionLifecycle::starting(session.started_at, None);
        state.status = if session.process_id == 0 {
            LifecycleStatus::Stopped
        } else {
            LifecycleStatus::Active
        };
        state
    });

    let (status, last_error) = match liveness {
        Ok(true) => {
            let status = match inferred.status {
                LifecycleStatus::Starting => "starting",
                LifecycleStatus::Stopping => "stopping",
                _ => "active",
            };
            (status.to_owned(), inferred.last_error.clone())
        }
        Ok(false) => (
            status_name(inferred.status).to_owned(),
            inferred.last_error.clone(),
        ),
        Err(error) => (
            "unknown".to_owned(),
            Some(format!("liveness probe failed: {error:#}")),
        ),
    };
    let pid = matches!(status.as_str(), "starting" | "active" | "stopping")
        .then_some(session.process_id)
        .filter(|pid| *pid != 0);

    Ok(SessionView {
        host_id: host_identity::resolve()?,
        id: session.id.clone(),
        session_id: session.id,
        status,
        pid,
        process_id: session.process_id,
        cwd: session.cwd,
        permitted_directories: session.permitted_directories,
        started_at: inferred.started_at,
        stopped_at: inferred.stopped_at,
        exit_reason: inferred.exit_reason,
        last_error,
        permission_mode: if session.yolo { "yolo" } else { "ask" }.to_owned(),
        yolo: session.yolo,
        logical_path: inferred.logical_path,
        restart_policy: inferred.restart_policy,
        restart_count: inferred.restart_count,
        last_restart_at: inferred.last_restart_at,
        next_restart_at: inferred.next_restart_at,
        restart_limit_reason: inferred.restart_limit_reason,
    })
}

async fn list_session_views() -> Result<Vec<SessionView>> {
    let directory = config::sessions_dir()?;
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("failed to read session metadata directory"),
    };
    let mut sessions = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if config::validate_session_id(id).is_err() {
            continue;
        }
        if let Ok(session) = inspect_session(id).await {
            sessions.push(session);
        }
    }
    sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    Ok(sessions)
}

fn status_name(status: LifecycleStatus) -> &'static str {
    match status {
        LifecycleStatus::Starting => "starting",
        LifecycleStatus::Active => "active",
        LifecycleStatus::Stopping => "stopping",
        LifecycleStatus::Stopped => "stopped",
        LifecycleStatus::Crashed => "crashed",
    }
}

fn encode_line<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        bytes.len() < MAX_CONTROL_MESSAGE_BYTES,
        "supervisor control message exceeds {MAX_CONTROL_MESSAGE_BYTES} bytes"
    );
    bytes.push(b'\n');
    Ok(bytes)
}

async fn read_stream_line(stream: &mut UnixStream, label: &str) -> Result<String> {
    let mut line = String::new();
    let read = BufReader::new(stream)
        .take((MAX_CONTROL_MESSAGE_BYTES + 1) as u64)
        .read_line(&mut line)
        .await
        .with_context(|| format!("failed to read {label}"))?;
    anyhow::ensure!(read > 0, "{label} closed before a message");
    anyhow::ensure!(
        read <= MAX_CONTROL_MESSAGE_BYTES,
        "{label} exceeds {MAX_CONTROL_MESSAGE_BYTES} bytes"
    );
    Ok(line)
}

async fn read_line_limited<R>(reader: &mut R, label: &str) -> Result<String>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut line = String::new();
    let read = reader
        .take((MAX_CONTROL_MESSAGE_BYTES + 1) as u64)
        .read_line(&mut line)
        .await
        .with_context(|| format!("failed to read {label}"))?;
    if read == 0 {
        return Ok(String::new());
    }
    anyhow::ensure!(
        read <= MAX_CONTROL_MESSAGE_BYTES,
        "{label} exceeds {MAX_CONTROL_MESSAGE_BYTES} bytes"
    );
    Ok(line)
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::approvals;
    use crate::test_support;

    fn fixture() -> (tempfile::TempDir, NamedRoots) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("volume");
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let canonical = std::fs::canonicalize(root).unwrap();
        let roots =
            NamedRoots::from_canonical_roots(BTreeMap::from([("src".to_owned(), canonical)]))
                .unwrap();
        (temp, roots)
    }

    async fn cleanup(id: &str) {
        let _ = tokio::fs::remove_file(config::socket_path(id).unwrap()).await;
        let _ = tokio::fs::remove_file(config::session_path(id).unwrap()).await;
        let _ = tokio::fs::remove_file(config::session_lifecycle_path(id).unwrap()).await;
    }

    #[tokio::test]
    async fn captured_start_environment_is_session_scoped_and_not_persisted() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let id = format!("captured-env-{}", uuid::Uuid::new_v4());
        let secret = "credential-sentinel-not-for-disk";
        let environment = CapturedStartEnvironment::from_values(BTreeMap::from([
            (
                "KINTONE_BASE_URL".to_owned(),
                "https://example.cybozu.com".to_owned(),
            ),
            ("KINTONE_USERNAME".to_owned(), "user".to_owned()),
            ("KINTONE_PASSWORD".to_owned(), secret.to_owned()),
            ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
            ("HOME".to_owned(), std::env::var("HOME").unwrap_or_default()),
        ]))
        .unwrap();
        assert!(!format!("{environment:?}").contains(secret));

        supervisor
            .start_with_environment("src/repo", Some(&id), environment)
            .await
            .unwrap();

        let mcp_status = approvals::kintone_mcp_status(&id).await.unwrap();
        assert_eq!(mcp_status["configured"], true);
        assert_eq!(mcp_status["auth_mode"], "password");
        let cli_status = approvals::kintone_cli_status(&id).await.unwrap();
        assert_eq!(cli_status["configured"], true);
        assert_eq!(cli_status["auth_mode"], "password");

        let metadata = tokio::fs::read_to_string(config::session_path(&id).unwrap())
            .await
            .unwrap();
        let lifecycle = tokio::fs::read_to_string(config::session_lifecycle_path(&id).unwrap())
            .await
            .unwrap();
        assert!(!metadata.contains(secret));
        assert!(!lifecycle.contains(secret));

        supervisor.shutdown().await.unwrap();
        cleanup(&id).await;

        assert!(
            CapturedStartEnvironment::from_values(BTreeMap::from([(
                "LD_PRELOAD".to_owned(),
                "not-allowlisted".to_owned(),
            )]))
            .is_err()
        );
    }

    #[tokio::test]
    async fn ping_advertises_control_protocol_and_root_capability() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let result = dispatch_request(ControlRequest::Ping, &supervisor)
            .await
            .unwrap();
        assert_eq!(result["status"], "active");
        assert_eq!(result["control_protocol"], CONTROL_PROTOCOL_VERSION);
        assert_eq!(result["roots_configured"], true);
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn supervisor_restart_preserves_policy_but_does_not_resume_memory_only_restart() {
        let (temp, _roots) = fixture();
        let id = format!("restart-reconcile-{}", uuid::Uuid::new_v4());
        let cwd = temp.path().join("volume/repo");
        let mut session = config::new_session(&cwd, Some(&id), false).unwrap();
        session.process_id = std::process::id();
        config::save_session(&session).await.unwrap();
        let mut lifecycle =
            SessionLifecycle::starting(session.started_at, Some("src/repo".to_owned()));
        lifecycle.status = LifecycleStatus::Active;
        lifecycle.restart_policy = "on-failure".to_owned();
        lifecycle.restart_count = 2;
        config::save_session_lifecycle(&id, &lifecycle)
            .await
            .unwrap();

        reconcile_stale_sessions().await.unwrap();

        let reconciled = config::read_session_lifecycle(&id).await.unwrap().unwrap();
        assert_eq!(reconciled.status, LifecycleStatus::Crashed);
        assert_eq!(reconciled.restart_policy, "on-failure");
        assert_eq!(reconciled.restart_count, 2);
        assert!(reconciled.next_restart_at.is_none());
        assert_eq!(
            reconciled.restart_limit_reason.as_deref(),
            Some(RESTART_NOT_RESUMED_AFTER_SUPERVISOR_RESTART)
        );
        cleanup(&id).await;
    }

    #[tokio::test]
    async fn detached_permission_control_mutates_live_session_without_restart() {
        let (temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let id = format!("permission-control-{}", uuid::Uuid::new_v4());
        let info = supervisor.start("src/repo", Some(&id)).await.unwrap();
        let original_pid = config::read_session_metadata(&id).await.unwrap().process_id;
        let extra = temp.path().join("extra");
        std::fs::create_dir_all(&extra).unwrap();
        let canonical_extra = std::fs::canonicalize(&extra).unwrap();

        let allowed = dispatch_request(
            ControlRequest::PermissionAllow {
                session_id: id.clone(),
                path: extra,
            },
            &supervisor,
        )
        .await
        .unwrap();
        assert_eq!(allowed["process_id"], original_pid);
        let roots = allowed["permitted_directories"].as_array().unwrap();
        assert!(
            roots
                .iter()
                .any(|value| value.as_str() == canonical_extra.to_str())
        );

        let yolo = dispatch_request(
            ControlRequest::PermissionMode {
                session_id: id.clone(),
                yolo: true,
            },
            &supervisor,
        )
        .await
        .unwrap();
        assert_eq!(yolo["permission_mode"], "yolo");
        assert_eq!(yolo["process_id"], original_pid);

        let revoke_cwd = dispatch_request(
            ControlRequest::PermissionRevoke {
                session_id: id.clone(),
                path: info.cwd.clone(),
            },
            &supervisor,
        )
        .await;
        assert!(
            revoke_cwd
                .unwrap_err()
                .to_string()
                .contains("cannot revoke the session cwd")
        );

        let ask = dispatch_request(
            ControlRequest::PermissionMode {
                session_id: id.clone(),
                yolo: false,
            },
            &supervisor,
        )
        .await
        .unwrap();
        assert_eq!(ask["permission_mode"], "ask");
        assert_eq!(ask["process_id"], original_pid);

        supervisor.shutdown().await.unwrap();
        cleanup(&id).await;
    }

    #[tokio::test]
    async fn control_approval_routes_through_reconnectable_console_and_fails_closed() {
        let (_temp, roots) = fixture();
        let (supervisor, approvals) = SessionSupervisor::new(roots);
        let (registration, registrations) = mpsc::channel(8);
        let broker = tokio::spawn(run_approval_broker(approvals, registrations));

        let (console, mut console_rx) = mpsc::channel(1);
        registration.send(console).await.unwrap();

        let request = Request {
            id: uuid::Uuid::new_v4(),
            operation: "Authorize OAuth client".to_owned(),
            detail: "proxy approval".to_owned(),
            cwd: std::env::current_dir().unwrap(),
        };
        let supervisor_for_request = Arc::clone(&supervisor);
        let allowed = tokio::spawn(async move {
            dispatch_request(
                ControlRequest::Approval {
                    session_id: "oauth".to_owned(),
                    request,
                },
                &supervisor_for_request,
            )
            .await
        });
        let prompt = tokio::time::timeout(Duration::from_secs(1), console_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prompt.session_id, "oauth");
        assert_eq!(prompt.request.operation, "Authorize OAuth client");
        prompt.respond(true);
        let result = allowed.await.unwrap().unwrap();
        assert_eq!(result["allow"], true);

        drop(console_rx);
        let denied = dispatch_request(
            ControlRequest::Approval {
                session_id: "oauth".to_owned(),
                request: Request {
                    id: uuid::Uuid::new_v4(),
                    operation: "Authorize OAuth client".to_owned(),
                    detail: "console disconnected".to_owned(),
                    cwd: std::env::current_dir().unwrap(),
                },
            },
            &supervisor,
        )
        .await
        .unwrap();
        assert_eq!(denied["allow"], false);

        supervisor.shutdown().await.unwrap();
        broker.abort();
        let _ = broker.await;
    }

    #[tokio::test]
    async fn approval_console_absence_disconnect_and_reconnect_fail_closed() {
        let (_temp, roots) = fixture();
        let (supervisor, approvals) = SessionSupervisor::new(roots);
        let id = format!("console-lifecycle-{}", uuid::Uuid::new_v4());
        let info = supervisor.start("src/repo", Some(&id)).await.unwrap();
        let (registration, registrations) = mpsc::channel(8);
        let broker = tokio::spawn(run_approval_broker(approvals, registrations));

        assert!(
            !approvals::request(
                &id,
                "no-console",
                "must fail closed".to_owned(),
                info.cwd.clone(),
            )
            .await
            .unwrap()
        );
        assert!(config::session_is_active(&id).await.unwrap());

        let (console, mut console_rx) = mpsc::channel(1);
        registration.send(console).await.unwrap();
        let request_id = id.clone();
        let cwd = info.cwd.clone();
        let allowed = tokio::spawn(async move {
            approvals::request(&request_id, "attached", "allow".to_owned(), cwd).await
        });
        let prompt = tokio::time::timeout(Duration::from_secs(1), console_rx.recv())
            .await
            .unwrap()
            .unwrap();
        prompt.respond(true);
        assert!(allowed.await.unwrap().unwrap());

        drop(console_rx);
        let request_id = id.clone();
        let cwd = info.cwd.clone();
        let denied = tokio::spawn(async move {
            approvals::request(&request_id, "disconnected", "deny".to_owned(), cwd).await
        });
        assert!(!denied.await.unwrap().unwrap());
        assert!(config::session_is_active(&id).await.unwrap());

        let (console, mut console_rx) = mpsc::channel(1);
        registration.send(console).await.unwrap();
        let request_id = id.clone();
        let cwd = info.cwd.clone();
        let allowed = tokio::spawn(async move {
            approvals::request(&request_id, "reattached", "allow again".to_owned(), cwd).await
        });
        let prompt = tokio::time::timeout(Duration::from_secs(1), console_rx.recv())
            .await
            .unwrap()
            .unwrap();
        prompt.respond(true);
        assert!(allowed.await.unwrap().unwrap());

        supervisor.shutdown().await.unwrap();
        broker.abort();
        let _ = broker.await;
        cleanup(&id).await;
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ModelState {
        Absent,
        Active,
        Stopped,
        Crashed,
    }

    #[test]
    fn generated_lifecycle_sequences_match_reference_model() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        test_support::run(0x4c49_4645_4359_434c, 32, |ctx| {
            let (_temp, roots) = fixture();
            let nonce = noprop::sample_u64(ctx);
            let ids = [
                format!("lifecycle-{nonce:x}-a"),
                format!("lifecycle-{nonce:x}-b"),
                format!("lifecycle-{nonce:x}-c"),
            ];
            let steps = (0..16)
                .map(|_| {
                    (
                        noprop::sample_usize_in(ctx, 0..5),
                        noprop::sample_usize_in(ctx, 0..ids.len()),
                    )
                })
                .collect::<Vec<_>>();

            runtime.block_on(async {
                for id in &ids {
                    cleanup(id).await;
                }
                let (supervisor, _approvals) = SessionSupervisor::new(roots);
                let mut model = [ModelState::Absent; 3];

                for (operation, index) in steps {
                    let id = &ids[index];
                    match operation {
                        0 => {
                            let expected = model[index] != ModelState::Active;
                            let result = supervisor.start("src/repo", Some(id)).await;
                            assert_eq!(
                                result.is_ok(),
                                expected,
                                "start mismatch for {id}: state={:?}, result={result:?}",
                                model[index]
                            );
                            if expected {
                                model[index] = ModelState::Active;
                            }
                        }
                        1 => {
                            let expected = model[index] == ModelState::Active;
                            let result = supervisor.stop(id).await;
                            assert_eq!(
                                result.is_ok(),
                                expected,
                                "stop mismatch for {id}: state={:?}, result={result:?}",
                                model[index]
                            );
                            if expected {
                                model[index] = ModelState::Stopped;
                            }
                        }
                        2 => {
                            let expected = model[index] == ModelState::Active;
                            let result = supervisor.crash_for_test(id).await;
                            assert_eq!(
                                result.is_ok(),
                                expected,
                                "crash mismatch for {id}: state={:?}, result={result:?}",
                                model[index]
                            );
                            if expected {
                                tokio::time::timeout(Duration::from_secs(1), async {
                                    loop {
                                        if config::read_session_lifecycle(id)
                                            .await
                                            .unwrap()
                                            .is_some_and(|state| {
                                                state.status == LifecycleStatus::Crashed
                                            })
                                        {
                                            break;
                                        }
                                        tokio::task::yield_now().await;
                                    }
                                })
                                .await
                                .expect("injected crash did not persist crashed lifecycle");
                                tokio::time::timeout(Duration::from_secs(1), async {
                                    loop {
                                        supervisor.reap_finished().await;
                                        if !supervisor.is_managed_for_test(id).await {
                                            break;
                                        }
                                        tokio::task::yield_now().await;
                                    }
                                })
                                .await
                                .expect("crashed session handle was not reaped");
                                model[index] = ModelState::Crashed;
                            }
                        }
                        3 => {
                            let expected = model[index] != ModelState::Absent;
                            let result = restart_session(
                                &supervisor,
                                id,
                                CapturedStartEnvironment::default(),
                            )
                            .await;
                            assert_eq!(
                                result.is_ok(),
                                expected,
                                "restart mismatch for {id}: state={:?}, result={result:?}",
                                model[index]
                            );
                            if expected {
                                model[index] = ModelState::Active;
                            }
                        }
                        _ => {}
                    }

                    for (candidate_index, candidate) in ids.iter().enumerate() {
                        match model[candidate_index] {
                            ModelState::Absent => {
                                assert!(inspect_session(candidate).await.is_err());
                                assert!(!config::session_is_active(candidate).await.unwrap());
                            }
                            ModelState::Active => {
                                let view = inspect_session(candidate).await.unwrap();
                                assert_eq!(view.status, "active", "candidate={candidate}");
                                assert!(config::session_is_active(candidate).await.unwrap());
                            }
                            ModelState::Stopped => {
                                let view = inspect_session(candidate).await.unwrap();
                                assert_eq!(view.status, "stopped", "candidate={candidate}");
                                assert!(!config::session_is_active(candidate).await.unwrap());
                            }
                            ModelState::Crashed => {
                                let view = inspect_session(candidate).await.unwrap();
                                assert_eq!(view.status, "crashed", "candidate={candidate}");
                                assert!(!config::session_is_active(candidate).await.unwrap());
                            }
                        }
                    }
                }

                supervisor.shutdown().await.unwrap();
                for id in &ids {
                    cleanup(id).await;
                }
            });
            Ok(())
        })
    }

    #[test]
    fn generated_dead_socket_states_never_report_active() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        test_support::run(0x5354_414c_4553_5441, 128, |ctx| {
            let nonce = noprop::sample_u64(ctx);
            let variant = noprop::sample_usize_in(ctx, 0..6);
            let has_process_id = noprop::sample_bool(ctx);
            runtime.block_on(async {
                let root = tempfile::tempdir().unwrap();
                let id = format!("stale-pbt-{nonce:x}");
                cleanup(&id).await;
                let mut session = config::new_session(root.path(), Some(&id), false).unwrap();
                session.process_id = if has_process_id {
                    std::process::id()
                } else {
                    0
                };
                config::save_session(&session).await.unwrap();

                let configured_status = match variant {
                    0 => None,
                    1 => Some(LifecycleStatus::Starting),
                    2 => Some(LifecycleStatus::Active),
                    3 => Some(LifecycleStatus::Stopping),
                    4 => Some(LifecycleStatus::Stopped),
                    _ => Some(LifecycleStatus::Crashed),
                };
                if let Some(status) = configured_status {
                    let mut lifecycle =
                        SessionLifecycle::starting(session.started_at, Some("src/repo".to_owned()));
                    lifecycle.status = status;
                    if matches!(status, LifecycleStatus::Stopped | LifecycleStatus::Crashed) {
                        lifecycle.stopped_at = Some(config::unix_time());
                    }
                    config::save_session_lifecycle(&id, &lifecycle)
                        .await
                        .unwrap();
                }

                let expected = match configured_status {
                    Some(LifecycleStatus::Stopped) => "stopped",
                    Some(LifecycleStatus::Crashed) => "crashed",
                    Some(
                        LifecycleStatus::Starting
                        | LifecycleStatus::Active
                        | LifecycleStatus::Stopping,
                    ) => "crashed",
                    None if has_process_id => "crashed",
                    None => "stopped",
                };
                let view = inspect_session(&id).await.unwrap();
                assert_eq!(
                    view.status, expected,
                    "variant={variant} has_process_id={has_process_id}"
                );
                assert_ne!(view.status, "active");
                assert!(view.pid.is_none());
                assert!(!config::session_is_active(&id).await.unwrap());

                if expected == "crashed" {
                    let persisted = config::read_session_lifecycle(&id)
                        .await
                        .unwrap()
                        .expect("crashed state must be durable");
                    assert_eq!(persisted.status, LifecycleStatus::Crashed);
                    assert!(persisted.stopped_at.is_some());
                }
                cleanup(&id).await;
            });
            Ok(())
        })
    }

    #[tokio::test]
    async fn dead_active_metadata_is_never_reported_active() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("stale-lifecycle-{}", uuid::Uuid::new_v4());
        cleanup(&id).await;
        let mut session = config::new_session(root.path(), Some(&id), false).unwrap();
        session.process_id = std::process::id();
        config::save_session(&session).await.unwrap();
        let mut lifecycle =
            SessionLifecycle::starting(session.started_at, Some("src/repo".to_owned()));
        lifecycle.status = LifecycleStatus::Active;
        config::save_session_lifecycle(&id, &lifecycle)
            .await
            .unwrap();

        let view = inspect_session(&id).await.unwrap();
        assert_eq!(view.status, "crashed");
        assert!(view.pid.is_none());
        assert!(
            view.last_error
                .as_deref()
                .is_some_and(|error| error.contains("socket is not active"))
        );
        let persisted = config::read_session_lifecycle(&id).await.unwrap().unwrap();
        assert_eq!(persisted.status, LifecycleStatus::Crashed);
        cleanup(&id).await;
    }
}
