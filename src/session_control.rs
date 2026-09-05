use std::io::{Read as _, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
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
use crate::supervisor::{SessionSupervisor, SupervisorUpgradePlan};

const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;
const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONSOLE_QUEUE: usize = 1;
const CONTROL_PROTOCOL_VERSION: u64 = 1;
const LIFECYCLE_SCHEMA_VERSION: u64 = 1;
const UPGRADE_PLAN_SCHEMA_VERSION: u64 = 1;
const MAX_UPGRADE_PLAN_BYTES: usize = 1024 * 1024;
const UPGRADE_FAILURE_REPORT_SCHEMA_VERSION: u64 = 1;
const MAX_UPGRADE_FAILURE_REPORT_BYTES: usize = 64 * 1024;
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
    Upgrade {
        executable: PathBuf,
        target_version: String,
        #[serde(default)]
        environment: CapturedStartEnvironment,
        #[serde(default)]
        dry_run: bool,
        #[serde(default)]
        force: bool,
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct UpgradeFailureReport {
    report_schema: u64,
    source_version: String,
    target_version: String,
    planned_sessions: Vec<String>,
    restored_sessions: Vec<String>,
    unrestored_sessions: Vec<String>,
    rollback: String,
    error: String,
}

pub async fn run_supervisor(restore_plan_path: Option<PathBuf>) -> Result<()> {
    let path = config::supervisor_socket_path()?;
    prepare_supervisor_socket(&path).await?;
    let parent = path.parent().context("supervisor socket has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;

    reconcile_stale_sessions().await?;

    let roots = NamedRoots::from_env()?;
    let (supervisor, approvals) = SessionSupervisor::new(roots);

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to listen at {}", path.display()))?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;

    if let Some(restore_plan_path) = restore_plan_path.as_deref() {
        let plan = read_upgrade_plan(restore_plan_path)?;
        validate_restore_plan(&plan)?;
        let available_environment = CapturedStartEnvironment::capture();
        if let Err(error) = supervisor
            .restore_upgrade_plan(&plan, &available_environment)
            .await
        {
            let restore_error =
                redact_captured_environment_values(&format!("{error:#}"), &available_environment);
            let _ = tokio::fs::remove_file(&path).await;
            let shutdown = supervisor.shutdown().await;
            let shutdown_error = shutdown.as_ref().err().map(|error| format!("{error:#}"));
            let report =
                collect_upgrade_failure_report(&plan, &restore_error, shutdown_error.as_deref())
                    .await;
            if let Err(report_error) = write_upgrade_failure_report(restore_plan_path, &report) {
                return Err(anyhow::anyhow!(
                    "failed to restore sessions after supervisor upgrade: {restore_error}; additionally failed to persist deterministic failure report: {report_error:#}"
                ));
            }
            if let Err(shutdown_error) = shutdown {
                return Err(anyhow::anyhow!(
                    "failed to restore sessions after supervisor upgrade: {restore_error}; replacement rollback was incomplete: {shutdown_error:#}"
                ));
            }
            return Err(anyhow::anyhow!(
                "failed to restore sessions after supervisor upgrade: {restore_error}; replacement sessions were stopped and a failure report was preserved"
            ));
        }
        remove_upgrade_plan(restore_plan_path)?;
        eprintln!(
            "Temote supervisor handoff restored {} session(s) on version {}",
            plan.sessions.len(),
            env!("CARGO_PKG_VERSION")
        );
    }

    let (console_registration, console_registrations) = mpsc::channel(8);
    let approval_broker = tokio::spawn(run_approval_broker(approvals, console_registrations));

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

    match request {
        ControlRequest::AttachConsole => {
            handle_console_attachment(stream, console_registration).await
        }
        ControlRequest::Upgrade {
            executable,
            target_version,
            environment,
            dry_run,
            force,
        } => {
            handle_upgrade_request(
                stream,
                supervisor,
                executable,
                target_version,
                environment,
                dry_run,
                force,
            )
            .await
        }
        request => {
            let result = dispatch_request(request, &supervisor).await;
            let response = match result {
                Ok(result) => json!({"ok": true, "result": result, "error": Value::Null}),
                Err(error) => {
                    json!({"ok": false, "result": Value::Null, "error": format!("{error:#}")})
                }
            };
            stream.write_all(&encode_line(&response)?).await?;
            let _ = stream.shutdown().await;
            Ok(())
        }
    }
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
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "control_protocol": CONTROL_PROTOCOL_VERSION,
            "lifecycle_schema": LIFECYCLE_SCHEMA_VERSION,
            "upgrade_plan_schema": UPGRADE_PLAN_SCHEMA_VERSION,
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
        ControlRequest::Upgrade { .. } => unreachable!("handled before dispatch"),
        ControlRequest::AttachConsole => unreachable!("handled before dispatch"),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SupervisorCapabilities {
    version: String,
    control_protocol: u64,
    lifecycle_schema: u64,
    upgrade_plan_schema: u64,
}

pub fn print_supervisor_capabilities() -> Result<()> {
    let capabilities = SupervisorCapabilities {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        control_protocol: CONTROL_PROTOCOL_VERSION,
        lifecycle_schema: LIFECYCLE_SCHEMA_VERSION,
        upgrade_plan_schema: UPGRADE_PLAN_SCHEMA_VERSION,
    };
    println!("{}", serde_json::to_string(&capabilities)?);
    Ok(())
}

fn validate_upgrade_executable(
    path: &Path,
    claimed_version: &str,
) -> Result<(PathBuf, SupervisorCapabilities)> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve upgrade executable {}", path.display()))?;
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("cannot inspect upgrade executable {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "upgrade executable is not a regular file: {}",
        path.display()
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o111 != 0,
        "upgrade executable is not executable: {}",
        path.display()
    );

    let output = std::process::Command::new(&path)
        .args(["supervisor", "--capabilities"])
        .output()
        .with_context(|| {
            format!(
                "failed to inspect upgrade capabilities from {}",
                path.display()
            )
        })?;
    anyhow::ensure!(
        output.status.success(),
        "upgrade executable did not report supervisor capabilities successfully"
    );
    anyhow::ensure!(
        output.stdout.len() <= 64 * 1024,
        "upgrade capability response is too large"
    );
    let capabilities: SupervisorCapabilities = serde_json::from_slice(&output.stdout)
        .context("invalid supervisor capability response from upgrade executable")?;
    anyhow::ensure!(
        capabilities.version == claimed_version,
        "upgrade executable version changed during preflight: expected {claimed_version}, found {}",
        capabilities.version
    );
    anyhow::ensure!(
        capabilities.control_protocol == CONTROL_PROTOCOL_VERSION,
        "supervisor control protocol {} is incompatible with running protocol {}",
        capabilities.control_protocol,
        CONTROL_PROTOCOL_VERSION
    );
    anyhow::ensure!(
        capabilities.lifecycle_schema == LIFECYCLE_SCHEMA_VERSION,
        "lifecycle schema {} is incompatible with running schema {}",
        capabilities.lifecycle_schema,
        LIFECYCLE_SCHEMA_VERSION
    );
    anyhow::ensure!(
        capabilities.upgrade_plan_schema == UPGRADE_PLAN_SCHEMA_VERSION,
        "upgrade plan schema {} is incompatible with running schema {}",
        capabilities.upgrade_plan_schema,
        UPGRADE_PLAN_SCHEMA_VERSION
    );
    Ok((path, capabilities))
}

async fn write_control_error(stream: &mut UnixStream, error: &anyhow::Error) -> Result<()> {
    let response = json!({
        "ok": false,
        "result": Value::Null,
        "error": format!("{error:#}"),
    });
    stream.write_all(&encode_line(&response)?).await?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn handle_upgrade_request(
    mut stream: UnixStream,
    supervisor: Arc<SessionSupervisor>,
    executable: PathBuf,
    target_version: String,
    environment: CapturedStartEnvironment,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    let preflight: Result<(PathBuf, SupervisorUpgradePlan)> = async {
        environment.validate()?;
        let (executable, capabilities) = validate_upgrade_executable(&executable, &target_version)?;
        let plan = supervisor
            .build_upgrade_plan(
                &target_version,
                capabilities.control_protocol,
                capabilities.lifecycle_schema,
                &environment,
                !dry_run,
                force,
            )
            .await?;
        Ok((executable, plan))
    }
    .await;
    let (executable, plan) = match preflight {
        Ok(value) => value,
        Err(error) => {
            write_control_error(&mut stream, &error).await?;
            return Ok(());
        }
    };

    if dry_run || !plan.handoff_required {
        let response = json!({"ok": true, "result": plan, "error": Value::Null});
        stream.write_all(&encode_line(&response)?).await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    let plan_path = match write_upgrade_plan(&plan) {
        Ok(path) => path,
        Err(error) => {
            supervisor.clear_upgrade_fence();
            write_control_error(&mut stream, &error).await?;
            return Ok(());
        }
    };
    if let Err(error) = supervisor.quiesce_for_upgrade(&plan).await {
        let _ = remove_upgrade_plan(&plan_path);
        write_control_error(&mut stream, &error).await?;
        return Ok(());
    }
    let response = json!({
        "ok": true,
        "result": {
            "plan": plan,
            "handoff": "accepted",
            "restore_plan_path": plan_path
        },
        "error": Value::Null
    });
    if let Err(error) = stream.write_all(&encode_line(&response)?).await {
        let rollback = supervisor.rollback_upgrade(&plan).await;
        let _ = remove_upgrade_plan(&plan_path);
        return match rollback {
            Ok(()) => Err(error)
                .context("failed to acknowledge supervisor upgrade; quiesce was rolled back"),
            Err(rollback) => Err(anyhow::anyhow!(
                "failed to acknowledge supervisor upgrade: {error}; rollback also failed: {rollback:#}"
            )),
        };
    }
    let _ = stream.shutdown().await;

    if let Err(error) = supervisor.drain_for_upgrade(&plan).await {
        let rollback = supervisor.rollback_upgrade(&plan).await;
        let _ = remove_upgrade_plan(&plan_path);
        return match rollback {
            Ok(()) => Err(error).context("supervisor upgrade drain failed; sessions were restored"),
            Err(rollback) => Err(anyhow::anyhow!(
                "supervisor upgrade drain failed: {error:#}; rollback also failed: {rollback:#}"
            )),
        };
    }

    let mut command = std::process::Command::new(&executable);
    command
        .arg("supervisor")
        .arg("--restore-plan")
        .arg(&plan_path);
    let _exec_credential_handoff = environment.apply_to_command(&mut command)?;
    let exec_error = command.exec();
    #[cfg(target_os = "linux")]
    drop(_exec_credential_handoff);

    let rollback = supervisor.rollback_upgrade(&plan).await;
    let _ = remove_upgrade_plan(&plan_path);
    match rollback {
        Ok(()) => {
            Err(exec_error).context("failed to exec upgraded supervisor; sessions were restored")
        }
        Err(rollback) => Err(anyhow::anyhow!(
            "failed to exec upgraded supervisor: {exec_error}; rollback also failed: {rollback:#}"
        )),
    }
}

fn upgrade_plan_directory() -> Result<PathBuf> {
    Ok(config::state_dir()?.join("upgrade"))
}

fn write_upgrade_plan(plan: &SupervisorUpgradePlan) -> Result<PathBuf> {
    let directory = upgrade_plan_directory()?;
    std::fs::create_dir_all(&directory).with_context(|| {
        format!(
            "failed to create upgrade plan directory {}",
            directory.display()
        )
    })?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    let path = directory.join(format!("restore-{}.json", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(plan)?;
    anyhow::ensure!(
        bytes.len() <= MAX_UPGRADE_PLAN_BYTES,
        "supervisor upgrade plan exceeds {MAX_UPGRADE_PLAN_BYTES} bytes"
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("failed to create upgrade plan {}", path.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(path)
}

fn validate_upgrade_plan_path(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("upgrade plan has no parent directory")?;
    let expected = upgrade_plan_directory()?;
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("cannot resolve upgrade plan directory {}", parent.display()))?;
    let expected = std::fs::canonicalize(&expected).with_context(|| {
        format!(
            "cannot resolve expected upgrade plan directory {}",
            expected.display()
        )
    })?;
    anyhow::ensure!(
        parent == expected,
        "restore plan must be inside {}",
        expected.display()
    );
    anyhow::ensure!(
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("restore-")
                    && name.ends_with(".json")
                    && !name.ends_with(".failure.json")
            }),
        "invalid supervisor restore plan file name"
    );
    Ok(())
}

fn read_upgrade_plan(path: &Path) -> Result<SupervisorUpgradePlan> {
    validate_upgrade_plan_path(path)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("cannot open supervisor restore plan {}", path.display()))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "supervisor restore plan must be a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_UPGRADE_PLAN_BYTES as u64,
        "supervisor restore plan exceeds {MAX_UPGRADE_PLAN_BYTES} bytes"
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "supervisor restore plan must be owner-only (mode {mode:04o})"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_UPGRADE_PLAN_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_UPGRADE_PLAN_BYTES,
        "supervisor restore plan exceeds {MAX_UPGRADE_PLAN_BYTES} bytes"
    );
    serde_json::from_slice(&bytes).context("invalid supervisor restore plan")
}

fn validate_restore_plan(plan: &SupervisorUpgradePlan) -> Result<()> {
    anyhow::ensure!(
        plan.plan_schema == UPGRADE_PLAN_SCHEMA_VERSION,
        "unsupported supervisor restore plan schema"
    );
    anyhow::ensure!(
        plan.control_protocol == CONTROL_PROTOCOL_VERSION,
        "restore plan control protocol is incompatible"
    );
    anyhow::ensure!(
        plan.lifecycle_schema == LIFECYCLE_SCHEMA_VERSION,
        "restore plan lifecycle schema is incompatible"
    );
    anyhow::ensure!(
        plan.target_version == env!("CARGO_PKG_VERSION"),
        "restore plan target version does not match this binary"
    );
    Ok(())
}

fn upgrade_failure_report_path(restore_plan_path: &Path) -> Result<PathBuf> {
    validate_upgrade_plan_path(restore_plan_path)?;
    let file_name = restore_plan_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("restore plan file name is not valid UTF-8")?;
    let base = file_name
        .strip_suffix(".json")
        .context("restore plan file name has no .json suffix")?;
    Ok(restore_plan_path.with_file_name(format!("{base}.failure.json")))
}

fn validate_upgrade_failure_report_path(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("upgrade failure report has no parent directory")?;
    let expected = upgrade_plan_directory()?;
    let parent = std::fs::canonicalize(parent).with_context(|| {
        format!(
            "cannot resolve upgrade failure report directory {}",
            parent.display()
        )
    })?;
    let expected = std::fs::canonicalize(&expected).with_context(|| {
        format!(
            "cannot resolve expected upgrade plan directory {}",
            expected.display()
        )
    })?;
    anyhow::ensure!(
        parent == expected,
        "upgrade failure report must be inside {}",
        expected.display()
    );
    anyhow::ensure!(
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("restore-") && name.ends_with(".failure.json")),
        "invalid supervisor upgrade failure report file name"
    );
    Ok(())
}

fn write_upgrade_failure_report(
    restore_plan_path: &Path,
    report: &UpgradeFailureReport,
) -> Result<PathBuf> {
    let path = upgrade_failure_report_path(restore_plan_path)?;
    validate_upgrade_failure_report_path(&path)?;
    let bytes = serde_json::to_vec_pretty(report)?;
    anyhow::ensure!(
        bytes.len() <= MAX_UPGRADE_FAILURE_REPORT_BYTES,
        "supervisor upgrade failure report exceeds {MAX_UPGRADE_FAILURE_REPORT_BYTES} bytes"
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("failed to create upgrade failure report {}", path.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(path)
}

fn read_upgrade_failure_report(restore_plan_path: &Path) -> Result<Option<UpgradeFailureReport>> {
    let path = upgrade_failure_report_path(restore_plan_path)?;
    validate_upgrade_failure_report_path(&path)?;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot open upgrade failure report {}", path.display()));
        }
    };
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "supervisor upgrade failure report must be a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_UPGRADE_FAILURE_REPORT_BYTES as u64,
        "supervisor upgrade failure report exceeds {MAX_UPGRADE_FAILURE_REPORT_BYTES} bytes"
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "supervisor upgrade failure report must be owner-only (mode {mode:04o})"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_UPGRADE_FAILURE_REPORT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_UPGRADE_FAILURE_REPORT_BYTES,
        "supervisor upgrade failure report exceeds {MAX_UPGRADE_FAILURE_REPORT_BYTES} bytes"
    );
    let report: UpgradeFailureReport =
        serde_json::from_slice(&bytes).context("invalid supervisor upgrade failure report")?;
    anyhow::ensure!(
        report.report_schema == UPGRADE_FAILURE_REPORT_SCHEMA_VERSION,
        "unsupported supervisor upgrade failure report schema"
    );
    Ok(Some(report))
}

fn redact_captured_environment_values(
    text: &str,
    environment: &CapturedStartEnvironment,
) -> String {
    let mut redacted = text.to_owned();
    for (name, value) in environment.values() {
        if !value.is_empty() && redacted.contains(value) {
            redacted = redacted.replace(value, &format!("<redacted:{name}>"));
        }
    }
    redacted
}

async fn collect_upgrade_failure_report(
    plan: &SupervisorUpgradePlan,
    restore_error: &str,
    shutdown_error: Option<&str>,
) -> UpgradeFailureReport {
    let planned_sessions = plan
        .sessions
        .iter()
        .map(|session| session.session_id.clone())
        .collect::<Vec<_>>();
    let mut restored_sessions = Vec::new();
    let mut unrestored_sessions = Vec::new();
    let mut probe_errors = Vec::new();
    for session_id in &planned_sessions {
        match config::session_is_active(session_id).await {
            Ok(true) => restored_sessions.push(session_id.clone()),
            Ok(false) => unrestored_sessions.push(session_id.clone()),
            Err(error) => {
                unrestored_sessions.push(session_id.clone());
                probe_errors.push(format!("{session_id}: {error:#}"));
            }
        }
    }

    let rollback = if shutdown_error.is_none() && restored_sessions.is_empty() {
        "replacement_sessions_stopped"
    } else {
        "incomplete"
    };
    let mut error = restore_error.to_owned();
    if let Some(shutdown_error) = shutdown_error {
        error.push_str("; replacement shutdown failed: ");
        error.push_str(shutdown_error);
    }
    if !probe_errors.is_empty() {
        error.push_str("; post-rollback liveness probe errors: ");
        error.push_str(&probe_errors.join(", "));
    }
    UpgradeFailureReport {
        report_schema: UPGRADE_FAILURE_REPORT_SCHEMA_VERSION,
        source_version: plan.source_version.clone(),
        target_version: plan.target_version.clone(),
        planned_sessions,
        restored_sessions,
        unrestored_sessions,
        rollback: rollback.to_owned(),
        error,
    }
}

fn format_upgrade_failure_report(report: &UpgradeFailureReport) -> String {
    format!(
        "supervisor handoff failed after exec\nsource: {}\ntarget: {}\nrollback: {}\nrestored: [{}]\nunrestored: [{}]\ncause: {}",
        report.source_version,
        report.target_version,
        report.rollback,
        report.restored_sessions.join(", "),
        report.unrestored_sessions.join(", "),
        report.error
    )
}

fn remove_upgrade_plan(path: &Path) -> Result<()> {
    validate_upgrade_plan_path(path)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove restore plan {}", path.display()))
        }
    }
}

pub async fn upgrade(dry_run: bool, force: bool) -> Result<()> {
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    let target_version = env!("CARGO_PKG_VERSION").to_owned();
    let ping = request(ControlRequest::Ping).await?;
    let source_version = ping
        .get("version")
        .and_then(Value::as_str)
        .context("running supervisor did not report its version; bootstrap by restarting it once with a handoff-capable Temote release")?;
    let source_pid = ping.get("pid").and_then(Value::as_u64).unwrap_or_default();
    anyhow::ensure!(
        ping.get("control_protocol").and_then(Value::as_u64) == Some(CONTROL_PROTOCOL_VERSION),
        "running supervisor control protocol is incompatible; manual supervisor restart is required"
    );

    let result = request(ControlRequest::Upgrade {
        executable,
        target_version: target_version.clone(),
        environment: CapturedStartEnvironment::capture(),
        dry_run,
        force,
    })
    .await
    .with_context(|| {
        format!(
            "running supervisor {source_version} does not support safe handoff or rejected the upgrade; bootstrap with one manual supervisor restart if this is the first handoff-capable release"
        )
    })?;

    if dry_run {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    let plan_value = result
        .get("plan")
        .cloned()
        .unwrap_or_else(|| result.clone());
    let plan: SupervisorUpgradePlan = serde_json::from_value(plan_value)?;
    let restore_plan_path = result
        .get("restore_plan_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    if !plan.handoff_required {
        println!("supervisor already runs Temote {target_version}; no handoff required");
    } else {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(restore_plan_path) = restore_plan_path.as_deref()
                && let Some(report) = read_upgrade_failure_report(restore_plan_path)?
            {
                anyhow::ensure!(
                    report.source_version == source_version
                        && report.target_version == target_version,
                    "supervisor upgrade failure report identity does not match the requested handoff"
                );
                anyhow::bail!(format_upgrade_failure_report(&report));
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "supervisor handoff did not become healthy within the bounded verification window; source version={source_version} pid={source_pid}, target version={target_version}"
                );
            }
            match request(ControlRequest::Ping).await {
                Ok(status)
                    if status.get("version").and_then(Value::as_str)
                        == Some(target_version.as_str())
                        && status.get("pid").and_then(Value::as_u64) == Some(source_pid) =>
                {
                    let mut healthy = true;
                    for session in &plan.sessions {
                        match request(ControlRequest::Info {
                            session_id: session.session_id.clone(),
                        })
                        .await
                        {
                            Ok(view)
                                if view.get("status").and_then(Value::as_str) == Some("active") => {
                            }
                            _ => {
                                healthy = false;
                                break;
                            }
                        }
                    }
                    if healthy {
                        break;
                    }
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!(
            "supervisor handoff complete: {source_version} -> {target_version}; restored {} session(s); pid={source_pid}",
            plan.sessions.len()
        );
    }

    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    match std::process::Command::new(&executable)
        .args(["codex", "plugin", "install"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            if !text.trim().is_empty() {
                println!("{}", text.trim());
            }
        }
        Ok(output) => {
            eprintln!(
                "Codex plugin reconciliation failed (exit {}); run `temote-mcp codex plugin install` manually. An already-running Codex session must be restarted after plugin replacement.\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(error) => {
            eprintln!(
                "Codex plugin reconciliation could not start: {error}; run `temote-mcp codex plugin install` manually. An already-running Codex session must be restarted after plugin replacement."
            );
        }
    }
    Ok(())
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
        assert_eq!(result["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(result["pid"], std::process::id());
        assert_eq!(result["control_protocol"], CONTROL_PROTOCOL_VERSION);
        assert_eq!(result["lifecycle_schema"], LIFECYCLE_SCHEMA_VERSION);
        assert_eq!(result["upgrade_plan_schema"], UPGRADE_PLAN_SCHEMA_VERSION);
        assert_eq!(result["roots_configured"], true);
        supervisor.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn upgrade_executable_gate_rejects_incompatible_protocol_without_running_handoff() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("fake-temote");
        std::fs::write(
            &executable,
            b"#!/bin/sh\necho '{\"version\":\"test-version\",\"control_protocol\":999,\"lifecycle_schema\":1,\"upgrade_plan_schema\":1}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = validate_upgrade_executable(&executable, "test-version").unwrap_err();
        assert!(error.to_string().contains("control protocol"), "{error:#}");
    }

    #[test]
    fn upgrade_failure_report_redacts_captured_environment_values() {
        let secret = "credential-sentinel-must-not-persist";
        let environment = CapturedStartEnvironment::from_values(BTreeMap::from([(
            "KINTONE_PASSWORD".to_owned(),
            secret.to_owned(),
        )]))
        .unwrap();
        let error = format!("injected child failure included {secret}");
        let redacted = redact_captured_environment_values(&error, &environment);
        assert!(!redacted.contains(secret));
        assert!(redacted.contains("<redacted:KINTONE_PASSWORD>"));
    }

    fn failure_report_fixture(secret: &str) -> (SupervisorUpgradePlan, UpgradeFailureReport) {
        let session_id = format!("upgrade-failure-{}", uuid::Uuid::new_v4());
        let plan = SupervisorUpgradePlan {
            plan_schema: UPGRADE_PLAN_SCHEMA_VERSION,
            source_version: "source-version".to_owned(),
            target_version: env!("CARGO_PKG_VERSION").to_owned(),
            control_protocol: CONTROL_PROTOCOL_VERSION,
            lifecycle_schema: LIFECYCLE_SCHEMA_VERSION,
            supervisor_pid: std::process::id(),
            created_at: config::unix_time(),
            handoff_required: true,
            sessions: vec![crate::supervisor::UpgradeSessionPlan {
                session_id: session_id.clone(),
                cwd: std::env::current_dir().unwrap(),
                permitted_directories: vec![std::env::current_dir().unwrap()],
                yolo: false,
                logical_path: None,
                restart_policy: "never".to_owned(),
                public: false,
                restart_context_keys: vec!["KINTONE_PASSWORD".to_owned()],
            }],
        };
        let report = UpgradeFailureReport {
            report_schema: UPGRADE_FAILURE_REPORT_SCHEMA_VERSION,
            source_version: plan.source_version.clone(),
            target_version: plan.target_version.clone(),
            planned_sessions: vec![session_id.clone()],
            restored_sessions: Vec::new(),
            unrestored_sessions: vec![session_id],
            rollback: "replacement_sessions_stopped".to_owned(),
            error: "injected restore failure".to_owned(),
        };
        assert!(!serde_json::to_string(&report).unwrap().contains(secret));
        (plan, report)
    }

    #[test]
    fn upgrade_failure_report_is_owner_only_bounded_and_secret_free() {
        let secret = "credential-sentinel-must-not-persist";
        let (plan, report) = failure_report_fixture(secret);
        let plan_path = write_upgrade_plan(&plan).unwrap();
        let report_path = write_upgrade_failure_report(&plan_path, &report).unwrap();
        let mode = std::fs::metadata(&report_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode & 0o077, 0);
        let bytes = std::fs::read(&report_path).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(secret));
        assert_eq!(
            read_upgrade_failure_report(&plan_path).unwrap(),
            Some(report)
        );

        std::fs::remove_file(&report_path).unwrap();
        remove_upgrade_plan(&plan_path).unwrap();
    }

    #[test]
    fn upgrade_failure_report_rejects_outside_paths_symlinks_and_oversize() {
        use std::os::unix::fs::symlink;

        let (_plan, report) = failure_report_fixture("not-written");
        let outside = tempfile::tempdir().unwrap();
        let outside_report = outside.path().join("restore-outside.failure.json");
        assert!(validate_upgrade_failure_report_path(&outside_report).is_err());

        let directory = upgrade_plan_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plan_path = directory.join(format!("restore-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&plan_path, b"{}").unwrap();
        std::fs::set_permissions(&plan_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let report_path = upgrade_failure_report_path(&plan_path).unwrap();
        let target = outside.path().join("target.json");
        std::fs::write(&target, serde_json::to_vec(&report).unwrap()).unwrap();
        symlink(&target, &report_path).unwrap();
        let symlink_error = read_upgrade_failure_report(&plan_path).unwrap_err();
        assert!(
            format!("{symlink_error:#}").contains("cannot open upgrade failure report"),
            "{symlink_error:#}"
        );
        std::fs::remove_file(&report_path).unwrap();

        std::fs::write(
            &report_path,
            vec![b'x'; MAX_UPGRADE_FAILURE_REPORT_BYTES + 1],
        )
        .unwrap();
        std::fs::set_permissions(&report_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let oversized = read_upgrade_failure_report(&plan_path).unwrap_err();
        assert!(format!("{oversized:#}").contains("exceeds"));
        std::fs::remove_file(&report_path).unwrap();
        std::fs::remove_file(&plan_path).unwrap();
    }

    #[tokio::test]
    async fn failure_report_classifies_partial_restore_as_incomplete() {
        let (mut plan, _report) = failure_report_fixture("not-written");
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let active_id = format!("upgrade-partial-{}", uuid::Uuid::new_v4());
        supervisor
            .start("src/repo", Some(&active_id))
            .await
            .unwrap();
        plan.sessions[0].session_id = active_id.clone();
        let missing_id = format!("upgrade-missing-{}", uuid::Uuid::new_v4());
        let mut missing = plan.sessions[0].clone();
        missing.session_id = missing_id.clone();
        plan.sessions.push(missing);

        let report = collect_upgrade_failure_report(&plan, "injected failure", None).await;
        assert_eq!(report.rollback, "incomplete");
        assert_eq!(report.restored_sessions, vec![active_id.clone()]);
        assert_eq!(report.unrestored_sessions, vec![missing_id.clone()]);
        assert!(format_upgrade_failure_report(&report).contains("rollback: incomplete"));

        supervisor.shutdown().await.unwrap();
        cleanup(&active_id).await;
        cleanup(&missing_id).await;
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
