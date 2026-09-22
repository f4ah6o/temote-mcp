use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{IsTerminal as _, Read as _, Seek as _, Write};
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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
use temote_mcp::activity::broker::{ActivityBroker, ActivityDelivery};
use temote_mcp::activity::contract::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, decode_event, encode_event,
};
use temote_mcp::activity::render::render_event;
use uuid::Uuid;

const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_SESSION_LIST_ENTRIES: usize = 256;
const MAX_SESSION_HISTORY_DIRECTORY_ENTRIES_SCANNED: usize = 16 * 1024;
const MAX_SESSION_HISTORY_CANDIDATES: usize = 4096;
const MAX_SESSION_CONTROL_LIST_BYTES: usize = 56 * 1024;
const TERMINAL_SESSION_RETENTION: usize = 512;
const RETENTION_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
// Upgrade requests can run while the caller owns the global admission lock or
// the per-transaction lease. Bound the entire connect/write/read exchange so a
// stalled supervisor cannot retain either lock indefinitely.
const UPGRADE_CONTROL_RPC_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ACTIVITY_OUTPUT_QUEUE: usize = 256;
const MAX_ACTIVITY_DIAGNOSTIC_QUEUE: usize = 64;
const ACTIVITY_OUTPUT_TIMEOUT: Duration = Duration::from_secs(5);
const SUPERVISOR_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(5);
const SUPERVISOR_BOOTSTRAP_POLL: Duration = Duration::from_millis(50);
const MAX_CONSOLE_QUEUE: usize = 1;
pub(crate) const CONTROL_PROTOCOL_VERSION: u64 = 2;
const LIFECYCLE_SCHEMA_VERSION: u64 = 1;
const UPGRADE_PLAN_SCHEMA_VERSION: u64 = 1;
const MAX_UPGRADE_PLAN_BYTES: usize = 1024 * 1024;
const UPGRADE_FAILURE_REPORT_SCHEMA_VERSION: u64 = 1;
const MAX_UPGRADE_FAILURE_REPORT_BYTES: usize = 64 * 1024;
const RESTART_NOT_RESUMED_AFTER_SUPERVISOR_RESTART: &str = "automatic restart was not resumed after supervisor restart because captured start credentials are intentionally memory-only; use `temote-mcp session restart <id>`";
const MAX_UPGRADE_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;
pub const INTERNAL_INSTALLED_LOCATOR_ENV: &str = "TEMOTE_MCP_INTERNAL_INSTALLED_LOCATOR";
static INSTALLED_UPGRADE_LOCATOR: OnceLock<PathBuf> = OnceLock::new();

pub fn initialize_installed_upgrade_locator() -> Result<()> {
    if INSTALLED_UPGRADE_LOCATOR.get().is_some() {
        return Ok(());
    }
    let path = std::fs::canonicalize(std::env::current_exe()?)?;
    let _ = INSTALLED_UPGRADE_LOCATOR.set(path);
    Ok(())
}

pub fn initialize_installed_upgrade_locator_from(path: &Path) -> Result<()> {
    let path = std::fs::canonicalize(path).context("cannot resolve installed Temote locator")?;
    if let Some(existing) = INSTALLED_UPGRADE_LOCATOR.get() {
        anyhow::ensure!(
            existing == &path,
            "installed Temote startup locator changed"
        );
        return Ok(());
    }
    let _ = INSTALLED_UPGRADE_LOCATOR.set(path);
    Ok(())
}

pub(crate) fn installed_upgrade_locator() -> Result<PathBuf> {
    initialize_installed_upgrade_locator()?;
    INSTALLED_UPGRADE_LOCATOR
        .get()
        .cloned()
        .context("installed Temote executable locator was not initialized")
}

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
    Forget {
        session_id: String,
    },
    Gc {
        #[serde(default)]
        apply: bool,
        limit: usize,
    },
    Restart {
        session_id: String,
        #[serde(default)]
        environment: CapturedStartEnvironment,
        #[serde(default)]
        public: bool,
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
        #[serde(default)]
        permission_mode: Option<config::PermissionMode>,
        #[serde(default)]
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
    PermissionGrant {
        session_id: String,
        request: config::SessionGrantRequest,
    },
    PermissionUngrant {
        session_id: String,
        request: config::SessionGrantRequest,
    },
    ValidatePublicUpgradeSession {
        session_id: String,
    },
    Upgrade {
        executable: PathBuf,
        #[serde(default)]
        installed_locator: Option<PathBuf>,
        target_version: String,
        #[serde(default)]
        environment: CapturedStartEnvironment,
        #[serde(default)]
        dry_run: bool,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        expected_sessions: Option<Vec<crate::upgrade_transaction::UpgradePlannedSession>>,
    },
    AttachConsole,
    AttachActivity(AttachActivityRequest),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachActivityRequest {
    schema_version: u64,
    session_id: Option<String>,
    tail: usize,
    follow: bool,
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
    pub permission_mode: config::PermissionMode,
    pub yolo: bool,
    /// Persisted additive capability grants (listen ports, dev-tool env
    /// prefixes, ambient Git credentials). Empty by default.
    #[serde(default)]
    pub grants: config::SessionGrants,
    pub logical_path: Option<String>,
    /// Bounded non-secret workspace identity derived from the canonical session
    /// working directory. `None` when the workspace is not a supported standard
    /// Git worktree or no longer resolves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::managed_worktree::SessionWorkspace>,
    pub restart_policy: String,
    pub restart_count: u32,
    pub last_restart_at: Option<u64>,
    pub next_restart_at: Option<u64>,
    pub restart_limit_reason: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct SessionMetadataDiagnostics {
    pub total_entries: usize,
    pub json_entries: usize,
    pub state_entries: usize,
    pub other_entries: usize,
    pub retained_terminal_count: usize,
    pub safely_prunable_count: usize,
    pub invalid_orphan_count: usize,
    pub missing_json_count: usize,
    pub missing_state_count: usize,
}

#[derive(Clone, Debug)]
struct TerminalMetadataCandidate {
    id: String,
    started_at: u64,
    stopped_at: u64,
}

#[derive(Debug)]
struct RetentionPlan {
    diagnostics: SessionMetadataDiagnostics,
    prune_candidates: Vec<TerminalMetadataCandidate>,
}

/// Grace period before a lone metadata half is considered an orphan rather
/// than a partial durable write or an in-flight lifecycle transition.
pub(crate) const SESSION_ORPHAN_GRACE_SECONDS: u64 = 24 * 60 * 60;
pub(crate) const MAX_SESSION_GC_LIMIT: usize = 1000;
pub(crate) const SESSION_GC_MIN_LIMIT: usize = 1;

/// Initial reviewed orphan classes eligible for maintenance GC.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionGcReason {
    /// Only the `.state` lifecycle half exists.
    MissingJson,
    /// Only the `.json` metadata half exists.
    MissingState,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionGcEntry {
    pub session_id: String,
    pub reason: SessionGcReason,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionGcReport {
    pub dry_run: bool,
    pub grace_seconds: u64,
    pub limit: usize,
    pub missing_json_orphans: usize,
    pub missing_state_orphans: usize,
    pub ineligible_orphans: usize,
    pub candidates: Vec<SessionGcEntry>,
    pub removed: Vec<SessionGcEntry>,
    pub skipped: Vec<SessionGcEntry>,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
struct SessionGcCandidate {
    id: String,
    reason: SessionGcReason,
    modified_secs: u64,
    kind: &'static str,
}

#[derive(Debug)]
struct SessionGcPlan {
    report: SessionGcReport,
    candidates: Vec<SessionGcCandidate>,
}

#[derive(Clone)]
pub enum SessionBackend {
    #[cfg(test)]
    InProcess(Arc<SessionSupervisor>),
    LocalControl,
}

impl SessionBackend {
    pub async fn list(&self) -> Result<Vec<SessionView>> {
        match self {
            #[cfg(test)]
            Self::InProcess(supervisor) => list_session_views(supervisor).await,
            Self::LocalControl => request_session_views().await,
        }
    }

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

    pub async fn status(&self) -> Result<Value> {
        match self {
            #[cfg(test)]
            Self::InProcess(supervisor) => Ok(json!({
                "status": "active",
                "host_id": host_identity::resolve()?,
                "version": env!("CARGO_PKG_VERSION"),
                "control_protocol": CONTROL_PROTOCOL_VERSION,
                "roots_configured": supervisor.roots_configured(),
                "named_roots": supervisor.named_root_names(),
            })),
            Self::LocalControl => request(ControlRequest::Ping).await,
        }
    }

    pub async fn roots_configured(&self) -> Result<bool> {
        Ok(self
            .status()
            .await?
            .get("roots_configured")
            .and_then(Value::as_bool)
            .unwrap_or(false))
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

    pub async fn restart(&self, session_id: &str) -> Result<Value> {
        match self {
            #[cfg(test)]
            Self::InProcess(supervisor) => {
                restart_session(
                    supervisor,
                    session_id,
                    CapturedStartEnvironment::default(),
                    true,
                )
                .await?;
                Ok(serde_json::to_value(inspect_session(session_id).await?)?)
            }
            Self::LocalControl => {
                let lifecycle = config::read_session_lifecycle(session_id)
                    .await?
                    .context("public managed session has no lifecycle metadata")?;
                let path = lifecycle
                    .logical_path
                    .as_deref()
                    .context("public managed session has no named-root path")?;
                request(ControlRequest::Stop {
                    session_id: session_id.to_owned(),
                    public: true,
                })
                .await?;
                request(ControlRequest::Start {
                    path: path.to_owned(),
                    session_id: session_id.to_owned(),
                    environment: CapturedStartEnvironment::default(),
                    public: true,
                })
                .await
            }
        }
    }

    pub async fn validate_upgrade_session(&self, session_id: &str) -> Result<config::Session> {
        config::validate_session_id(session_id)?;
        match self {
            #[cfg(test)]
            Self::InProcess(supervisor) => {
                supervisor.validate_public_upgrade_session(session_id).await
            }
            Self::LocalControl => {
                upgrade_request(ControlRequest::ValidatePublicUpgradeSession {
                    session_id: session_id.to_owned(),
                })
                .await?;
                config::read_session_metadata(session_id).await
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityAttachResult {
    control_protocol: u64,
    activity_schema: u64,
    generation: Uuid,
    snapshot_sequence: u64,
    replayed: usize,
    history_truncated: bool,
}

#[derive(Debug)]
struct ActivityAttachResponse {
    ok: bool,
    result: Option<ActivityAttachResult>,
    error: Option<String>,
}

struct ActivityAttachResponseVisitor;

impl<'de> serde::de::Visitor<'de> for ActivityAttachResponseVisitor {
    type Value = ActivityAttachResponse;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an activity attach response")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut ok = None;
        let mut result = None;
        let mut result_seen = false;
        let mut error = None;
        let mut error_seen = false;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "ok" => {
                    if ok.is_some() {
                        return Err(serde::de::Error::custom("invalid activity attach response"));
                    }
                    ok = Some(map.next_value()?);
                }
                "result" => {
                    if result_seen {
                        return Err(serde::de::Error::custom("invalid activity attach response"));
                    }
                    result_seen = true;
                    result = map.next_value()?;
                }
                "error" => {
                    if error_seen {
                        return Err(serde::de::Error::custom("invalid activity attach response"));
                    }
                    error_seen = true;
                    error = map.next_value()?;
                }
                _ => return Err(serde::de::Error::custom("invalid activity attach response")),
            }
        }
        if !result_seen || !error_seen {
            return Err(serde::de::Error::custom("invalid activity attach response"));
        }
        Ok(ActivityAttachResponse {
            ok: ok.ok_or_else(|| serde::de::Error::custom("invalid activity attach response"))?,
            result,
            error,
        })
    }
}

impl<'de> Deserialize<'de> for ActivityAttachResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(ActivityAttachResponseVisitor)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityEndFrame {
    #[serde(rename = "type")]
    frame_type: String,
    snapshot_sequence: u64,
    history_truncated: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityGapFrame {
    #[serde(rename = "type")]
    frame_type: String,
    scope: String,
    after_sequence: u64,
    through_sequence: u64,
    dropped: u64,
}

#[derive(Debug)]
enum ActivityClientFrame {
    Event(ActivityEvent),
    End,
    Gap {
        after_sequence: u64,
        through_sequence: u64,
        dropped: u64,
    },
}

struct ActivityStreamState {
    attach: ActivityAttachResult,
    session_id: Option<String>,
    replayed: usize,
    last_event_sequence: u64,
    live_cursor: u64,
    ended: bool,
}

struct ActivityClientConnection {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    _writer: tokio::net::unix::OwnedWriteHalf,
    follow: bool,
    state: ActivityStreamState,
}

impl ActivityClientConnection {
    async fn attach(
        stream: UnixStream,
        session_id: Option<String>,
        tail: usize,
        follow: bool,
    ) -> Result<Self> {
        anyhow::ensure!(tail <= 1024, "activity tail exceeds 1024 events");
        let (reader, mut writer) = stream.into_split();
        let request = encode_line(&ControlRequest::AttachActivity(AttachActivityRequest {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            session_id: session_id.clone(),
            tail,
            follow,
        }))?;
        tokio::time::timeout(CONTROL_READ_TIMEOUT, writer.write_all(&request))
            .await
            .context("timed out writing activity attach request")??;
        let mut reader = BufReader::new(reader);
        let response = read_activity_client_line(&mut reader, "activity attach response").await?;
        let attach = decode_activity_attach_response(&response, tail)?;
        Ok(Self {
            reader,
            _writer: writer,
            follow,
            state: ActivityStreamState::new(attach, session_id),
        })
    }

    async fn next_frame(&mut self) -> Result<Option<ActivityClientFrame>> {
        let line = if activity_frame_read_has_timeout(self.follow, self.state.ended) {
            read_activity_client_line(&mut self.reader, "activity stream frame").await?
        } else {
            read_line_limited(&mut self.reader, "activity stream frame").await?
        };
        if line.is_empty() {
            return Ok(None);
        }
        self.state.decode_line(&line).map(Some)
    }
}

fn activity_frame_read_has_timeout(follow: bool, replay_ended: bool) -> bool {
    !follow || !replay_ended
}

impl ActivityStreamState {
    fn new(attach: ActivityAttachResult, session_id: Option<String>) -> Self {
        let live_cursor = attach.snapshot_sequence;
        Self {
            attach,
            session_id,
            replayed: 0,
            last_event_sequence: 0,
            live_cursor,
            ended: false,
        }
    }

    fn decode_line(&mut self, line: &str) -> Result<ActivityClientFrame> {
        let frame = line
            .strip_suffix('\n')
            .context("activity frame is missing newline terminator")?;
        anyhow::ensure!(!frame.ends_with('\r'), "invalid activity frame terminator");
        if let Ok(event) = decode_event(frame.as_bytes()) {
            anyhow::ensure!(
                self.last_event_sequence < event.sequence(),
                "activity event sequence is not increasing"
            );
            anyhow::ensure!(
                self.session_id
                    .as_deref()
                    .is_none_or(|expected| event.session_id() == Some(expected)),
                "activity event does not match requested session"
            );
            if self.ended {
                let advances_stream = if self.session_id.is_some() {
                    event.sequence() > self.live_cursor
                } else {
                    self.live_cursor
                        .checked_add(1)
                        .is_some_and(|next| event.sequence() == next)
                };
                anyhow::ensure!(advances_stream, "invalid live activity sequence");
                self.live_cursor = event.sequence();
            } else {
                anyhow::ensure!(
                    event.sequence() <= self.attach.snapshot_sequence,
                    "activity replay exceeds snapshot boundary"
                );
                self.replayed += 1;
                anyhow::ensure!(
                    self.replayed <= self.attach.replayed,
                    "too many activity replay events"
                );
            }
            self.last_event_sequence = event.sequence();
            return Ok(ActivityClientFrame::Event(event));
        }
        if let Ok(end) = serde_json::from_str::<ActivityEndFrame>(frame) {
            anyhow::ensure!(
                !self.ended && end.frame_type == "activity_end",
                "invalid activity replay frame"
            );
            anyhow::ensure!(
                end.snapshot_sequence == self.attach.snapshot_sequence
                    && end.history_truncated == self.attach.history_truncated,
                "activity_end does not match attachment"
            );
            anyhow::ensure!(
                self.replayed == self.attach.replayed,
                "activity replay count mismatch"
            );
            self.ended = true;
            return Ok(ActivityClientFrame::End);
        }
        if let Ok(gap) = serde_json::from_str::<ActivityGapFrame>(frame) {
            anyhow::ensure!(
                self.ended && gap.frame_type == "activity_gap" && gap.scope == "all_sessions",
                "invalid activity gap frame"
            );
            anyhow::ensure!(
                gap.after_sequence < gap.through_sequence
                    && gap.through_sequence - gap.after_sequence == gap.dropped
                    && if self.session_id.is_some() {
                        gap.after_sequence >= self.live_cursor
                    } else {
                        gap.after_sequence == self.live_cursor
                    },
                "invalid activity gap range"
            );
            self.live_cursor = gap.through_sequence;
            return Ok(ActivityClientFrame::Gap {
                after_sequence: gap.after_sequence,
                through_sequence: gap.through_sequence,
                dropped: gap.dropped,
            });
        }
        Err(anyhow::anyhow!("invalid activity replay frame"))
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct ActivityReplay {
    pub(crate) generation: Uuid,
    pub(crate) snapshot_sequence: u64,
    pub(crate) history_truncated: bool,
    pub(crate) events: Vec<ActivityEvent>,
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

    if let Err(error) = maintain_session_metadata(&supervisor).await {
        eprintln!("session metadata retention maintenance skipped: {error:#}");
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
    let mut retention_maintenance = tokio::time::interval(RETENTION_MAINTENANCE_INTERVAL);
    retention_maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    retention_maintenance.tick().await;
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
            _ = retention_maintenance.tick() => {
                if let Err(error) = maintain_session_metadata(&supervisor).await {
                    eprintln!("session metadata retention maintenance skipped: {error:#}");
                }
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
    ensure_supervisor_for_start().await?;
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
    ensure_supervisor_for_start().await?;
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
    println!("SESSION\tSTATUS\tPID\tPERMISSION\tCWD");
    for session in sessions {
        let pid = session
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "{}\t{}\t{}\t{}\t{}",
            session.session_id,
            session.status,
            pid,
            session.permission_mode.as_str(),
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

pub async fn forget(session_id: String) -> Result<()> {
    let result = request(ControlRequest::Forget { session_id }).await?;
    print_json(&result)
}

pub async fn gc(apply: bool, limit: usize) -> Result<()> {
    let result = request(ControlRequest::Gc { apply, limit }).await?;
    print_json(&result)
}

pub async fn restart(session_id: String) -> Result<()> {
    let result = request(ControlRequest::Restart {
        session_id,
        environment: CapturedStartEnvironment::capture(),
        public: false,
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
            permission_mode: Some(config::PermissionMode::Ask),
            yolo: false,
        },
        crate::cli::SessionPermissionCommand::Agent => ControlRequest::PermissionMode {
            session_id,
            permission_mode: Some(config::PermissionMode::Agent),
            yolo: false,
        },
        crate::cli::SessionPermissionCommand::Yolo => ControlRequest::PermissionMode {
            session_id,
            permission_mode: Some(config::PermissionMode::Yolo),
            yolo: true,
        },
        crate::cli::SessionPermissionCommand::Allow { path } => {
            ControlRequest::PermissionAllow { session_id, path }
        }
        crate::cli::SessionPermissionCommand::Revoke { path } => {
            ControlRequest::PermissionRevoke { session_id, path }
        }
        crate::cli::SessionPermissionCommand::Grant { request } => {
            ControlRequest::PermissionGrant {
                session_id,
                request,
            }
        }
        crate::cli::SessionPermissionCommand::Ungrant { request } => {
            ControlRequest::PermissionUngrant {
                session_id,
                request,
            }
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
    let (line, buffered_input) =
        tokio::time::timeout(CONTROL_READ_TIMEOUT, read_control_request(&mut stream))
            .await
            .context("timed out waiting for supervisor control request")??;
    let request: ControlRequest = serde_json::from_str(line.trim())
        .map_err(|_| anyhow::anyhow!("invalid control request"))?;

    match request {
        ControlRequest::AttachConsole => {
            handle_console_attachment(stream, console_registration).await
        }
        ControlRequest::AttachActivity(_) if buffered_input => Ok(()),
        ControlRequest::AttachActivity(request) => {
            let broker = supervisor.activity_broker();
            handle_activity_attachment(stream, broker, request).await
        }
        ControlRequest::Upgrade {
            executable,
            installed_locator,
            target_version,
            environment,
            dry_run,
            force,
            expected_sessions,
        } => {
            handle_upgrade_request(
                stream,
                supervisor,
                UpgradeControlRequest {
                    executable,
                    installed_locator,
                    target_version,
                    environment,
                    dry_run,
                    force,
                    expected_sessions,
                },
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
            "boot_generation": crate::boot_identity::generation(),
            "pid": std::process::id(),
            "control_protocol": CONTROL_PROTOCOL_VERSION,
            "lifecycle_schema": LIFECYCLE_SCHEMA_VERSION,
            "upgrade_plan_schema": UPGRADE_PLAN_SCHEMA_VERSION,
            "roots_configured": supervisor.roots_configured(),
            "named_roots": supervisor.named_root_names(),
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
        ControlRequest::List => Ok(serde_json::to_value(list_session_views(supervisor).await?)?),
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
        ControlRequest::Forget { session_id } => Ok(serde_json::to_value(
            supervisor.forget_session(&session_id).await?,
        )?),
        ControlRequest::Gc { apply, limit } => Ok(serde_json::to_value(
            supervisor.gc_session_metadata(!apply, limit).await?,
        )?),
        ControlRequest::Restart {
            session_id,
            environment,
            public,
        } => {
            environment.validate()?;
            restart_session(supervisor, &session_id, environment, public).await?;
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::RestartPolicy { session_id, policy } => {
            supervisor.set_restart_policy(&session_id, &policy).await?;
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::PermissionStatus { session_id } => {
            Ok(serde_json::to_value(inspect_session(&session_id).await?)?)
        }
        ControlRequest::PermissionMode {
            session_id,
            permission_mode,
            yolo,
        } => {
            let permission_mode =
                permission_mode.unwrap_or_else(|| config::PermissionMode::from_legacy_yolo(yolo));
            supervisor
                .set_permission_mode(&session_id, permission_mode)
                .await?;
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
        ControlRequest::PermissionGrant {
            session_id,
            request,
        } => {
            let applied = supervisor.apply_grants(&session_id, request).await?;
            Ok(json!({
                "grants": applied,
                "session": inspect_session(&session_id).await?,
            }))
        }
        ControlRequest::PermissionUngrant {
            session_id,
            request,
        } => {
            let removed = supervisor.revoke_grants(&session_id, request).await?;
            Ok(json!({
                "grants": removed,
                "session": inspect_session(&session_id).await?,
            }))
        }
        ControlRequest::ValidatePublicUpgradeSession { session_id } => {
            let session = supervisor
                .validate_public_upgrade_session(&session_id)
                .await?;
            Ok(json!({
                "session_id": session.id,
                "permission_mode": session.permission_mode,
                "status": "active"
            }))
        }
        ControlRequest::Upgrade { .. } => unreachable!("handled before dispatch"),
        ControlRequest::AttachConsole => unreachable!("handled before dispatch"),
        ControlRequest::AttachActivity(_) => unreachable!("handled before dispatch"),
    }
}

async fn handle_activity_attachment(
    stream: UnixStream,
    broker: Arc<ActivityBroker>,
    request: AttachActivityRequest,
) -> Result<()> {
    if request.schema_version != ACTIVITY_SCHEMA_VERSION {
        return write_activity_attach_error(stream, "unsupported activity schema").await;
    }
    let mut subscription =
        match broker.subscribe_snapshot(request.session_id.as_deref(), request.tail) {
            Ok(subscription) => subscription,
            Err(error) => {
                return write_activity_attach_error(
                    stream,
                    &format!("activity attachment unavailable: {error}"),
                )
                .await;
            }
        };
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let response = encode_line(&json!({
        "ok": true,
        "result": {
            "control_protocol": CONTROL_PROTOCOL_VERSION,
            "activity_schema": ACTIVITY_SCHEMA_VERSION,
            "generation": subscription.generation(),
            "snapshot_sequence": subscription.cutoff(),
            "replayed": subscription.replayed(),
            "history_truncated": subscription.history_truncated(),
        },
        "error": Value::Null,
    }))?;
    if !write_activity_bytes(&mut reader, &mut writer, &response).await? {
        return Ok(());
    }

    for event in subscription.snapshot() {
        let mut line = encode_event(event)?;
        line.push(b'\n');
        if !write_activity_bytes(&mut reader, &mut writer, &line).await? {
            return Ok(());
        }
    }
    let end = encode_line(&json!({
        "type": "activity_end",
        "snapshot_sequence": subscription.cutoff(),
        "history_truncated": subscription.history_truncated(),
    }))?;
    if !write_activity_bytes(&mut reader, &mut writer, &end).await? || !request.follow {
        let _ = writer.shutdown().await;
        return Ok(());
    }

    loop {
        let mut byte = [0_u8; 1];
        let delivery = tokio::select! {
            biased;
            input = reader.read(&mut byte) => {
                input.context("failed to monitor activity attachment input")?;
                return Ok(());
            }
            delivery = subscription.recv() => delivery?,
        };
        let line = match delivery {
            ActivityDelivery::Event(event) => {
                let mut line = encode_event(&event)?;
                line.push(b'\n');
                line
            }
            ActivityDelivery::Gap(gap) => encode_line(&json!({
                "type": "activity_gap",
                "scope": "all_sessions",
                "after_sequence": gap.after_sequence(),
                "through_sequence": gap.through_sequence(),
                "dropped": gap.dropped(),
            }))?,
        };
        if !write_activity_bytes(&mut reader, &mut writer, &line).await? {
            return Ok(());
        }
    }
}

async fn write_activity_attach_error(stream: UnixStream, message: &str) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let response = encode_line(&json!({
        "ok": false,
        "result": Value::Null,
        "error": message,
    }))?;
    let _ = write_activity_bytes(&mut reader, &mut writer, &response).await?;
    let _ = writer.shutdown().await;
    Ok(())
}

async fn write_activity_bytes<R, W>(reader: &mut R, writer: &mut W, bytes: &[u8]) -> Result<bool>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut byte = [0_u8; 1];
    tokio::select! {
        biased;
        input = reader.read(&mut byte) => {
            input.context("failed to monitor activity attachment input")?;
            Ok(false)
        }
        written = tokio::time::timeout(CONTROL_WRITE_TIMEOUT, writer.write_all(bytes)) => {
            written.context("timed out writing activity attachment")??;
            Ok(true)
        }
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
    let (path, capabilities) = inspect_upgrade_executable(path)?;
    anyhow::ensure!(
        capabilities.version == claimed_version,
        "upgrade executable version changed during preflight: expected {claimed_version}, found {}",
        capabilities.version
    );
    Ok((path, capabilities))
}

fn inspect_upgrade_executable(path: &Path) -> Result<(PathBuf, SupervisorCapabilities)> {
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
    use std::os::unix::fs::MetadataExt;
    anyhow::ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "upgrade executable is not owned by the current user"
    );
    anyhow::ensure!(
        mode & 0o022 == 0,
        "upgrade executable is group/world writable"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_UPGRADE_EXECUTABLE_BYTES,
        "upgrade executable exceeds bounded identity size"
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

/// Bounded classification of the sandbox helper bundled next to the upgrade
/// executable. The running supervisor keeps serving sessions until the
/// handoff exec, so a replacement helper that rejects the running policy
/// schema would degrade ordinary sandbox commands during that window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelperGeneration {
    Compatible,
    Incompatible,
    Unavailable,
}

#[cfg(target_os = "linux")]
fn classify_helper_generation(executable: &Path) -> HelperGeneration {
    use temote_mcp::sandbox::linux::helper_sibling_of;
    use temote_mcp::sandbox::linux::policy::LINUX_SANDBOX_POLICY_VERSION;

    #[derive(Deserialize)]
    struct HelperCapabilities {
        policy_schema: u64,
    }

    let reported = (|| -> Result<u64> {
        let helper = helper_sibling_of(executable)
            .context("sandbox helper is missing next to the upgrade executable")?;
        let helper = std::fs::canonicalize(&helper)?;
        let metadata = std::fs::metadata(&helper)?;
        anyhow::ensure!(metadata.is_file(), "sandbox helper is not a regular file");
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(mode & 0o111 != 0, "sandbox helper is not executable");
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "sandbox helper is not owned by the current user"
        );
        anyhow::ensure!(mode & 0o022 == 0, "sandbox helper is group/world writable");
        anyhow::ensure!(
            metadata.len() <= MAX_UPGRADE_EXECUTABLE_BYTES,
            "sandbox helper exceeds bounded identity size"
        );
        let output = std::process::Command::new(&helper)
            .arg("--capabilities")
            .output()
            .context("failed to inspect sandbox helper capabilities")?;
        anyhow::ensure!(
            output.status.success(),
            "sandbox helper did not report capabilities successfully"
        );
        anyhow::ensure!(
            output.stdout.len() <= 64 * 1024,
            "sandbox helper capability response is too large"
        );
        let capabilities: HelperCapabilities = serde_json::from_slice(&output.stdout)
            .context("invalid capability response from sandbox helper")?;
        Ok(capabilities.policy_schema)
    })();

    match reported {
        Ok(schema) if schema == u64::from(LINUX_SANDBOX_POLICY_VERSION) => {
            HelperGeneration::Compatible
        }
        Ok(_) => HelperGeneration::Incompatible,
        Err(_) => HelperGeneration::Unavailable,
    }
}

#[cfg(not(target_os = "linux"))]
fn classify_helper_generation(_executable: &Path) -> HelperGeneration {
    HelperGeneration::Compatible
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

struct UpgradeControlRequest {
    executable: PathBuf,
    installed_locator: Option<PathBuf>,
    target_version: String,
    environment: CapturedStartEnvironment,
    dry_run: bool,
    force: bool,
    expected_sessions: Option<Vec<crate::upgrade_transaction::UpgradePlannedSession>>,
}

async fn handle_upgrade_request(
    mut stream: UnixStream,
    supervisor: Arc<SessionSupervisor>,
    request: UpgradeControlRequest,
) -> Result<()> {
    let UpgradeControlRequest {
        executable,
        installed_locator,
        target_version,
        environment,
        dry_run,
        force,
        expected_sessions,
    } = request;
    let executable_preflight = (|| -> Result<(PathBuf, SupervisorCapabilities)> {
        environment.validate()?;
        validate_upgrade_executable(&executable, &target_version)
    })();
    let (executable, capabilities) = match executable_preflight {
        Ok(value) => value,
        Err(error) => {
            write_control_error(&mut stream, &error).await?;
            return Ok(());
        }
    };

    let helper_generation = classify_helper_generation(&executable);

    if dry_run {
        let preview = supervisor
            .preview_upgrade_plan(
                &target_version,
                capabilities.control_protocol,
                capabilities.lifecycle_schema,
                &environment,
                force,
            )
            .await;
        let preview = match preview {
            Ok(preview) => preview,
            Err(error) => {
                write_control_error(&mut stream, &error).await?;
                return Ok(());
            }
        };
        let mut result = match serde_json::to_value(&preview) {
            Ok(result) => result,
            Err(error) => {
                write_control_error(&mut stream, &anyhow::Error::from(error)).await?;
                return Ok(());
            }
        };
        if let Value::Object(ref mut fields) = result {
            fields.insert("helper_generation".to_owned(), json!(helper_generation));
        }
        let response = json!({"ok": true, "result": result, "error": Value::Null});
        stream.write_all(&encode_line(&response)?).await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    match helper_generation {
        HelperGeneration::Compatible => {}
        HelperGeneration::Incompatible => {
            write_control_error(
                &mut stream,
                &anyhow::anyhow!(
                    "bundled Linux sandbox helper reports a policy schema incompatible with the running supervisor; refusing upgrade handoff"
                ),
            )
            .await?;
            return Ok(());
        }
        HelperGeneration::Unavailable => {
            write_control_error(
                &mut stream,
                &anyhow::anyhow!(
                    "bundled Linux sandbox helper is missing or could not be inspected; refusing upgrade handoff"
                ),
            )
            .await?;
            return Ok(());
        }
    }

    let preflight: Result<SupervisorUpgradePlan> = async {
        let plan = supervisor
            .build_upgrade_plan_with_expected(
                crate::supervisor::SupervisorUpgradePlanRequest::new(
                    &target_version,
                    capabilities.control_protocol,
                    capabilities.lifecycle_schema,
                    &environment,
                    force,
                ),
                true,
                expected_sessions.as_deref(),
            )
            .await?;
        Ok(plan)
    }
    .await;
    let plan = match preflight {
        Ok(value) => value,
        Err(error) => {
            write_control_error(&mut stream, &error).await?;
            return Ok(());
        }
    };

    if !plan.handoff_required {
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
        let rollback = supervisor.rollback_upgrade(&plan, true).await;
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
        let rollback = supervisor.rollback_upgrade(&plan, false).await;
        let _ = remove_upgrade_plan(&plan_path);
        return match rollback {
            Ok(()) => Err(error)
                .context("supervisor upgrade drain failed; replacement startup was blocked"),
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
    if let Some(locator) = installed_locator {
        command.env(INTERNAL_INSTALLED_LOCATOR_ENV, locator);
    }
    let exec_error = command.exec();
    #[cfg(target_os = "linux")]
    drop(_exec_credential_handoff);

    let rollback = supervisor.rollback_upgrade(&plan, true).await;
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

#[derive(Clone, Debug)]
pub struct InstalledUpgradeExecutable {
    path: PathBuf,
    execution: Arc<UpgradeExecutionBinding>,
    digest: [u8; 32],
    pub target_version: String,
}

#[derive(Debug)]
struct UpgradeExecutionBinding {
    directory: PathBuf,
    path: PathBuf,
    file: std::fs::File,
}

struct PendingUpgradeSnapshot {
    directory: PathBuf,
    path: PathBuf,
    armed: bool,
}

impl Drop for PendingUpgradeSnapshot {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir(&self.directory);
        }
    }
}

impl Drop for UpgradeExecutionBinding {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

impl InstalledUpgradeExecutable {
    pub(crate) fn digest_hex(&self) -> String {
        self.digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub(crate) fn execution_fd(&self) -> i32 {
        self.execution.file.as_raw_fd()
    }

    pub(crate) fn execution_path(&self) -> &Path {
        &self.execution.path
    }

    pub(crate) fn installed_locator(&self) -> &Path {
        &self.path
    }
}

fn create_upgrade_execution_snapshot(
    source: &mut std::fs::File,
) -> Result<UpgradeExecutionBinding> {
    let directory = std::env::temp_dir()
        .join(format!("temote-mcp-upgrade-candidates-{}", unsafe {
            libc::geteuid()
        }));
    match std::fs::create_dir(&directory) {
        Ok(()) => std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("cannot create upgrade candidate directory"),
    }
    let directory_metadata = std::fs::symlink_metadata(&directory)?;
    use std::os::unix::fs::MetadataExt;
    anyhow::ensure!(
        directory_metadata.is_dir()
            && directory_metadata.uid() == unsafe { libc::geteuid() }
            && directory_metadata.permissions().mode() & 0o077 == 0,
        "upgrade candidate directory is not private to the current user"
    );
    let snapshot_directory = directory.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir(&snapshot_directory)
        .context("cannot create private upgrade execution snapshot directory")?;
    std::fs::set_permissions(&snapshot_directory, std::fs::Permissions::from_mode(0o700))?;
    let path = snapshot_directory.join("temote-mcp");
    let mut cleanup = PendingUpgradeSnapshot {
        directory: snapshot_directory.clone(),
        path: path.clone(),
        armed: true,
    };
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .context("cannot create private upgrade execution snapshot")?;
    source.seek(std::io::SeekFrom::Start(0))?;
    let copied = std::io::copy(
        &mut source.take(MAX_UPGRADE_EXECUTABLE_BYTES + 1),
        &mut file,
    )?;
    anyhow::ensure!(
        copied <= MAX_UPGRADE_EXECUTABLE_BYTES,
        "upgrade executable exceeds bounded identity size"
    );
    file.sync_all()?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500))?;
    // Linux rejects exec while any process has the image open for writing.
    // Retain only a read descriptor once the private snapshot is complete.
    drop(file);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .context("cannot reopen private upgrade execution snapshot")?;
    file.seek(std::io::SeekFrom::Start(0))?;
    cleanup.armed = false;
    Ok(UpgradeExecutionBinding {
        directory: snapshot_directory,
        path,
        file,
    })
}

fn bounded_upgrade_executable_digest(file: &mut std::fs::File) -> Result<[u8; 32]> {
    file.seek(std::io::SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied += read as u64;
        anyhow::ensure!(
            copied <= MAX_UPGRADE_EXECUTABLE_BYTES,
            "upgrade executable exceeds bounded identity size"
        );
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

pub fn capture_installed_upgrade_executable() -> Result<InstalledUpgradeExecutable> {
    initialize_installed_upgrade_locator()?;
    let locator = INSTALLED_UPGRADE_LOCATOR
        .get()
        .context("installed Temote startup locator is unavailable")?;
    let path =
        std::fs::canonicalize(locator).context("cannot resolve installed Temote executable")?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .context("cannot open installed Temote executable")?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "upgrade executable is not a regular file"
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(mode & 0o111 != 0, "upgrade executable is not executable");
    use std::os::unix::fs::MetadataExt;
    anyhow::ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "upgrade executable is not owned by the current user"
    );
    anyhow::ensure!(
        mode & 0o022 == 0,
        "upgrade executable is group/world writable"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_UPGRADE_EXECUTABLE_BYTES,
        "upgrade executable exceeds bounded identity size"
    );
    let digest = bounded_upgrade_executable_digest(&mut file)?;
    let mut execution = create_upgrade_execution_snapshot(&mut file)?;
    let snapshot_digest = bounded_upgrade_executable_digest(&mut execution.file)?;
    anyhow::ensure!(
        snapshot_digest == digest,
        "upgrade execution snapshot identity mismatch"
    );
    let mut command = std::process::Command::new(&execution.path);
    command.args(["supervisor", "--capabilities"]);
    let output = command
        .output()
        .context("failed to inspect installed Temote capabilities")?;
    anyhow::ensure!(
        output.status.success() && output.stdout.len() <= 64 * 1024,
        "installed Temote executable did not report bounded capabilities"
    );
    let capabilities: SupervisorCapabilities = serde_json::from_slice(&output.stdout)
        .context("invalid installed Temote capability response")?;
    anyhow::ensure!(
        capabilities.control_protocol == CONTROL_PROTOCOL_VERSION
            && capabilities.lifecycle_schema == LIFECYCLE_SCHEMA_VERSION
            && capabilities.upgrade_plan_schema == UPGRADE_PLAN_SCHEMA_VERSION,
        "installed Temote executable is incompatible with the running lifecycle protocol"
    );
    let target_version = capabilities.version;
    Ok(InstalledUpgradeExecutable {
        path,
        execution: Arc::new(execution),
        digest,
        target_version,
    })
}

pub fn revalidate_installed_upgrade_executable(
    approved: &InstalledUpgradeExecutable,
) -> Result<PathBuf> {
    let current = capture_installed_upgrade_executable()?;
    anyhow::ensure!(
        current.path == approved.path
            && current.digest == approved.digest
            && current.target_version == approved.target_version,
        "installed Temote executable changed after approval"
    );
    Ok(approved.execution_path().to_owned())
}

#[derive(Clone, Debug, Serialize)]
pub struct RemoteUpgradePreflight {
    pub source_version: String,
    pub target_version: String,
    pub compatible: bool,
    pub supervisor_handoff_required: bool,
    pub planned_session_count: usize,
    pub blocked_session_count: usize,
    pub blocker_reasons: Vec<&'static str>,
    pub direct_ingress_action: String,
    pub direct_ingress_blocked: bool,
    pub reconnect_expected: bool,
    pub plugin_reconciliation_required: bool,
    pub client_restart_required_if_plugin_replaced: bool,
    pub helper_generation: HelperGeneration,
    #[serde(skip)]
    pub(crate) planned_sessions: Vec<crate::upgrade_transaction::UpgradePlannedSession>,
}

pub async fn remote_upgrade_preflight(
    executable: &InstalledUpgradeExecutable,
) -> Result<RemoteUpgradePreflight> {
    upgrade_preflight_with_force(executable, false).await
}

fn validate_running_supervisor_upgrade_capabilities(ping: &Value) -> Result<()> {
    anyhow::ensure!(
        ping.get("control_protocol").and_then(Value::as_u64) == Some(CONTROL_PROTOCOL_VERSION),
        "running supervisor control protocol is incompatible; manual supervisor restart is required"
    );
    anyhow::ensure!(
        ping.get("lifecycle_schema").and_then(Value::as_u64) == Some(LIFECYCLE_SCHEMA_VERSION),
        "running supervisor lifecycle schema is incompatible; manual supervisor restart is required"
    );
    anyhow::ensure!(
        ping.get("upgrade_plan_schema").and_then(Value::as_u64)
            == Some(UPGRADE_PLAN_SCHEMA_VERSION),
        "running supervisor upgrade plan schema is incompatible; manual supervisor restart is required"
    );
    Ok(())
}

async fn upgrade_preflight_with_force(
    executable: &InstalledUpgradeExecutable,
    force: bool,
) -> Result<RemoteUpgradePreflight> {
    let ping = upgrade_request(ControlRequest::Ping).await?;
    validate_running_supervisor_upgrade_capabilities(&ping)?;
    let source_version = ping
        .get("version")
        .and_then(Value::as_str)
        .context("running supervisor did not report its version")?
        .to_owned();
    let preview_value = upgrade_request(ControlRequest::Upgrade {
        executable: executable.execution_path().to_owned(),
        installed_locator: Some(executable.path.clone()),
        target_version: executable.target_version.clone(),
        environment: CapturedStartEnvironment::capture(),
        dry_run: true,
        force,
        expected_sessions: None,
    })
    .await?;
    let helper_generation = preview_value
        .get("helper_generation")
        .cloned()
        .and_then(|value| serde_json::from_value::<HelperGeneration>(value).ok())
        .unwrap_or(HelperGeneration::Unavailable);
    let preview: crate::supervisor::SupervisorUpgradePreview =
        serde_json::from_value(preview_value).context("invalid supervisor upgrade preview")?;
    #[cfg(all(feature = "network", unix))]
    let ingress =
        crate::lifecycle::prepare_direct_ingress_upgrade(&executable.target_version).await?;
    #[cfg(all(feature = "network", unix))]
    let (direct_ingress_action, direct_ingress_blocked, reconnect_expected) = (
        ingress.plan().action.clone(),
        ingress.blocker().is_some(),
        ingress.plan().action == "restart",
    );
    #[cfg(not(all(feature = "network", unix)))]
    let (direct_ingress_action, direct_ingress_blocked, reconnect_expected) =
        ("unavailable".to_owned(), false, false);
    let planned_sessions = preview
        .active_sessions
        .iter()
        .map(
            |session| crate::upgrade_transaction::UpgradePlannedSession {
                session_id: session.session_id.clone(),
                source_process_id: session.process_id,
                source_started_at: session.started_at,
            },
        )
        .collect::<Vec<_>>();
    Ok(RemoteUpgradePreflight {
        source_version,
        target_version: executable.target_version.clone(),
        compatible: true,
        supervisor_handoff_required: preview.plan.handoff_required,
        planned_session_count: planned_sessions.len(),
        blocked_session_count: preview.blocked_sessions.len(),
        blocker_reasons: preview
            .blocked_sessions
            .iter()
            .map(|_| "session_not_restorable")
            .collect(),
        direct_ingress_action,
        direct_ingress_blocked,
        reconnect_expected,
        plugin_reconciliation_required: true,
        client_restart_required_if_plugin_replaced: true,
        helper_generation,
        planned_sessions,
    })
}

pub(crate) async fn verify_planned_upgrade_sessions(
    planned: &[crate::upgrade_transaction::UpgradePlannedSession],
    require_source_instance: bool,
) -> Result<usize> {
    let active = request_upgrade_session_views()
        .await?
        .into_iter()
        .filter(|view| view.status == "active")
        .map(|view| crate::upgrade_transaction::UpgradePlannedSession {
            session_id: view.session_id,
            source_process_id: view.process_id,
            source_started_at: view.started_at,
        })
        .collect::<Vec<_>>();
    validate_planned_upgrade_session_identities(planned, &active, require_source_instance)?;
    Ok(active.len())
}

fn validate_planned_upgrade_session_identities(
    planned: &[crate::upgrade_transaction::UpgradePlannedSession],
    active: &[crate::upgrade_transaction::UpgradePlannedSession],
    require_source_instance: bool,
) -> Result<()> {
    anyhow::ensure!(
        active.len() == planned.len(),
        "active session set changed after upgrade approval"
    );
    let active_by_id = active
        .iter()
        .map(|identity| (identity.session_id.as_str(), identity))
        .collect::<std::collections::BTreeMap<_, _>>();
    for expected in planned {
        let current = active_by_id
            .get(expected.session_id.as_str())
            .with_context(|| "approved session set changed")?;
        if require_source_instance {
            anyhow::ensure!(
                current.source_process_id == expected.source_process_id
                    && current.source_started_at == expected.source_started_at,
                "approved session instance changed"
            );
        }
    }
    Ok(())
}

pub async fn apply_supervisor_upgrade(
    executable: &Path,
    installed_locator: &Path,
    target_version: &str,
    force: bool,
    expected_sessions: Option<&[crate::upgrade_transaction::UpgradePlannedSession]>,
) -> Result<usize> {
    let ping = upgrade_request(ControlRequest::Ping).await?;
    validate_running_supervisor_upgrade_capabilities(&ping)?;
    let source_version = ping
        .get("version")
        .and_then(Value::as_str)
        .context("running supervisor did not report its version")?
        .to_owned();
    let source_pid = ping.get("pid").and_then(Value::as_u64).unwrap_or_default();
    let source_boot_generation = ping
        .get("boot_generation")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let result = upgrade_request(ControlRequest::Upgrade {
        executable: executable.to_owned(),
        installed_locator: Some(installed_locator.to_owned()),
        target_version: target_version.to_owned(),
        environment: CapturedStartEnvironment::capture(),
        dry_run: false,
        force,
        expected_sessions: expected_sessions.map(|sessions| sessions.to_vec()),
    })
    .await?;
    let plan_value = result
        .get("plan")
        .cloned()
        .unwrap_or_else(|| result.clone());
    let plan: SupervisorUpgradePlan = serde_json::from_value(plan_value)?;
    let restore_plan_path = result
        .get("restore_plan_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    if plan.handoff_required {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(path) = restore_plan_path.as_deref()
                && let Some(report) = read_upgrade_failure_report(path)?
            {
                anyhow::ensure!(
                    report.source_version == source_version
                        && report.target_version == target_version,
                    "supervisor upgrade failure report identity mismatch"
                );
                anyhow::bail!(format_upgrade_failure_report(&report));
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "supervisor handoff verification timed out"
            );
            if let Ok(status) = upgrade_request_until(ControlRequest::Ping, deadline).await
                && supervisor_handoff_identity_changed(
                    &status,
                    target_version,
                    source_pid,
                    source_boot_generation.as_deref(),
                )
            {
                let mut all_active = true;
                for session in &plan.sessions {
                    let active = upgrade_request_until(
                        ControlRequest::Info {
                            session_id: session.session_id.clone(),
                        },
                        deadline,
                    )
                    .await
                    .ok()
                    .and_then(|value| {
                        value
                            .get("status")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                    if active.as_deref() != Some("active") {
                        all_active = false;
                        break;
                    }
                }
                if all_active {
                    return Ok(plan.sessions.len());
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Ok(plan.sessions.len())
}

fn supervisor_handoff_identity_changed(
    status: &Value,
    target_version: &str,
    source_pid: u64,
    source_boot_generation: Option<&str>,
) -> bool {
    if status.get("version").and_then(Value::as_str) != Some(target_version)
        || status.get("pid").and_then(Value::as_u64) != Some(source_pid)
    {
        return false;
    }
    let Some(target_boot_generation) = status.get("boot_generation").and_then(Value::as_str) else {
        return false;
    };
    !target_boot_generation.is_empty()
        && source_boot_generation.is_none_or(|source| source != target_boot_generation)
}

pub fn reconcile_codex_plugin(executable: &Path, installed_locator: &Path) -> Result<()> {
    let output = codex_plugin_reconcile_command(executable, installed_locator)
        .output()
        .context("Codex plugin reconciliation could not start")?;
    anyhow::ensure!(
        output.status.success(),
        "Codex plugin reconciliation failed"
    );
    Ok(())
}

fn codex_plugin_reconcile_command(
    executable: &Path,
    installed_locator: &Path,
) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    command
        .env(INTERNAL_INSTALLED_LOCATOR_ENV, installed_locator)
        .args(["codex", "plugin", "install"]);
    command
}

pub async fn upgrade(dry_run: bool, force: bool) -> Result<()> {
    let executable = capture_installed_upgrade_executable()?;
    let preflight = upgrade_preflight_with_force(&executable, force).await?;
    if dry_run {
        println!("{}", serde_json::to_string_pretty(&preflight)?);
        return Ok(());
    }
    let _admission = crate::upgrade_transaction::acquire_admission_lock()?;
    ensure_no_remote_upgrade_owns_runtime(&crate::upgrade_transaction::load_transactions()?)?;
    anyhow::ensure!(
        preflight.blocked_session_count == 0,
        "upgrade is blocked by {} session(s)",
        preflight.blocked_session_count
    );
    anyhow::ensure!(
        !preflight.direct_ingress_blocked,
        "direct ingress upgrade is blocked"
    );
    anyhow::ensure!(
        preflight.helper_generation == HelperGeneration::Compatible,
        "sandbox helper generation is not compatible with the running supervisor"
    );
    let executable_path = revalidate_installed_upgrade_executable(&executable)?;
    let restored = apply_supervisor_upgrade(
        &executable_path,
        executable.installed_locator(),
        &executable.target_version,
        force,
        None,
    )
    .await?;
    #[cfg(all(feature = "network", unix))]
    {
        let executable_path = revalidate_installed_upgrade_executable(&executable)?;
        let ingress =
            crate::lifecycle::prepare_direct_ingress_upgrade(&executable.target_version).await?;
        crate::lifecycle::apply_direct_ingress_upgrade(
            ingress,
            &executable_path,
            executable.installed_locator(),
        )
        .await?;
    }
    let executable_path = revalidate_installed_upgrade_executable(&executable)?;
    if let Err(error) = reconcile_codex_plugin(&executable_path, executable.installed_locator()) {
        eprintln!("{error:#}; run `temote-mcp codex plugin install` manually");
    }
    println!(
        "Temote upgrade complete: {} -> {}; restored {restored} session(s)",
        preflight.source_version, executable.target_version
    );
    Ok(())
}

fn ensure_no_remote_upgrade_owns_runtime(
    transactions: &[crate::upgrade_transaction::UpgradeTransaction],
) -> Result<()> {
    if let Some(active) = crate::upgrade_transaction::active_transactions(transactions).first() {
        anyhow::bail!(
            "upgrade transaction {} already owns the runtime",
            active.transaction_id
        );
    }
    Ok(())
}

async fn restart_session(
    supervisor: &Arc<SessionSupervisor>,
    session_id: &str,
    environment: CapturedStartEnvironment,
    public: bool,
) -> Result<()> {
    supervisor
        .restart_with_environment(session_id, environment, public)
        .await
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
        let mut event = json!({
            "type": "approval",
            "session_id": prompt.session_id,
            "id": prompt.request.id,
            "cwd": prompt.request.cwd,
            "operation": prompt.request.operation,
            "detail": prompt.request.detail,
        });
        if !prompt.request.metadata.is_empty() {
            event["metadata"] = serde_json::to_value(&prompt.request.metadata)?;
        }
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
    let path = config::supervisor_socket_path()?;
    request_at_path(&path, request).await
}

async fn request_at_path(path: &Path, request: ControlRequest) -> Result<Value> {
    let mut stream = connect_supervisor_at(path).await?;
    stream.write_all(&encode_line(&request)?).await?;
    stream.shutdown().await?;
    let mut reader = BufReader::new(stream);
    let line = read_line_limited(&mut reader, "supervisor response").await?;
    let response: ControlResponse =
        serde_json::from_str(line.trim()).context("invalid supervisor response")?;
    ensure_response_ok(response)
}

#[allow(dead_code)]
pub(crate) async fn activity_replay(
    session_id: Option<String>,
    tail: usize,
) -> Result<ActivityReplay> {
    let stream = tokio::time::timeout(CONTROL_READ_TIMEOUT, connect_supervisor())
        .await
        .context("timed out connecting to session supervisor")??;
    activity_replay_on_stream(stream, session_id, tail).await
}

pub async fn run_activity_command(
    session_id: Option<String>,
    tail: usize,
    follow: bool,
) -> Result<()> {
    match run_activity(session_id, tail, follow).await {
        Ok(()) => Ok(()),
        Err(error) => {
            report_activity_command_error(&error.to_string()).await;
            std::process::exit(1);
        }
    }
}

async fn report_activity_command_error(error: &str) {
    let Ok((sender, mut result)) =
        spawn_activity_writer(libc::STDERR_FILENO, 1, "temote-activity-final-error")
    else {
        return;
    };
    if queue_activity_line(&sender, format!("Error: {error}"), "activity diagnostics").is_err() {
        return;
    }
    drop(sender);
    let _ = tokio::time::timeout(Duration::from_millis(100), &mut result).await;
}

async fn run_activity(session_id: Option<String>, tail: usize, follow: bool) -> Result<()> {
    if let Some(session_id) = session_id.as_deref() {
        config::validate_session_id(session_id)?;
    }
    let stream = tokio::time::timeout(CONTROL_READ_TIMEOUT, connect_supervisor())
        .await
        .context("timed out connecting to session supervisor")??;
    let mut connection = ActivityClientConnection::attach(stream, session_id, tail, follow).await?;
    let (output, mut output_result) = spawn_activity_writer(
        libc::STDOUT_FILENO,
        MAX_ACTIVITY_OUTPUT_QUEUE,
        "temote-activity-output",
    )?;
    let (diagnostics, diagnostics_result) = spawn_activity_writer(
        libc::STDERR_FILENO,
        MAX_ACTIVITY_DIAGNOSTIC_QUEUE,
        "temote-activity-diagnostics",
    )?;
    let mut diagnostics = Some(diagnostics);
    let mut diagnostics_result = Some(diagnostics_result);
    queue_activity_diagnostic(
        &diagnostics,
        "Attached to best-effort recent activity; this view does not guarantee current state.",
    )?;
    if connection.state.attach.history_truncated {
        queue_activity_diagnostic(
            &diagnostics,
            "Retained activity history was truncated before this replay.",
        )?;
    }
    let stdin_monitor = if follow {
        ActivityStdinMonitor::start()?
    } else {
        None
    };
    let mut stdin_eof = stdin_monitor.as_ref().map(ActivityStdinMonitor::subscribe);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut cancelled = false;
    loop {
        enum ActivityLoopEvent {
            Cancelled,
            Stdin(Result<()>),
            Output(Result<ActivityOutputStatus, tokio::sync::oneshot::error::RecvError>),
            Diagnostics(Result<ActivityOutputStatus, tokio::sync::oneshot::error::RecvError>),
            Frame(Result<Option<ActivityClientFrame>>),
        }
        let event = if follow {
            tokio::select! {
                biased;
                signal = &mut ctrl_c => {
                    signal.context("failed to receive Ctrl-C")?;
                    ActivityLoopEvent::Cancelled
                }
                stdin = wait_for_tty_eof(&mut stdin_eof) => ActivityLoopEvent::Stdin(stdin),
                status = &mut output_result => ActivityLoopEvent::Output(status),
                status = wait_for_activity_diagnostics(&mut diagnostics_result) => ActivityLoopEvent::Diagnostics(status),
                frame = connection.next_frame() => ActivityLoopEvent::Frame(frame),
            }
        } else {
            tokio::select! {
                biased;
                signal = &mut ctrl_c => {
                    signal.context("failed to receive Ctrl-C")?;
                    ActivityLoopEvent::Cancelled
                }
                status = &mut output_result => ActivityLoopEvent::Output(status),
                status = wait_for_activity_diagnostics(&mut diagnostics_result) => ActivityLoopEvent::Diagnostics(status),
                frame = connection.next_frame() => ActivityLoopEvent::Frame(frame),
            }
        };
        let frame = match event {
            ActivityLoopEvent::Cancelled => {
                cancelled = true;
                break;
            }
            ActivityLoopEvent::Stdin(status) => {
                status?;
                cancelled = true;
                break;
            }
            ActivityLoopEvent::Output(status) => return finish_early_activity_output(status),
            ActivityLoopEvent::Diagnostics(status) => match status {
                Ok(ActivityOutputStatus::Complete | ActivityOutputStatus::BrokenPipe) => {
                    diagnostics = None;
                    diagnostics_result = None;
                    continue;
                }
                Ok(ActivityOutputStatus::Failed) | Err(_) => {
                    return Err(anyhow::anyhow!("activity diagnostics failed"));
                }
            },
            ActivityLoopEvent::Frame(frame) => frame?,
        };
        let Some(frame) = frame else {
            if follow {
                queue_activity_diagnostic(
                    &diagnostics,
                    "Activity stream disconnected; rerun `temote-mcp activity` to reconnect.",
                )?;
                break;
            }
            return Err(anyhow::anyhow!("activity stream ended before activity_end"));
        };
        match frame {
            ActivityClientFrame::Event(event) => {
                let timestamp = format_local_activity_timestamp(event.timestamp_ms())?;
                let line = render_event(&event, &timestamp)?;
                match output.try_send(line) {
                    Ok(()) => {}
                    Err(std_mpsc::TrySendError::Full(_)) => {
                        return Err(anyhow::anyhow!("activity output queue is full"));
                    }
                    Err(std_mpsc::TrySendError::Disconnected(_)) => {
                        return finish_early_activity_output((&mut output_result).await);
                    }
                }
            }
            ActivityClientFrame::End => {
                if connection.state.attach.replayed == 0 {
                    queue_activity_diagnostic(
                        &diagnostics,
                        "No matching recent activity was retained.",
                    )?;
                }
                if !follow {
                    break;
                }
            }
            ActivityClientFrame::Gap {
                after_sequence,
                through_sequence,
                dropped,
            } => queue_activity_diagnostic(
                &diagnostics,
                &format!(
                    "Activity gap across all sessions after sequence {after_sequence} through {through_sequence} ({dropped} events); rerun to replay retained history."
                ),
            )?,
        }
    }
    drop(stdin_monitor);
    drop(output);
    drop(diagnostics);
    let drain = tokio::time::timeout(ACTIVITY_OUTPUT_TIMEOUT, async {
        let output = (&mut output_result).await;
        let diagnostics = match diagnostics_result.as_mut() {
            Some(receiver) => Some(receiver.await),
            None => None,
        };
        (output, diagnostics)
    });
    tokio::pin!(drain);
    if cancelled {
        let _ = (&mut drain).await;
        return Ok(());
    }
    let drained = tokio::select! {
        biased;
        signal = &mut ctrl_c => {
            signal.context("failed to receive Ctrl-C")?;
            cancelled = true;
            None
        }
        drained = &mut drain => Some(drained),
    };
    if cancelled {
        return Ok(());
    }
    match drained.expect("activity drain result missing without cancellation") {
        Ok((output_status, diagnostics_status)) => {
            let output_status = normalize_activity_output_status(output_status)?;
            if output_status == ActivityOutputStatus::BrokenPipe {
                return Ok(());
            }
            if let Some(status) = diagnostics_status {
                match normalize_activity_output_status(status)? {
                    ActivityOutputStatus::Complete | ActivityOutputStatus::BrokenPipe => {}
                    ActivityOutputStatus::Failed => {
                        return Err(anyhow::anyhow!("activity diagnostics failed"));
                    }
                }
            }
            match output_status {
                ActivityOutputStatus::Complete => Ok(()),
                ActivityOutputStatus::BrokenPipe => Ok(()),
                ActivityOutputStatus::Failed => Err(anyhow::anyhow!("activity output failed")),
            }
        }
        Err(_) => Err(anyhow::anyhow!("activity output drain timed out")),
    }
}

async fn wait_for_activity_diagnostics(
    receiver: &mut Option<tokio::sync::oneshot::Receiver<ActivityOutputStatus>>,
) -> Result<ActivityOutputStatus, tokio::sync::oneshot::error::RecvError> {
    match receiver {
        Some(receiver) => receiver.await,
        None => std::future::pending().await,
    }
}

async fn wait_for_tty_eof(
    receiver: &mut Option<tokio::sync::oneshot::Receiver<ActivityStdinStatus>>,
) -> Result<()> {
    match receiver {
        Some(receiver) => match receiver.await {
            Ok(ActivityStdinStatus::Eof) => Ok(()),
            Ok(ActivityStdinStatus::Failed) | Err(_) => {
                Err(anyhow::anyhow!("activity stdin monitor failed"))
            }
        },
        None => std::future::pending().await,
    }
}

fn finish_early_activity_output(
    status: Result<ActivityOutputStatus, tokio::sync::oneshot::error::RecvError>,
) -> Result<()> {
    match status {
        Ok(ActivityOutputStatus::Complete | ActivityOutputStatus::BrokenPipe) => Ok(()),
        Ok(ActivityOutputStatus::Failed) | Err(_) => Err(anyhow::anyhow!("activity output failed")),
    }
}

fn normalize_activity_output_status(
    status: Result<ActivityOutputStatus, tokio::sync::oneshot::error::RecvError>,
) -> Result<ActivityOutputStatus> {
    status.map_err(|_| anyhow::anyhow!("activity output failed"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivityOutputStatus {
    Complete,
    BrokenPipe,
    Failed,
}

fn spawn_activity_writer(
    target_fd: RawFd,
    capacity: usize,
    thread_name: &'static str,
) -> Result<(
    std_mpsc::SyncSender<String>,
    tokio::sync::oneshot::Receiver<ActivityOutputStatus>,
)> {
    let fd = unsafe { libc::dup(target_fd) };
    if fd < 0 {
        return Err(anyhow::anyhow!("activity output is unavailable"));
    }
    let (sender, receiver) = std_mpsc::sync_channel::<String>(capacity);
    let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
    let result_sender = Arc::new(std::sync::Mutex::new(Some(result_sender)));
    let (watchdog_sender, watchdog_receiver) = std_mpsc::channel();
    let watchdog_result = Arc::clone(&result_sender);
    std::thread::Builder::new()
        .name(format!("{thread_name}-watchdog"))
        .spawn(move || {
            while watchdog_receiver.recv().is_ok() {
                match watchdog_receiver.recv_timeout(ACTIVITY_OUTPUT_TIMEOUT) {
                    Ok(()) => {}
                    Err(std_mpsc::RecvTimeoutError::Timeout) => {
                        send_activity_output_status(&watchdog_result, ActivityOutputStatus::Failed);
                        return;
                    }
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .map_err(|_| anyhow::anyhow!("activity output is unavailable"))?;
    let output = unsafe { std::fs::File::from_raw_fd(fd) };
    let writer_result = Arc::clone(&result_sender);
    std::thread::Builder::new()
        .name(thread_name.to_owned())
        .spawn(move || {
            let fd = output.as_raw_fd();
            let mut status = ActivityOutputStatus::Complete;
            while let Ok(mut line) = receiver.recv() {
                line.push('\n');
                if watchdog_sender.send(()).is_err() {
                    status = ActivityOutputStatus::Failed;
                    break;
                }
                status = write_activity_output_line(fd, line.as_bytes(), ACTIVITY_OUTPUT_TIMEOUT);
                let _ = watchdog_sender.send(());
                if status != ActivityOutputStatus::Complete {
                    break;
                }
            }
            send_activity_output_status(&writer_result, status);
        })
        .map_err(|_| anyhow::anyhow!("activity output is unavailable"))?;
    Ok((sender, result_receiver))
}

fn send_activity_output_status(
    sender: &std::sync::Mutex<Option<tokio::sync::oneshot::Sender<ActivityOutputStatus>>>,
    status: ActivityOutputStatus,
) {
    if let Some(sender) = sender
        .lock()
        .expect("activity output status lock poisoned")
        .take()
    {
        let _ = sender.send(status);
    }
}

fn queue_activity_line(
    sender: &std_mpsc::SyncSender<String>,
    line: String,
    label: &str,
) -> Result<()> {
    match sender.try_send(line) {
        Ok(()) => Ok(()),
        Err(std_mpsc::TrySendError::Full(_)) => Err(anyhow::anyhow!("{label} queue is full")),
        Err(std_mpsc::TrySendError::Disconnected(_)) => {
            Err(anyhow::anyhow!("{label} is unavailable"))
        }
    }
}

fn queue_activity_diagnostic(
    sender: &Option<std_mpsc::SyncSender<String>>,
    line: &str,
) -> Result<()> {
    match sender {
        Some(sender) => queue_activity_line(sender, line.to_owned(), "activity diagnostics"),
        None => Ok(()),
    }
}

fn write_activity_output_line(fd: RawFd, bytes: &[u8], timeout: Duration) -> ActivityOutputStatus {
    let deadline = std::time::Instant::now() + timeout;
    let mut written = 0;
    while written < bytes.len() {
        let now = std::time::Instant::now();
        if now >= deadline {
            return ActivityOutputStatus::Failed;
        }
        let remaining = deadline.duration_since(now);
        let timeout_ms = remaining.as_millis().clamp(1, libc::c_int::MAX as u128) as libc::c_int;
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if polled == 0 {
            return ActivityOutputStatus::Failed;
        }
        if polled < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return ActivityOutputStatus::Failed;
        }
        let count =
            unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return ActivityOutputStatus::BrokenPipe;
            }
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return ActivityOutputStatus::Failed;
        }
        if count == 0 {
            return ActivityOutputStatus::Failed;
        }
        written += count as usize;
    }
    ActivityOutputStatus::Complete
}

struct ActivityStdinMonitor {
    stop: Arc<AtomicBool>,
    eof: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<ActivityStdinStatus>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivityStdinStatus {
    Eof,
    Failed,
}

impl ActivityStdinMonitor {
    fn start() -> Result<Option<Self>> {
        if !should_monitor_activity_stdin(true, std::io::stdin().is_terminal()) {
            return Ok(None);
        }
        let fd = unsafe { libc::dup(libc::STDIN_FILENO) };
        if fd < 0 {
            return Err(anyhow::anyhow!("activity stdin monitor is unavailable"));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (eof_sender, eof_receiver) = tokio::sync::oneshot::channel();
        let input = unsafe { std::fs::File::from_raw_fd(fd) };
        std::thread::Builder::new()
            .name("temote-activity-stdin".to_owned())
            .spawn(move || {
                let fd = input.as_raw_fd();
                let mut byte = [0_u8; 1];
                while !thread_stop.load(Ordering::Acquire) {
                    let mut poll_fd = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let polled = unsafe { libc::poll(&mut poll_fd, 1, 250) };
                    if polled < 0 {
                        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                        {
                            continue;
                        }
                        let _ = eof_sender.send(ActivityStdinStatus::Failed);
                        return;
                    }
                    if polled == 0 {
                        continue;
                    }
                    let read = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
                    if read == 0 {
                        let _ = eof_sender.send(ActivityStdinStatus::Eof);
                        return;
                    }
                    if read < 0
                        && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                    {
                        let _ = eof_sender.send(ActivityStdinStatus::Failed);
                        return;
                    }
                }
            })
            .map_err(|_| anyhow::anyhow!("activity stdin monitor is unavailable"))?;
        Ok(Some(Self {
            stop,
            eof: std::sync::Mutex::new(Some(eof_receiver)),
        }))
    }

    fn subscribe(&self) -> tokio::sync::oneshot::Receiver<ActivityStdinStatus> {
        self.eof
            .lock()
            .expect("activity stdin monitor lock poisoned")
            .take()
            .unwrap_or_else(|| {
                let (_sender, receiver) = tokio::sync::oneshot::channel();
                receiver
            })
    }
}

fn should_monitor_activity_stdin(follow: bool, is_terminal: bool) -> bool {
    follow && is_terminal
}

impl Drop for ActivityStdinMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn format_local_activity_timestamp(timestamp_ms: u64) -> Result<String> {
    let seconds: libc::time_t = (timestamp_ms / 1000)
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid activity timestamp"))?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    let converted = unsafe { libc::localtime_r(&seconds, local.as_mut_ptr()) };
    if converted.is_null() {
        return Err(anyhow::anyhow!("invalid activity timestamp"));
    }
    let local = unsafe { local.assume_init() };
    let offset = local.tm_gmtoff;
    let sign = if offset < 0 { '-' } else { '+' };
    let absolute_offset = offset.unsigned_abs();
    let offset_hours = absolute_offset / 3600;
    let offset_minutes = (absolute_offset % 3600) / 60;
    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {sign}{offset_hours:02}:{offset_minutes:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec,
        timestamp_ms % 1000,
    ))
}

async fn activity_replay_on_stream(
    stream: UnixStream,
    session_id: Option<String>,
    tail: usize,
) -> Result<ActivityReplay> {
    let mut connection = ActivityClientConnection::attach(stream, session_id, tail, false).await?;
    let mut events = Vec::with_capacity(connection.state.attach.replayed);
    loop {
        match connection.next_frame().await? {
            Some(ActivityClientFrame::Event(event)) => events.push(event),
            Some(ActivityClientFrame::End) => {
                return Ok(ActivityReplay {
                    generation: connection.state.attach.generation,
                    snapshot_sequence: connection.state.attach.snapshot_sequence,
                    history_truncated: connection.state.attach.history_truncated,
                    events,
                });
            }
            Some(ActivityClientFrame::Gap { .. }) => {
                return Err(anyhow::anyhow!("activity gap arrived before replay end"));
            }
            None => return Err(anyhow::anyhow!("activity stream ended before activity_end")),
        }
    }
}

fn decode_activity_attach_response(response: &str, tail: usize) -> Result<ActivityAttachResult> {
    let response: ActivityAttachResponse = serde_json::from_str(response.trim_end_matches('\n'))
        .map_err(|_| anyhow::anyhow!("invalid activity attach response"))?;
    if !response.ok {
        anyhow::ensure!(
            response.result.is_none() && response.error.is_some(),
            "invalid activity attach response"
        );
        return Err(anyhow::anyhow!("activity attachment rejected"));
    }
    anyhow::ensure!(response.error.is_none(), "invalid activity attach response");
    let attach = response
        .result
        .context("invalid activity attach response")?;
    anyhow::ensure!(
        attach.control_protocol == CONTROL_PROTOCOL_VERSION,
        "unsupported supervisor control protocol"
    );
    anyhow::ensure!(
        attach.activity_schema == ACTIVITY_SCHEMA_VERSION,
        "unsupported activity schema"
    );
    anyhow::ensure!(attach.replayed <= tail, "invalid activity replay count");
    Ok(attach)
}

async fn read_activity_client_line<R>(reader: &mut R, label: &str) -> Result<String>
where
    R: AsyncBufReadExt + Unpin,
{
    tokio::time::timeout(CONTROL_READ_TIMEOUT, read_line_limited(reader, label))
        .await
        .with_context(|| format!("timed out waiting for {label}"))?
}

async fn upgrade_request(request: ControlRequest) -> Result<Value> {
    upgrade_request_until(
        request,
        tokio::time::Instant::now() + UPGRADE_CONTROL_RPC_TIMEOUT,
    )
    .await
}

async fn upgrade_request_until(
    request: ControlRequest,
    deadline: tokio::time::Instant,
) -> Result<Value> {
    let path = config::supervisor_socket_path()?;
    upgrade_request_at_path_until(&path, request, deadline).await
}

async fn upgrade_request_at_path_until(
    path: &Path,
    request: ControlRequest,
    deadline: tokio::time::Instant,
) -> Result<Value> {
    tokio::time::timeout_at(deadline, request_at_path(path, request))
        .await
        .map_err(|_| anyhow::anyhow!("supervisor upgrade control request timed out"))?
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
    connect_supervisor_at(&path).await
}

async fn connect_supervisor_at(path: &Path) -> Result<UnixStream> {
    UnixStream::connect(path).await.with_context(|| {
        format!(
            "Temote session supervisor is not running at {}; run `temote-mcp supervisor` first",
            path.display()
        )
    })
}

async fn ensure_supervisor_for_start() -> Result<()> {
    let path = config::supervisor_socket_path()?;
    match UnixStream::connect(&path).await {
        Ok(stream) => {
            drop(stream);
            return Ok(());
        }
        Err(error) if supervisor_socket_unavailable(&error) => {}
        Err(error) => {
            return Err(error).context("failed to connect to the Temote session supervisor");
        }
    }

    let executable = std::env::current_exe().context("cannot locate the Temote executable")?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("supervisor")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("failed to start the Temote session supervisor automatically")?;

    let deadline = Instant::now() + SUPERVISOR_BOOTSTRAP_TIMEOUT;
    let mut bootstrap_exit = None;
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                drop(stream);
                return Ok(());
            }
            Err(error) if supervisor_socket_unavailable(&error) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("automatic Temote supervisor startup failed");
            }
        }

        if bootstrap_exit.is_none() {
            bootstrap_exit = child
                .try_wait()
                .context("failed to inspect automatic Temote supervisor startup")?;
        }
        if Instant::now() >= deadline {
            if let Some(status) = bootstrap_exit {
                anyhow::bail!(
                    "automatic Temote supervisor startup exited before becoming ready ({status})"
                );
            }
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("automatic Temote supervisor startup did not become ready");
        }
        tokio::time::sleep(SUPERVISOR_BOOTSTRAP_POLL).await;
    }
}

fn supervisor_socket_unavailable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
    )
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

/// Status for a session whose durable metadata is intact but whose canonical
/// workspace paths (cwd and permitted roots) no longer all resolve. The view
/// keeps the stored identity, lifecycle, and liveness facts instead of
/// relabeling them as stopped, crashed, or active.
const SESSION_STATUS_DEGRADED: &str = "degraded";
const SESSION_WORKSPACE_DEGRADED: &str = "session workspace is missing or not resolvable";
const SESSION_LIVENESS_UNKNOWN: &str = "session liveness could not be determined safely";

pub(crate) async fn inspect_session(id: &str) -> Result<SessionView> {
    build_session_view(id, true).await
}

/// Builds one bounded session view.
///
/// `reconcile_lifecycle` matches the session_info/reporting behavior that
/// persists a crash when durable metadata claims a live runtime whose socket
/// is not active. Read-only enumeration passes `false` and never writes.
async fn build_session_view(id: &str, reconcile_lifecycle: bool) -> Result<SessionView> {
    let metadata = config::read_session_metadata_for_view(id).await?;
    let workspace_resolved = metadata.workspace_resolved;
    let session = metadata.session;
    let mut lifecycle = config::read_session_lifecycle(id).await?;
    let liveness = config::session_is_active(id).await;

    let claims_live_runtime = lifecycle.as_ref().map_or(session.process_id != 0, |state| {
        matches!(
            state.status,
            LifecycleStatus::Starting | LifecycleStatus::Active | LifecycleStatus::Stopping
        )
    });
    if reconcile_lifecycle && matches!(liveness, Ok(false)) && claims_live_runtime {
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

    let (status, last_error) = if !workspace_resolved {
        let last_error = match inferred.last_error.as_deref() {
            Some(existing) => {
                format!("{SESSION_WORKSPACE_DEGRADED}; last durable error: {existing}")
            }
            None => SESSION_WORKSPACE_DEGRADED.to_owned(),
        };
        (SESSION_STATUS_DEGRADED.to_owned(), Some(last_error))
    } else {
        match &liveness {
            Ok(true) => {
                let status = match inferred.status {
                    LifecycleStatus::Starting => "starting",
                    LifecycleStatus::Stopping => "stopping",
                    _ => "active",
                };
                (status.to_owned(), inferred.last_error.clone())
            }
            Ok(false) if !reconcile_lifecycle && claims_live_runtime => (
                "unknown".to_owned(),
                Some(
                    inferred
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "session socket is not active".to_owned()),
                ),
            ),
            Ok(false) => (
                status_name(inferred.status).to_owned(),
                inferred.last_error.clone(),
            ),
            Err(_) => (
                "unknown".to_owned(),
                Some(SESSION_LIVENESS_UNKNOWN.to_owned()),
            ),
        }
    };
    let pid = if workspace_resolved {
        matches!(status.as_str(), "starting" | "active" | "stopping")
            .then_some(session.process_id)
            .filter(|pid| *pid != 0)
    } else {
        // A missing workspace says nothing about liveness: keep the process
        // identity only when the socket probe actually observed a live session.
        liveness
            .as_ref()
            .is_ok_and(|live| *live)
            .then_some(session.process_id)
            .filter(|pid| *pid != 0)
    };
    let permission_mode = session.permission_mode;
    let yolo = session.yolo();
    let grants = session.grants.clone();
    // Workspace identity is derived from the canonical cwd and the configured
    // `src` named root; an unresolvable workspace reports none.
    let workspace = if workspace_resolved {
        crate::managed_worktree::inspect_session_workspace(
            &session.cwd,
            crate::managed_worktree::configured_src_root_from_env().as_deref(),
        )
    } else {
        None
    };

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
        permission_mode,
        yolo,
        grants,
        logical_path: inferred.logical_path,
        workspace,
        restart_policy: inferred.restart_policy,
        restart_count: inferred.restart_count,
        last_restart_at: inferred.last_restart_at,
        next_restart_at: inferred.next_restart_at,
        restart_limit_reason: inferred.restart_limit_reason,
    })
}

async fn list_session_views(supervisor: &SessionSupervisor) -> Result<Vec<SessionView>> {
    let owned_ids = supervisor.owned_session_ids().await;
    let owned = owned_ids.iter().cloned().collect::<HashSet<_>>();
    let mut sessions = Vec::new();

    for id in owned_ids {
        let session = inspect_session_read_only(&id)
            .await
            .with_context(|| format!("failed to inspect supervisor-owned session {id}"))?;
        push_control_session_view(&mut sessions, session, true)?;
    }

    let history_ids = bounded_history_session_ids(&owned).await?;
    let mut history = Vec::new();
    for id in history_ids {
        // Bounded history stays limited to sessions whose workspace still
        // resolves so accumulated removed worktrees cannot crowd out healthy
        // entries. Stale metadata is neither deleted nor forgotten; owned
        // sessions are always surfaced, degraded when their cwd is gone.
        if let Ok(session) = inspect_session_read_only(&id).await
            && session.status != SESSION_STATUS_DEGRADED
        {
            history.push(session);
        }
    }
    sort_history_views(&mut history);
    for session in history {
        if !push_control_session_view(&mut sessions, session, false)? {
            break;
        }
    }
    Ok(sessions)
}

pub(crate) async fn request_session_views() -> Result<Vec<SessionView>> {
    let result = request(ControlRequest::List).await?;
    serde_json::from_value(result).context("invalid supervisor session list response")
}

async fn request_upgrade_session_views() -> Result<Vec<SessionView>> {
    let result = upgrade_request(ControlRequest::List).await?;
    serde_json::from_value(result).context("invalid supervisor session list response")
}

pub(crate) async fn session_views_for_mcp() -> Result<Vec<SessionView>> {
    if supervisor_is_available().await? {
        request_session_views().await
    } else {
        filesystem_session_views_read_only().await
    }
}

async fn supervisor_is_available() -> Result<bool> {
    let path = config::supervisor_socket_path()?;
    match UnixStream::connect(path).await {
        Ok(_) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionAborted
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error).context("failed to inspect supervisor availability"),
    }
}

async fn filesystem_session_views_read_only() -> Result<Vec<SessionView>> {
    let ids = bounded_history_session_ids(&HashSet::new()).await?;
    let mut sessions = Vec::new();
    for id in ids {
        if let Ok(session) = inspect_session_read_only(&id).await
            && session.status != SESSION_STATUS_DEGRADED
        {
            sessions.push(session);
        }
    }
    sort_history_views(&mut sessions);
    let mut bounded = Vec::new();
    for session in sessions {
        if !push_control_session_view(&mut bounded, session, false)? {
            break;
        }
    }
    Ok(bounded)
}

async fn bounded_history_session_ids(excluded: &HashSet<String>) -> Result<Vec<String>> {
    collect_bounded_history_session_ids(
        excluded,
        MAX_SESSION_HISTORY_DIRECTORY_ENTRIES_SCANNED,
        MAX_SESSION_HISTORY_CANDIDATES,
    )
    .await
}

/// Collects bounded history candidates, excluding entries whose workspace no
/// longer resolves before the candidate bound is applied.
///
/// Filtering after the bound would let accumulated removed worktrees consume
/// the whole candidate budget and push healthy history out of the list. Stale
/// metadata is neither deleted nor forgotten here; it stays addressable through
/// `session_info` and explicit lifecycle commands.
async fn collect_bounded_history_session_ids(
    excluded: &HashSet<String>,
    max_scanned_entries: usize,
    max_candidates: usize,
) -> Result<Vec<String>> {
    let directory = config::sessions_dir()?;
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("failed to read session metadata directory"),
    };
    let mut scanned = 0usize;
    let mut ids = BTreeSet::new();
    while scanned < max_scanned_entries {
        let Some(entry) = entries.next_entry().await? else {
            break;
        };
        scanned += 1;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if excluded.contains(id) || config::validate_session_id(id).is_err() {
            continue;
        }
        match config::read_session_metadata_for_view(id).await {
            Ok(metadata) if metadata.workspace_resolved => {}
            _ => continue,
        }
        ids.insert(id.to_owned());
        if ids.len() > max_candidates {
            let last = ids.iter().next_back().cloned();
            if let Some(last) = last {
                ids.remove(&last);
            }
        }
    }
    Ok(ids.into_iter().collect())
}

fn sort_history_views(sessions: &mut [SessionView]) {
    sessions.sort_by(|left, right| {
        right
            .stopped_at
            .unwrap_or_default()
            .cmp(&left.stopped_at.unwrap_or_default())
            .then_with(|| right.started_at.cmp(&left.started_at))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
}

fn push_control_session_view(
    sessions: &mut Vec<SessionView>,
    session: SessionView,
    required: bool,
) -> Result<bool> {
    if sessions.len() >= MAX_SESSION_LIST_ENTRIES {
        anyhow::ensure!(
            !required,
            "active session list exceeds {MAX_SESSION_LIST_ENTRIES} entries"
        );
        return Ok(false);
    }
    sessions.push(session);
    let fits = serde_json::to_vec(sessions)?.len() <= MAX_SESSION_CONTROL_LIST_BYTES;
    if fits {
        return Ok(true);
    }
    sessions.pop();
    anyhow::ensure!(
        !required,
        "active session list exceeds {MAX_SESSION_CONTROL_LIST_BYTES} bytes"
    );
    Ok(false)
}

pub(crate) async fn inspect_session_read_only(id: &str) -> Result<SessionView> {
    build_session_view(id, false).await
}

pub(crate) async fn session_metadata_diagnostics() -> Result<SessionMetadataDiagnostics> {
    let plan = build_retention_plan(&HashSet::new()).await?;
    Ok(plan.diagnostics)
}

async fn maintain_session_metadata(
    supervisor: &SessionSupervisor,
) -> Result<SessionMetadataDiagnostics> {
    let owned = supervisor
        .owned_session_ids()
        .await
        .into_iter()
        .collect::<HashSet<_>>();
    let plan = build_retention_plan(&owned).await?;
    for candidate in &plan.prune_candidates {
        if retention_candidate_still_safe(supervisor, candidate).await? {
            prune_terminal_metadata_pair(&candidate.id).await?;
        }
    }
    Ok(plan.diagnostics)
}

async fn build_retention_plan(owned: &HashSet<String>) -> Result<RetentionPlan> {
    let protected = protected_upgrade_session_ids()?;
    let directory = config::sessions_dir()?;
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RetentionPlan {
                diagnostics: SessionMetadataDiagnostics::default(),
                prune_candidates: Vec::new(),
            });
        }
        Err(error) => return Err(error).context("failed to read session metadata directory"),
    };
    let mut diagnostics = SessionMetadataDiagnostics::default();
    let mut pairs: HashMap<String, (bool, bool)> = HashMap::new();
    while let Some(entry) = entries.next_entry().await? {
        diagnostics.total_entries = diagnostics.total_entries.saturating_add(1);
        let path = entry.path();
        let extension = path.extension().and_then(|value| value.to_str());
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            diagnostics.other_entries = diagnostics.other_entries.saturating_add(1);
            continue;
        };
        match extension {
            Some("json") => {
                diagnostics.json_entries = diagnostics.json_entries.saturating_add(1);
                pairs.entry(id.to_owned()).or_default().0 = true;
            }
            Some("state") => {
                diagnostics.state_entries = diagnostics.state_entries.saturating_add(1);
                pairs.entry(id.to_owned()).or_default().1 = true;
            }
            _ => diagnostics.other_entries = diagnostics.other_entries.saturating_add(1),
        }
    }

    let mut terminal = Vec::new();
    for (id, (has_json, has_state)) in pairs {
        if !has_json || !has_state {
            diagnostics.invalid_orphan_count = diagnostics.invalid_orphan_count.saturating_add(1);
            match (has_json, has_state) {
                (false, true) => {
                    diagnostics.missing_json_count =
                        diagnostics.missing_json_count.saturating_add(1);
                }
                (true, false) => {
                    diagnostics.missing_state_count =
                        diagnostics.missing_state_count.saturating_add(1);
                }
                _ => {}
            }
            continue;
        }
        if config::validate_session_id(&id).is_err() {
            diagnostics.invalid_orphan_count = diagnostics.invalid_orphan_count.saturating_add(1);
            continue;
        }
        let session = match config::read_session_metadata(&id).await {
            Ok(session) if session.id == id => session,
            _ => {
                diagnostics.invalid_orphan_count =
                    diagnostics.invalid_orphan_count.saturating_add(1);
                continue;
            }
        };
        let lifecycle = match config::read_session_lifecycle(&id).await {
            Ok(Some(lifecycle)) => lifecycle,
            _ => {
                diagnostics.invalid_orphan_count =
                    diagnostics.invalid_orphan_count.saturating_add(1);
                continue;
            }
        };
        if !matches!(
            lifecycle.status,
            LifecycleStatus::Stopped | LifecycleStatus::Crashed
        ) {
            continue;
        }
        let Some(stopped_at) = lifecycle.stopped_at else {
            diagnostics.invalid_orphan_count = diagnostics.invalid_orphan_count.saturating_add(1);
            continue;
        };
        if owned.contains(&id) || protected.contains(&id) {
            diagnostics.retained_terminal_count =
                diagnostics.retained_terminal_count.saturating_add(1);
            continue;
        }
        match config::session_is_active(&id).await {
            Ok(false) => terminal.push(TerminalMetadataCandidate {
                id,
                started_at: lifecycle.started_at.max(session.started_at),
                stopped_at,
            }),
            Ok(true) | Err(_) => {
                diagnostics.retained_terminal_count =
                    diagnostics.retained_terminal_count.saturating_add(1);
            }
        }
    }
    terminal.sort_by(|left, right| {
        right
            .stopped_at
            .cmp(&left.stopped_at)
            .then_with(|| right.started_at.cmp(&left.started_at))
            .then_with(|| left.id.cmp(&right.id))
    });
    let split = terminal.len().min(TERMINAL_SESSION_RETENTION);
    diagnostics.retained_terminal_count = diagnostics.retained_terminal_count.saturating_add(split);
    let prune_candidates = terminal.split_off(split);
    diagnostics.safely_prunable_count = prune_candidates.len();
    Ok(RetentionPlan {
        diagnostics,
        prune_candidates,
    })
}

fn protected_upgrade_session_ids() -> Result<HashSet<String>> {
    let directory = upgrade_plan_directory()?;
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => return Err(error).context("failed to inspect supervisor restore plans"),
    };
    let mut protected = HashSet::new();
    for entry in entries {
        let entry = entry.context("failed to inspect supervisor restore plan entry")?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !name.starts_with("restore-")
            || !name.ends_with(".json")
            || name.ends_with(".failure.json")
        {
            continue;
        }
        let plan = read_upgrade_plan(&path)
            .context("a supervisor restore plan could not be read safely")?;
        for session in plan.sessions {
            config::validate_session_id(&session.session_id)?;
            protected.insert(session.session_id);
        }
    }
    Ok(protected)
}

async fn retention_candidate_still_safe(
    supervisor: &SessionSupervisor,
    candidate: &TerminalMetadataCandidate,
) -> Result<bool> {
    if supervisor
        .owned_session_ids()
        .await
        .iter()
        .any(|id| id == &candidate.id)
    {
        return Ok(false);
    }
    if protected_upgrade_session_ids()?.contains(&candidate.id) {
        return Ok(false);
    }
    let session = match config::read_session_metadata(&candidate.id).await {
        Ok(session) if session.id == candidate.id => session,
        _ => return Ok(false),
    };
    let lifecycle = match config::read_session_lifecycle(&candidate.id).await {
        Ok(Some(lifecycle)) => lifecycle,
        _ => return Ok(false),
    };
    if !matches!(
        lifecycle.status,
        LifecycleStatus::Stopped | LifecycleStatus::Crashed
    ) || lifecycle.stopped_at != Some(candidate.stopped_at)
        || lifecycle.started_at.max(session.started_at) != candidate.started_at
    {
        return Ok(false);
    }
    match config::session_is_active(&candidate.id).await {
        Ok(false) => Ok(true),
        Ok(true) | Err(_) => Ok(false),
    }
}

async fn prune_terminal_metadata_pair(id: &str) -> Result<()> {
    let state = config::session_lifecycle_path(id)?;
    let metadata = config::session_path(id)?;
    tokio::fs::remove_file(&state)
        .await
        .with_context(|| format!("failed to prune terminal lifecycle for {id}"))?;
    tokio::fs::remove_file(&metadata)
        .await
        .with_context(|| format!("failed to prune terminal metadata for {id}"))?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct SessionGcPresence {
    json: bool,
    state: bool,
    json_regular_secs: Option<u64>,
    state_regular_secs: Option<u64>,
}

/// Modification time of a regular file only; symlinks, directories, sockets,
/// and special files yield `None` so they are never GC candidates.
async fn regular_file_modified_secs(path: &Path) -> Result<Option<u64>> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
        }
    };
    if !metadata.file_type().is_file() {
        return Ok(None);
    }
    let modified = metadata
        .modified()
        .with_context(|| format!("cannot read modification time for {}", path.display()))?;
    let secs = match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_) => return Ok(Some(0)),
    };
    Ok(Some(secs))
}

/// Whether the lone half may be removed: lifecycle halves must be terminal,
/// metadata halves must be readable with a matching ID, and the session socket
/// must not answer a liveness probe.
async fn session_gc_orphan_is_safe(id: &str, reason: SessionGcReason) -> Result<bool> {
    match reason {
        SessionGcReason::MissingJson => {
            let lifecycle = match config::read_session_lifecycle(id).await {
                Ok(Some(lifecycle)) => lifecycle,
                _ => return Ok(false),
            };
            if !matches!(
                lifecycle.status,
                LifecycleStatus::Stopped | LifecycleStatus::Crashed
            ) || lifecycle.stopped_at.is_none()
            {
                return Ok(false);
            }
        }
        SessionGcReason::MissingState => {
            if config::read_session_metadata(id).await.is_err() {
                return Ok(false);
            }
        }
    }
    match config::session_is_active(id).await {
        Ok(false) => Ok(true),
        Ok(true) | Err(_) => Ok(false),
    }
}

/// Build the dry-run plan for the initial reviewed orphan classes
/// (`missing_json`, `missing_state`).
///
/// Only lone regular-file halves older than the grace period are eligible.
/// Live, supervisor-owned, upgrade-protected, symlinked, special-file,
/// ID-mismatched, and malformed entries are excluded. Ordering is deterministic
/// (oldest modification first, then ID) so the bounded limit is stable.
async fn build_session_gc_plan(owned: &HashSet<String>, limit: usize) -> Result<SessionGcPlan> {
    let protected = protected_upgrade_session_ids()?;
    let mut report = SessionGcReport {
        dry_run: true,
        grace_seconds: SESSION_ORPHAN_GRACE_SECONDS,
        limit,
        missing_json_orphans: 0,
        missing_state_orphans: 0,
        ineligible_orphans: 0,
        candidates: Vec::new(),
        removed: Vec::new(),
        skipped: Vec::new(),
        truncated: false,
    };
    let directory = config::sessions_dir()?;
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SessionGcPlan {
                report,
                candidates: Vec::new(),
            });
        }
        Err(error) => return Err(error).context("failed to read session metadata directory"),
    };
    let now = config::unix_time();
    let grace_cutoff = now.saturating_sub(SESSION_ORPHAN_GRACE_SECONDS);
    let mut presence: HashMap<String, SessionGcPresence> = HashMap::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let modified = regular_file_modified_secs(&path).await?;
        let slot = presence.entry(id.to_owned()).or_default();
        match path.extension().and_then(|value| value.to_str()) {
            Some("json") => {
                slot.json = true;
                slot.json_regular_secs = modified;
            }
            Some("state") => {
                slot.state = true;
                slot.state_regular_secs = modified;
            }
            _ => {}
        }
    }

    let mut candidates = Vec::new();
    for (id, presence) in presence {
        let reason = match (presence.json, presence.state) {
            (true, false) => SessionGcReason::MissingState,
            (false, true) => SessionGcReason::MissingJson,
            _ => continue,
        };
        match reason {
            SessionGcReason::MissingJson => {
                report.missing_json_orphans = report.missing_json_orphans.saturating_add(1);
            }
            SessionGcReason::MissingState => {
                report.missing_state_orphans = report.missing_state_orphans.saturating_add(1);
            }
        }
        let (modified_secs, kind) = match reason {
            SessionGcReason::MissingJson => (presence.state_regular_secs, "lifecycle"),
            SessionGcReason::MissingState => (presence.json_regular_secs, "metadata"),
        };
        let Some(modified_secs) = modified_secs else {
            report.ineligible_orphans = report.ineligible_orphans.saturating_add(1);
            continue;
        };
        if modified_secs > grace_cutoff {
            report.ineligible_orphans = report.ineligible_orphans.saturating_add(1);
            continue;
        }
        if config::validate_session_id(&id).is_err() {
            report.ineligible_orphans = report.ineligible_orphans.saturating_add(1);
            continue;
        }
        if owned.contains(&id) || protected.contains(&id) {
            report.ineligible_orphans = report.ineligible_orphans.saturating_add(1);
            continue;
        }
        if !session_gc_orphan_is_safe(&id, reason).await? {
            report.ineligible_orphans = report.ineligible_orphans.saturating_add(1);
            continue;
        }
        candidates.push(SessionGcCandidate {
            id,
            reason,
            modified_secs,
            kind,
        });
    }
    candidates.sort_by(|left, right| {
        left.modified_secs
            .cmp(&right.modified_secs)
            .then_with(|| left.id.cmp(&right.id))
    });
    report.truncated = candidates.len() > limit;
    let candidates = candidates.into_iter().take(limit).collect::<Vec<_>>();
    report.candidates = candidates
        .iter()
        .map(|candidate| SessionGcEntry {
            session_id: candidate.id.clone(),
            reason: candidate.reason,
        })
        .collect();
    Ok(SessionGcPlan { report, candidates })
}

/// Re-check one planned candidate immediately before deletion. Any drift (the
/// counterpart appeared, the file was replaced or touched, the session became
/// live or owned, a restore plan appeared) skips the candidate instead of
/// failing the whole run.
async fn revalidate_session_gc_candidate(
    owned: &HashSet<String>,
    candidate: &SessionGcCandidate,
) -> Result<bool> {
    if owned.contains(&candidate.id) {
        return Ok(false);
    }
    if protected_upgrade_session_ids()?.contains(&candidate.id) {
        return Ok(false);
    }
    let (orphan_path, counterpart_path) = match candidate.reason {
        SessionGcReason::MissingJson => (
            config::session_lifecycle_path(&candidate.id)?,
            config::session_path(&candidate.id)?,
        ),
        SessionGcReason::MissingState => (
            config::session_path(&candidate.id)?,
            config::session_lifecycle_path(&candidate.id)?,
        ),
    };
    match tokio::fs::symlink_metadata(&counterpart_path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => return Ok(false),
    }
    if regular_file_modified_secs(&orphan_path).await? != Some(candidate.modified_secs) {
        return Ok(false);
    }
    session_gc_orphan_is_safe(&candidate.id, candidate.reason).await
}

/// Apply a previously built plan under the caller's supervisor transition lock.
async fn apply_session_gc_plan(
    owned: &HashSet<String>,
    plan: &SessionGcPlan,
) -> Result<(Vec<SessionGcEntry>, Vec<SessionGcEntry>)> {
    let mut removed = Vec::new();
    let mut skipped = Vec::new();
    for candidate in &plan.candidates {
        let entry = SessionGcEntry {
            session_id: candidate.id.clone(),
            reason: candidate.reason,
        };
        if !revalidate_session_gc_candidate(owned, candidate).await? {
            skipped.push(entry);
            continue;
        }
        let path = match candidate.reason {
            SessionGcReason::MissingJson => config::session_lifecycle_path(&candidate.id)?,
            SessionGcReason::MissingState => config::session_path(&candidate.id)?,
        };
        if config::remove_owned_session_entry(&path, candidate.kind, false).await? {
            removed.push(entry);
        } else {
            skipped.push(entry);
        }
    }
    Ok((removed, skipped))
}

/// Build and optionally apply a session orphan GC plan.
pub(crate) async fn run_session_gc(
    owned: &HashSet<String>,
    dry_run: bool,
    limit: usize,
) -> Result<SessionGcReport> {
    anyhow::ensure!(
        (SESSION_GC_MIN_LIMIT..=MAX_SESSION_GC_LIMIT).contains(&limit),
        "session gc limit must be between {SESSION_GC_MIN_LIMIT} and {MAX_SESSION_GC_LIMIT}"
    );
    let plan = build_session_gc_plan(owned, limit).await?;
    let mut report = plan.report.clone();
    report.dry_run = dry_run;
    if !dry_run {
        let (removed, skipped) = apply_session_gc_plan(owned, &plan).await?;
        report.removed = removed;
        report.skipped = skipped;
    }
    Ok(report)
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

async fn read_control_request(stream: &mut UnixStream) -> Result<(String, bool)> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = (&mut reader)
        .take((MAX_CONTROL_MESSAGE_BYTES + 1) as u64)
        .read_line(&mut line)
        .await
        .context("failed to read supervisor control request")?;
    anyhow::ensure!(
        read > 0,
        "supervisor control request closed before a message"
    );
    anyhow::ensure!(
        read <= MAX_CONTROL_MESSAGE_BYTES,
        "supervisor control request exceeds {MAX_CONTROL_MESSAGE_BYTES} bytes"
    );
    Ok((line, !reader.buffer().is_empty()))
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
    use temote_mcp::activity::contract::{
        ActivityOperation, ActivityState, ActivitySummary, ActivityUpdate,
    };
    use temote_mcp::activity::history::{ActivityHistory, MAX_ACTIVITY_HISTORY_BYTES};
    use uuid::Uuid;

    const ACTIVITY_TEST_GENERATION: Uuid =
        Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0701);
    const ACTIVITY_TEST_INSTANCE: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0702);

    fn stalled_control_listener(
        socket_path: &Path,
    ) -> (tokio::task::JoinHandle<()>, Arc<AtomicBool>) {
        let listener = UnixListener::bind(socket_path).unwrap();
        let accepted = Arc::new(AtomicBool::new(false));
        let accepted_by_server = accepted.clone();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            accepted_by_server.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        });
        (server, accepted)
    }

    #[tokio::test]
    async fn upgrade_control_rpc_times_out_when_private_listener_stalls() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("control.sock");
        let (server, accepted) = stalled_control_listener(&socket_path);

        let error = upgrade_request_at_path_until(
            &socket_path,
            ControlRequest::Ping,
            tokio::time::Instant::now() + Duration::from_millis(200),
        )
        .await
        .unwrap_err();

        assert!(accepted.load(Ordering::SeqCst));
        assert!(error.to_string().contains("timed out"), "{error}");
        server.abort();
    }

    #[tokio::test]
    async fn upgrade_control_rpc_timeout_releases_admission_and_transaction_locks() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("control.sock");
        let (server, accepted) = stalled_control_listener(&socket_path);
        let transaction_id = uuid::Uuid::new_v4().to_string();

        let error = async {
            let _admission = crate::upgrade_transaction::acquire_admission_lock()?;
            let _transaction =
                crate::upgrade_transaction::acquire_transaction_lock(&transaction_id)?;
            upgrade_request_at_path_until(
                &socket_path,
                ControlRequest::List,
                tokio::time::Instant::now() + Duration::from_millis(200),
            )
            .await
        }
        .await
        .unwrap_err();

        assert!(accepted.load(Ordering::SeqCst));
        assert!(error.to_string().contains("timed out"), "{error}");
        let admission = crate::upgrade_transaction::acquire_admission_lock().unwrap();
        let transaction =
            crate::upgrade_transaction::acquire_transaction_lock(&transaction_id).unwrap();
        let transaction_lock_path = transaction.path().to_owned();
        drop(transaction);
        drop(admission);
        std::fs::remove_file(transaction_lock_path).unwrap();
        server.abort();
    }

    fn activity_test_broker(broadcast_capacity: usize) -> Arc<ActivityBroker> {
        Arc::new(
            ActivityBroker::with_limits(
                ActivityHistory::with_limits(32, MAX_ACTIVITY_HISTORY_BYTES).unwrap(),
                broadcast_capacity,
                16,
                || 1_780_000_000_000,
                ACTIVITY_TEST_GENERATION,
            )
            .unwrap(),
        )
    }

    fn activity_test_update(operation_id: u128) -> ActivityUpdate {
        ActivityUpdate::new(
            Uuid::from_u128(operation_id),
            ActivityOperation::ReadFile,
            ActivityState::Started,
            None,
            ActivitySummary::empty(),
        )
        .unwrap()
    }

    fn activity_test_frame(sequence: u64, session_id: Option<&str>) -> String {
        let mut frame = json!({
            "type": "activity",
            "event": {
                "schema_version": ACTIVITY_SCHEMA_VERSION,
                "sequence": sequence,
                "operation_id": Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_7000 + u128::from(sequence)),
                "timestamp_ms": 1_780_000_000_000_u64,
                "session_id": session_id,
                "session_instance": Value::Null,
                "operation": "read_file",
                "state": "started",
                "duration_ms": Value::Null,
                "safe_summary": "",
            }
        })
        .to_string();
        frame.push('\n');
        frame
    }

    async fn read_activity_test_json<R>(reader: &mut R) -> Value
    where
        R: AsyncBufReadExt + Unpin,
    {
        let line = tokio::time::timeout(
            Duration::from_secs(1),
            read_line_limited(reader, "activity test response"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!line.is_empty(), "activity response closed unexpectedly");
        serde_json::from_str(line.trim()).unwrap()
    }

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
        assert_eq!(
            result["boot_generation"],
            crate::boot_identity::generation()
        );
        assert_eq!(result["pid"], std::process::id());
        assert_eq!(result["control_protocol"], CONTROL_PROTOCOL_VERSION);
        assert_eq!(result["lifecycle_schema"], LIFECYCLE_SCHEMA_VERSION);
        assert_eq!(result["upgrade_plan_schema"], UPGRADE_PLAN_SCHEMA_VERSION);
        assert_eq!(result["roots_configured"], true);
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn activity_attachment_replays_filtered_tail_and_sends_one_end_marker() {
        let broker = activity_test_broker(8);
        broker
            .publish(
                activity_test_update(0x7001),
                Some("other"),
                Some(ACTIVITY_TEST_INSTANCE),
            )
            .unwrap();
        broker
            .publish(
                activity_test_update(0x7002),
                Some("target"),
                Some(ACTIVITY_TEST_INSTANCE),
            )
            .unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(handle_activity_attachment(
            server,
            Arc::clone(&broker),
            AttachActivityRequest {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                session_id: Some("target".to_owned()),
                tail: 100,
                follow: false,
            },
        ));
        let mut reader = BufReader::new(client);

        let response = read_activity_test_json(&mut reader).await;
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["control_protocol"], 2);
        assert_eq!(response["result"]["activity_schema"], 1);
        assert_eq!(
            response["result"]["generation"],
            ACTIVITY_TEST_GENERATION.to_string()
        );
        assert_eq!(response["result"]["snapshot_sequence"], 2);
        assert_eq!(response["result"]["replayed"], 1);
        assert_eq!(response["result"]["history_truncated"], false);
        let event = read_activity_test_json(&mut reader).await;
        assert_eq!(event["type"], "activity");
        assert_eq!(event["event"]["sequence"], 2);
        assert_eq!(event["event"]["session_id"], "target");
        let end = read_activity_test_json(&mut reader).await;
        assert_eq!(
            end,
            json!({
                "type": "activity_end",
                "snapshot_sequence": 2,
                "history_truncated": false,
            })
        );
        assert_eq!(
            read_line_limited(&mut reader, "activity eof")
                .await
                .unwrap(),
            ""
        );
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn activity_attachment_follows_live_events_and_any_input_detaches_only_viewer() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let broker = supervisor.activity_broker();
        let (server, client) = UnixStream::pair().unwrap();
        let (registration, _registrations) = mpsc::channel(1);
        let task = tokio::spawn(handle_control_connection(
            server,
            Arc::clone(&supervisor),
            registration,
        ));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        writer
            .write_all(
                &encode_line(&ControlRequest::AttachActivity(AttachActivityRequest {
                    schema_version: ACTIVITY_SCHEMA_VERSION,
                    session_id: None,
                    tail: 0,
                    follow: true,
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(read_activity_test_json(&mut reader).await["ok"], true);
        assert_eq!(
            read_activity_test_json(&mut reader).await["type"],
            "activity_end"
        );

        broker
            .publish(
                activity_test_update(0x7010),
                Some("target"),
                Some(ACTIVITY_TEST_INSTANCE),
            )
            .unwrap();
        let event = read_activity_test_json(&mut reader).await;
        assert_eq!(event["event"]["sequence"], 1);

        writer
            .write_all(b"{\"command\":\"stop\",\"session_id\":\"target\"}\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            read_line_limited(&mut reader, "activity eof")
                .await
                .unwrap(),
            ""
        );
        assert_eq!(broker.subscribe_snapshot(None, 0).unwrap().cutoff(), 1);
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn activity_attachment_reports_global_broadcast_gap_before_next_event() {
        let broker = activity_test_broker(2);
        let (server, client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(handle_activity_attachment(
            server,
            Arc::clone(&broker),
            AttachActivityRequest {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                session_id: None,
                tail: 0,
                follow: true,
            },
        ));
        let (reader, writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        assert_eq!(read_activity_test_json(&mut reader).await["ok"], true);
        assert_eq!(
            read_activity_test_json(&mut reader).await["type"],
            "activity_end"
        );

        for operation_id in 0x7020..0x7024 {
            broker
                .publish(
                    activity_test_update(operation_id),
                    Some("target"),
                    Some(ACTIVITY_TEST_INSTANCE),
                )
                .unwrap();
        }
        let gap = read_activity_test_json(&mut reader).await;
        assert_eq!(
            gap,
            json!({
                "type": "activity_gap",
                "scope": "all_sessions",
                "after_sequence": 0,
                "through_sequence": 2,
                "dropped": 2,
            })
        );
        assert_eq!(
            read_activity_test_json(&mut reader).await["event"]["sequence"],
            3
        );
        drop(writer);
        task.await.unwrap().unwrap();
    }

    #[test]
    fn activity_attachment_request_is_strict_without_changing_legacy_variants() {
        let request: ControlRequest = serde_json::from_str(
            r#"{"command":"attach_activity","schema_version":1,"session_id":null,"tail":100,"follow":true}"#,
        )
        .unwrap();
        assert!(matches!(request, ControlRequest::AttachActivity(_)));
        assert!(
            serde_json::from_str::<ControlRequest>(
                r#"{"command":"attach_activity","schema_version":1,"session_id":null,"tail":100,"follow":true,"extra":false}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ControlRequest>(
                r#"{"command":"attach_activity","schema_version":1,"session_id":null,"tail":100,"tail":101,"follow":true}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ControlRequest>(r#"{"command":"list","extra":true}"#).is_ok()
        );
    }

    #[tokio::test]
    async fn activity_attachment_parse_errors_never_echo_values_or_unknown_keys() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let (registration, _registrations) = mpsc::channel(2);
        for request in [
            r#"{"command":"attach_activity","schema_version":1,"session_id":null,"tail":"value-sentinel","follow":true}"#,
            r#"{"command":"attach_activity","schema_version":1,"session_id":null,"tail":0,"follow":true,"key-sentinel":"value-sentinel"}"#,
        ] {
            let (server, mut client) = UnixStream::pair().unwrap();
            client
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let error =
                handle_control_connection(server, Arc::clone(&supervisor), registration.clone())
                    .await
                    .unwrap_err();
            let logged = format!("{error:#}");
            assert_eq!(logged, "invalid control request");
            assert!(!logged.contains("sentinel"));
        }
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn activity_attachment_rejects_unknown_schema_with_bounded_response() {
        let broker = activity_test_broker(8);
        let (server, client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(handle_activity_attachment(
            server,
            broker,
            AttachActivityRequest {
                schema_version: ACTIVITY_SCHEMA_VERSION + 1,
                session_id: None,
                tail: 0,
                follow: false,
            },
        ));
        let mut reader = BufReader::new(client);
        let response = read_activity_test_json(&mut reader).await;
        assert_eq!(response["ok"], false);
        assert_eq!(response["result"], Value::Null);
        assert_eq!(response["error"], "unsupported activity schema");
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn activity_client_no_follow_keeps_write_half_until_valid_replay_end() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        supervisor
            .activity_broker()
            .publish(
                activity_test_update(0x7080),
                Some("target"),
                Some(ACTIVITY_TEST_INSTANCE),
            )
            .unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        let (registration, _registrations) = mpsc::channel(1);
        let task = tokio::spawn(handle_control_connection(
            server,
            Arc::clone(&supervisor),
            registration,
        ));

        let replay = activity_replay_on_stream(client, Some("target".to_owned()), 1)
            .await
            .unwrap();
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0].sequence(), 1);
        assert_eq!(replay.snapshot_sequence, 1);
        assert!(!replay.history_truncated);
        assert_eq!(
            replay.generation,
            supervisor
                .activity_broker()
                .subscribe_snapshot(None, 0)
                .unwrap()
                .generation()
        );
        task.await.unwrap().unwrap();
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn activity_client_rejects_eof_before_replay_end() {
        let (server, client) = UnixStream::pair().unwrap();
        let server_task = tokio::spawn(async move {
            let (reader, mut writer) = server.into_split();
            let mut reader = BufReader::new(reader);
            assert!(
                read_line_limited(&mut reader, "activity request")
                    .await
                    .unwrap()
                    .contains("attach_activity")
            );
            writer
                .write_all(
                    &encode_line(&json!({
                        "ok": true,
                        "result": {
                            "control_protocol": 2,
                            "activity_schema": 1,
                            "generation": ACTIVITY_TEST_GENERATION,
                            "snapshot_sequence": 0,
                            "replayed": 0,
                            "history_truncated": false,
                        },
                        "error": Value::Null,
                    }))
                    .unwrap(),
                )
                .await
                .unwrap();
        });
        let error = activity_replay_on_stream(client, None, 0)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "activity stream ended before activity_end"
        );
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn activity_client_rejects_incompatible_metadata_before_frames() {
        let (server, client) = UnixStream::pair().unwrap();
        let server_task = tokio::spawn(async move {
            let (reader, mut writer) = server.into_split();
            let mut reader = BufReader::new(reader);
            let _ = read_line_limited(&mut reader, "activity request")
                .await
                .unwrap();
            writer
                .write_all(
                    &encode_line(&json!({
                        "ok": true,
                        "result": {
                            "control_protocol": 2,
                            "activity_schema": 2,
                            "generation": ACTIVITY_TEST_GENERATION,
                            "snapshot_sequence": 0,
                            "replayed": 0,
                            "history_truncated": false,
                        },
                        "error": Value::Null,
                    }))
                    .unwrap(),
                )
                .await
                .unwrap();
        });
        let error = activity_replay_on_stream(client, None, 0)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "unsupported activity schema");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn activity_client_strict_handshake_never_echoes_peer_errors() {
        let duplicate_metadata = concat!(
            "{\"ok\":true,\"result\":{",
            "\"control_protocol\":2,\"control_protocol\":2,\"activity_schema\":1,",
            "\"generation\":\"00000000-0000-4000-8000-000000000701\",",
            "\"snapshot_sequence\":0,\"replayed\":0,\"history_truncated\":false},",
            "\"error\":null}\n"
        );
        let malicious_error =
            "{\"ok\":false,\"result\":null,\"error\":\"secret-sentinel\\u001b[31m\"}\n";
        let missing_error = concat!(
            "{\"ok\":true,\"result\":{",
            "\"control_protocol\":2,\"activity_schema\":1,",
            "\"generation\":\"00000000-0000-4000-8000-000000000701\",",
            "\"snapshot_sequence\":0,\"replayed\":0,\"history_truncated\":false}}\n"
        );
        let missing_result = "{\"ok\":false,\"error\":\"unavailable\"}\n";
        for (response, expected) in [
            (duplicate_metadata, "invalid activity attach response"),
            (malicious_error, "activity attachment rejected"),
            (missing_error, "invalid activity attach response"),
            (missing_result, "invalid activity attach response"),
        ] {
            let (server, client) = UnixStream::pair().unwrap();
            let response = response.as_bytes().to_vec();
            let server_task = tokio::spawn(async move {
                let (reader, mut writer) = server.into_split();
                let mut reader = BufReader::new(reader);
                let _ = read_line_limited(&mut reader, "activity request")
                    .await
                    .unwrap();
                writer.write_all(&response).await.unwrap();
            });
            let error = activity_replay_on_stream(client, None, 0)
                .await
                .unwrap_err();
            let diagnostic = format!("{error:#}");
            assert_eq!(diagnostic, expected);
            assert!(!diagnostic.contains("sentinel"));
            assert!(!diagnostic.contains('\u{1b}'));
            server_task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn activity_client_uses_strict_event_decoder() {
        let (server, client) = UnixStream::pair().unwrap();
        let server_task = tokio::spawn(async move {
            let (reader, mut writer) = server.into_split();
            let mut reader = BufReader::new(reader);
            let _ = read_line_limited(&mut reader, "activity request")
                .await
                .unwrap();
            writer
                .write_all(
                    &encode_line(&json!({
                        "ok": true,
                        "result": {
                            "control_protocol": 2,
                            "activity_schema": 1,
                            "generation": ACTIVITY_TEST_GENERATION,
                            "snapshot_sequence": 1,
                            "replayed": 1,
                            "history_truncated": false,
                        },
                        "error": Value::Null,
                    }))
                    .unwrap(),
                )
                .await
                .unwrap();
            let duplicate_sequence = concat!(
                "{\"type\":\"activity\",\"event\":{",
                "\"schema_version\":1,\"sequence\":1,\"sequence\":1,",
                "\"operation_id\":\"00000000-0000-4000-8000-000000007080\",",
                "\"timestamp_ms\":1780000000000,\"session_id\":null,",
                "\"session_instance\":null,\"operation\":\"read_file\",",
                "\"state\":\"started\",\"duration_ms\":null,\"safe_summary\":\"\"}}\n"
            );
            writer
                .write_all(duplicate_sequence.as_bytes())
                .await
                .unwrap();
        });
        let error = activity_replay_on_stream(client, None, 1)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "invalid activity replay frame");
        server_task.await.unwrap();
    }

    #[test]
    fn activity_cli_local_time_is_single_line_and_preserves_milliseconds() {
        let formatted = format_local_activity_timestamp(1_780_000_000_123).unwrap();
        assert_eq!(formatted.len(), 30);
        assert_eq!(&formatted[19..23], ".123");
        assert!(matches!(formatted.as_bytes()[24], b'+' | b'-'));
        assert_eq!(&formatted[27..28], ":");
        assert!(!formatted.chars().any(char::is_control));
    }

    #[test]
    fn activity_cli_stdin_policy_ignores_non_tty_and_all_no_follow_input() {
        assert!(!should_monitor_activity_stdin(false, false));
        assert!(!should_monitor_activity_stdin(false, true));
        assert!(!should_monitor_activity_stdin(true, false));
        assert!(should_monitor_activity_stdin(true, true));

        assert!(activity_frame_read_has_timeout(false, false));
        assert!(activity_frame_read_has_timeout(false, true));
        assert!(activity_frame_read_has_timeout(true, false));
        assert!(!activity_frame_read_has_timeout(true, true));
    }

    #[tokio::test]
    async fn activity_cli_stdin_monitor_distinguishes_eof_from_failure() {
        let (eof_sender, eof_receiver) = tokio::sync::oneshot::channel();
        eof_sender.send(ActivityStdinStatus::Eof).unwrap();
        let mut eof = Some(eof_receiver);
        wait_for_tty_eof(&mut eof).await.unwrap();

        let (failed_sender, failed_receiver) = tokio::sync::oneshot::channel();
        failed_sender.send(ActivityStdinStatus::Failed).unwrap();
        let mut failed = Some(failed_receiver);
        assert_eq!(
            wait_for_tty_eof(&mut failed).await.unwrap_err().to_string(),
            "activity stdin monitor failed"
        );

        let (closed_sender, closed_receiver) = tokio::sync::oneshot::channel();
        drop(closed_sender);
        let mut closed = Some(closed_receiver);
        assert_eq!(
            wait_for_tty_eof(&mut closed).await.unwrap_err().to_string(),
            "activity stdin monitor failed"
        );
    }

    #[test]
    fn activity_cli_output_queue_is_exactly_bounded_at_256_lines() {
        assert_eq!(MAX_ACTIVITY_OUTPUT_QUEUE, 256);
        let (sender, _receiver) = std_mpsc::sync_channel::<String>(MAX_ACTIVITY_OUTPUT_QUEUE);
        for index in 0..MAX_ACTIVITY_OUTPUT_QUEUE {
            sender.try_send(index.to_string()).unwrap();
        }
        assert!(matches!(
            sender.try_send("overflow".to_owned()),
            Err(std_mpsc::TrySendError::Full(_))
        ));
    }

    #[test]
    fn activity_cli_output_writer_handles_success_broken_pipe_and_timeout() {
        let mutable_status_flags =
            |flags| flags & (libc::O_APPEND | libc::O_NONBLOCK | libc::O_ASYNC);
        let mut success_pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(success_pipe.as_mut_ptr()) }, 0);
        let success_flags = unsafe { libc::fcntl(success_pipe[1], libc::F_GETFL) };
        assert!(success_flags >= 0);
        assert_eq!(
            write_activity_output_line(success_pipe[1], b"one line\n", Duration::from_millis(100),),
            ActivityOutputStatus::Complete
        );
        assert_eq!(
            mutable_status_flags(unsafe { libc::fcntl(success_pipe[1], libc::F_GETFL) }),
            mutable_status_flags(success_flags),
            "activity output must preserve shared file-description flags"
        );
        let mut bytes = [0_u8; 9];
        assert_eq!(
            unsafe { libc::read(success_pipe[0], bytes.as_mut_ptr().cast(), bytes.len()) },
            9
        );
        assert_eq!(&bytes, b"one line\n");
        unsafe {
            libc::close(success_pipe[0]);
            libc::close(success_pipe[1]);
        }

        let mut broken_pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(broken_pipe.as_mut_ptr()) }, 0);
        unsafe { libc::close(broken_pipe[0]) };
        assert_eq!(
            write_activity_output_line(broken_pipe[1], b"ignored\n", Duration::from_millis(100),),
            ActivityOutputStatus::BrokenPipe
        );
        unsafe { libc::close(broken_pipe[1]) };

        let mut full_pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(full_pipe.as_mut_ptr()) }, 0);
        let flags = unsafe { libc::fcntl(full_pipe[1], libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(full_pipe[1], libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let fill = [0_u8; 4096];
        loop {
            let written = unsafe { libc::write(full_pipe[1], fill.as_ptr().cast(), fill.len()) };
            if written < 0 {
                assert_eq!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                break;
            }
        }
        assert_eq!(
            unsafe { libc::fcntl(full_pipe[1], libc::F_SETFL, flags) },
            0
        );
        assert_eq!(
            write_activity_output_line(full_pipe[1], b"blocked\n", Duration::from_millis(20),),
            ActivityOutputStatus::Failed
        );
        assert_eq!(
            mutable_status_flags(unsafe { libc::fcntl(full_pipe[1], libc::F_GETFL) }),
            mutable_status_flags(flags)
        );
        unsafe {
            libc::close(full_pipe[0]);
            libc::close(full_pipe[1]);
        }
    }

    #[test]
    fn activity_cli_stream_state_accepts_end_then_strict_global_gap() {
        let attach = ActivityAttachResult {
            control_protocol: CONTROL_PROTOCOL_VERSION,
            activity_schema: ACTIVITY_SCHEMA_VERSION,
            generation: ACTIVITY_TEST_GENERATION,
            snapshot_sequence: 0,
            replayed: 0,
            history_truncated: false,
        };
        let mut state = ActivityStreamState::new(attach, None);
        assert!(matches!(
            state
                .decode_line(
                    "{\"type\":\"activity_end\",\"snapshot_sequence\":0,\"history_truncated\":false}\n"
                )
                .unwrap(),
            ActivityClientFrame::End
        ));
        assert!(matches!(
            state
                .decode_line(
                    "{\"type\":\"activity_gap\",\"scope\":\"all_sessions\",\"after_sequence\":0,\"through_sequence\":3,\"dropped\":3}\n"
                )
                .unwrap(),
            ActivityClientFrame::Gap {
                after_sequence: 0,
                through_sequence: 3,
                dropped: 3,
            }
        ));
        assert!(matches!(
            state.decode_line(&activity_test_frame(4, None)).unwrap(),
            ActivityClientFrame::Event(event) if event.sequence() == 4
        ));
        assert!(state.decode_line(&activity_test_frame(6, None)).is_err());
        assert!(state
            .decode_line(
                "{\"type\":\"activity_gap\",\"scope\":\"target\",\"after_sequence\":4,\"through_sequence\":7,\"dropped\":3}\n"
            )
            .is_err());
    }

    #[test]
    fn activity_cli_stream_state_rejects_overlapping_gap_and_event_ranges() {
        let attach = ActivityAttachResult {
            control_protocol: CONTROL_PROTOCOL_VERSION,
            activity_schema: ACTIVITY_SCHEMA_VERSION,
            generation: ACTIVITY_TEST_GENERATION,
            snapshot_sequence: 3,
            replayed: 0,
            history_truncated: false,
        };
        let mut state = ActivityStreamState::new(attach, Some("target".to_owned()));
        state
            .decode_line(
                "{\"type\":\"activity_end\",\"snapshot_sequence\":3,\"history_truncated\":false}\n",
            )
            .unwrap();
        state
            .decode_line(
                "{\"type\":\"activity_gap\",\"scope\":\"all_sessions\",\"after_sequence\":5,\"through_sequence\":7,\"dropped\":2}\n",
            )
            .unwrap();
        assert!(
            state
                .decode_line(
                    "{\"type\":\"activity_gap\",\"scope\":\"all_sessions\",\"after_sequence\":6,\"through_sequence\":8,\"dropped\":2}\n",
                )
                .is_err()
        );
        assert!(
            state
                .decode_line(&activity_test_frame(7, Some("target")))
                .is_err()
        );
    }

    #[tokio::test]
    async fn activity_attachment_old_peer_close_is_observable_without_hanging() {
        let (old_peer, mut client) = UnixStream::pair().unwrap();
        let old_peer_task = tokio::spawn(async move {
            let mut reader = BufReader::new(old_peer);
            let line = read_line_limited(&mut reader, "legacy request")
                .await
                .unwrap();
            assert!(line.contains("attach_activity"));
        });
        let request = ControlRequest::AttachActivity(AttachActivityRequest {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            session_id: None,
            tail: 100,
            follow: true,
        });
        client
            .write_all(&encode_line(&request).unwrap())
            .await
            .unwrap();
        let mut response = [0_u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), client.read(&mut response))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        old_peer_task.await.unwrap();
    }

    #[tokio::test]
    async fn activity_attachment_pipelined_input_is_not_dispatched_or_buffered() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let (server, mut client) = UnixStream::pair().unwrap();
        let (registration, _registrations) = mpsc::channel(1);
        let task = tokio::spawn(handle_control_connection(
            server,
            Arc::clone(&supervisor),
            registration,
        ));
        client
            .write_all(
                b"{\"command\":\"attach_activity\",\"schema_version\":1,\"session_id\":null,\"tail\":0,\"follow\":true}\n{\"allow\":true}\n",
            )
            .await
            .unwrap();
        let mut response = [0_u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), client.read(&mut response))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        task.await.unwrap().unwrap();
        assert_eq!(
            supervisor
                .activity_broker()
                .subscribe_snapshot(None, 0)
                .unwrap()
                .cutoff(),
            0
        );
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn activity_attachment_initial_request_timeout_does_not_dispatch() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let (server, client) = UnixStream::pair().unwrap();
        let (registration, _registrations) = mpsc::channel(1);
        let task = tokio::spawn(handle_control_connection(
            server,
            Arc::clone(&supervisor),
            registration,
        ));
        let result = tokio::time::timeout(Duration::from_secs(6), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("timed out waiting for supervisor control request")
        );
        drop(client);
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn forgotten_session_disappears_from_list_and_info() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let id = format!("forget-views-{}", uuid::Uuid::new_v4());
        supervisor.start("src/repo", Some(&id)).await.unwrap();
        supervisor.stop(&id).await.unwrap();

        let listed = list_session_views(&supervisor).await.unwrap();
        assert!(listed.iter().any(|session| session.session_id == id));
        assert_eq!(inspect_session(&id).await.unwrap().session_id, id);

        supervisor.forget_session(&id).await.unwrap();
        let listed = list_session_views(&supervisor).await.unwrap();
        assert!(!listed.iter().any(|session| session.session_id == id));
        assert!(inspect_session(&id).await.is_err());

        supervisor.shutdown().await.unwrap();
        cleanup(&id).await;
    }

    fn named_root_fixture(paths: &[&str]) -> (tempfile::TempDir, NamedRoots, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let volume = temp.path().join("volume");
        for path in paths {
            std::fs::create_dir_all(volume.join(path)).unwrap();
        }
        let canonical = std::fs::canonicalize(&volume).unwrap();
        let roots =
            NamedRoots::from_canonical_roots(BTreeMap::from([("src".to_owned(), canonical)]))
                .unwrap();
        (temp, roots, volume)
    }

    #[tokio::test]
    async fn session_list_survives_an_owned_session_with_a_removed_workspace() {
        let (_temp, roots, volume) = named_root_fixture(&["healthy", "stale"]);
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let healthy_id = format!("list-healthy-{}", uuid::Uuid::new_v4());
        let stale_id = format!("list-stale-{}", uuid::Uuid::new_v4());
        supervisor
            .start("src/healthy", Some(&healthy_id))
            .await
            .unwrap();
        supervisor
            .start("src/stale", Some(&stale_id))
            .await
            .unwrap();

        let stale_cwd = config::read_session_metadata(&stale_id).await.unwrap().cwd;
        assert_eq!(
            stale_cwd,
            std::fs::canonicalize(volume.join("stale")).unwrap()
        );
        std::fs::remove_dir_all(&stale_cwd).unwrap();
        assert!(!stale_cwd.exists());

        let listed = list_session_views(&supervisor)
            .await
            .expect("one removed workspace must not fail the whole listing");
        let healthy = listed
            .iter()
            .find(|view| view.session_id == healthy_id)
            .expect("healthy owned session missing from list");
        assert_eq!(healthy.status, "active");
        let stale = listed
            .iter()
            .find(|view| view.session_id == stale_id)
            .expect("stale owned session missing from list");
        assert_eq!(stale.status, SESSION_STATUS_DEGRADED);
        assert_eq!(stale.cwd, stale_cwd);
        assert_ne!(stale.status, "stopped");
        assert_ne!(stale.status, "crashed");
        assert_ne!(stale.status, "active");
        assert!(
            config::session_path(&stale_id).unwrap().exists(),
            "listing must not delete stale metadata"
        );
        assert!(
            config::session_lifecycle_path(&stale_id).unwrap().exists(),
            "listing must not delete stale lifecycle state"
        );

        let info = inspect_session(&stale_id).await.unwrap();
        assert_eq!(info.status, SESSION_STATUS_DEGRADED);
        assert_eq!(info.session_id, stale_id);
        assert_eq!(info.cwd, stale_cwd);
        assert!(
            info.last_error
                .as_deref()
                .is_some_and(|error| error.contains(SESSION_WORKSPACE_DEGRADED))
        );
        assert_eq!(inspect_session(&healthy_id).await.unwrap().status, "active");

        supervisor.stop(&stale_id).await.unwrap();
        supervisor.stop(&healthy_id).await.unwrap();
        let after_stop = list_session_views(&supervisor).await.unwrap();
        assert!(
            !after_stop.iter().any(|view| view.session_id == stale_id),
            "bounded history keeps omitting sessions whose workspace no longer resolves"
        );
        supervisor.shutdown().await.unwrap();
        cleanup(&healthy_id).await;
        cleanup(&stale_id).await;
    }

    #[tokio::test]
    async fn degraded_history_is_excluded_before_the_candidate_bound() {
        let root = tempfile::tempdir().unwrap();
        let prefix = format!("history-bound-{}", uuid::Uuid::new_v4());
        let mut excluded = HashSet::new();
        if let Ok(mut entries) = tokio::fs::read_dir(config::sessions_dir().unwrap()).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) == Some("json")
                    && let Some(id) = path.file_stem().and_then(|value| value.to_str())
                {
                    excluded.insert(id.to_owned());
                }
            }
        }

        let healthy_dir = root.path().join("healthy");
        std::fs::create_dir(&healthy_dir).unwrap();
        let healthy_cwd = config::canonical_directory(&healthy_dir).unwrap();
        let healthy_id = format!("{prefix}-healthy");
        config::save_session(&config::Session {
            id: healthy_id.clone(),
            cwd: healthy_cwd.clone(),
            permitted_directories: vec![healthy_cwd],
            started_at: 2,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
            grants: config::SessionGrants::default(),
        })
        .await
        .unwrap();

        let mut degraded_ids = Vec::new();
        for index in 0..10 {
            let id = format!("{prefix}-{index:02}-degraded");
            let directory = root.path().join(format!("gone-{index:02}"));
            std::fs::create_dir(&directory).unwrap();
            let cwd = config::canonical_directory(&directory).unwrap();
            config::save_session(&config::Session {
                id: id.clone(),
                cwd: cwd.clone(),
                permitted_directories: vec![cwd],
                started_at: 1,
                process_id: 0,
                permission_mode: config::PermissionMode::Agent,
                grants: config::SessionGrants::default(),
            })
            .await
            .unwrap();
            std::fs::remove_dir(&directory).unwrap();
            degraded_ids.push(id);
        }

        let candidates = collect_bounded_history_session_ids(
            &excluded,
            MAX_SESSION_HISTORY_DIRECTORY_ENTRIES_SCANNED,
            8,
        )
        .await
        .unwrap();
        assert!(
            candidates.contains(&healthy_id),
            "degraded entries must not consume the candidate bound: {candidates:?}"
        );
        for id in &degraded_ids {
            assert!(
                !candidates.contains(id),
                "degraded history must be excluded before the bound: {id}"
            );
            assert!(
                config::session_path(id).unwrap().exists(),
                "exclusion must not delete stale metadata"
            );
        }

        let _ = tokio::fs::remove_file(config::session_path(&healthy_id).unwrap()).await;
        for id in &degraded_ids {
            let _ = tokio::fs::remove_file(config::session_path(id).unwrap()).await;
        }
    }

    #[tokio::test]
    async fn missing_workspace_is_never_reported_as_a_liveness_outcome() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("workspace");
        std::fs::create_dir(&cwd).unwrap();
        let cwd = config::canonical_directory(&cwd).unwrap();
        let id = format!("degraded-lived-{}", uuid::Uuid::new_v4());
        cleanup(&id).await;
        let session = config::Session {
            id: id.clone(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 1_700_000_000,
            process_id: std::process::id(),
            permission_mode: config::PermissionMode::Agent,
            grants: config::SessionGrants::default(),
        };
        config::save_session(&session).await.unwrap();
        let mut lifecycle =
            SessionLifecycle::starting(session.started_at, Some("src/workspace".to_owned()));
        lifecycle.status = LifecycleStatus::Active;
        config::save_session_lifecycle(&id, &lifecycle)
            .await
            .unwrap();
        let socket_server = spawn_active_session_socket(&id).await;
        std::fs::remove_dir_all(&session.cwd).unwrap();

        let live = inspect_session_read_only(&id).await.unwrap();
        assert_eq!(live.status, SESSION_STATUS_DEGRADED);
        assert_eq!(
            live.pid,
            Some(std::process::id()),
            "a live socket must keep the process identity visible"
        );
        assert!(live.last_error.is_some());
        assert_eq!(
            inspect_session(&id).await.unwrap().status,
            SESSION_STATUS_DEGRADED
        );
        let durable = config::read_session_lifecycle(&id).await.unwrap().unwrap();
        assert_eq!(
            durable.status,
            LifecycleStatus::Active,
            "a resolved live socket must not rewrite lifecycle state"
        );
        socket_server.abort();
        let _ = tokio::fs::remove_file(config::socket_path(&id).unwrap()).await;

        let dead = inspect_session_read_only(&id).await.unwrap();
        assert_eq!(dead.status, SESSION_STATUS_DEGRADED);
        assert_ne!(dead.status, "crashed");
        assert_ne!(dead.status, "stopped");
        assert!(dead.pid.is_none());
        assert!(config::session_path(&id).unwrap().exists());
        assert!(config::session_lifecycle_path(&id).unwrap().exists());
        cleanup(&id).await;
    }

    #[tokio::test]
    async fn session_view_reports_the_derived_workspace_identity() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("workspace");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(cwd.join(".git")).unwrap();
        std::fs::write(cwd.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let cwd = config::canonical_directory(&cwd).unwrap();
        let id = format!("workspace-view-{}", uuid::Uuid::new_v4());
        cleanup(&id).await;
        config::save_session(&config::Session {
            id: id.clone(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
            started_at: 1,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
            grants: config::SessionGrants::default(),
        })
        .await
        .unwrap();

        let view = inspect_session_read_only(&id).await.unwrap();
        let workspace = view.workspace.expect("workspace identity");
        assert_eq!(workspace.workspace_type.as_str(), "canonical_checkout");
        assert_eq!(workspace.repository_root, cwd);
        assert_eq!(workspace.workspace_root, cwd);
        assert_eq!(workspace.branch.as_deref(), Some("main"));
        assert_eq!(workspace.task, None);

        // A session whose workspace no longer resolves reports no workspace
        // identity instead of a stale one.
        std::fs::remove_dir_all(&cwd).unwrap();
        let degraded = inspect_session_read_only(&id).await.unwrap();
        assert_eq!(degraded.status, SESSION_STATUS_DEGRADED);
        assert!(degraded.workspace.is_none());

        cleanup(&id).await;
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

    #[cfg(target_os = "linux")]
    #[test]
    fn helper_generation_classifies_bundle_against_running_policy_schema() {
        use std::os::unix::fs::PermissionsExt;

        let write_bundle = |helper_stdout: Option<String>| {
            let temp = tempfile::tempdir().unwrap();
            let executable = temp.path().join("temote-mcp");
            std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            if let Some(stdout) = helper_stdout {
                let helper = temp.path().join("temote-linux-sandbox");
                std::fs::write(
                    &helper,
                    format!("#!/bin/sh\nif [ \"$1\" = \"--capabilities\" ]; then echo '{stdout}'; else exit 2; fi\n"),
                )
                .unwrap();
                std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            temp
        };

        let matching = write_bundle(Some(
            "{\"version\":\"test-version\",\"policy_schema\":1}".to_owned(),
        ));
        assert_eq!(
            classify_helper_generation(&matching.path().join("temote-mcp")),
            HelperGeneration::Compatible
        );

        let newer = write_bundle(Some(
            "{\"version\":\"test-version\",\"policy_schema\":999}".to_owned(),
        ));
        assert_eq!(
            classify_helper_generation(&newer.path().join("temote-mcp")),
            HelperGeneration::Incompatible
        );

        let missing = write_bundle(None);
        assert_eq!(
            classify_helper_generation(&missing.path().join("temote-mcp")),
            HelperGeneration::Unavailable
        );

        let unparseable = write_bundle(Some("not-json".to_owned()));
        assert_eq!(
            classify_helper_generation(&unparseable.path().join("temote-mcp")),
            HelperGeneration::Unavailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn protected_upgrade_snapshot_stays_bound_after_locator_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let locator = temp.path().join("temote-mcp");
        std::fs::write(&locator, b"#!/bin/sh\nprintf 'approved\\n'\n").unwrap();
        std::fs::set_permissions(&locator, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut source = std::fs::File::open(&locator).unwrap();
        let snapshot = create_upgrade_execution_snapshot(&mut source).unwrap();

        let replacement = temp.path().join("replacement");
        std::fs::write(&replacement, b"#!/bin/sh\nprintf 'replacement\\n'\n").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::rename(&replacement, &locator).unwrap();

        let output = std::process::Command::new(&snapshot.path).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"approved\n");
        let snapshot_path = snapshot.path.clone();
        let snapshot_directory = snapshot.directory.clone();
        assert_eq!(snapshot_path.file_name().unwrap(), "temote-mcp");
        drop(snapshot);
        assert!(!snapshot_path.exists());
        assert!(!snapshot_directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn codex_plugin_reconciliation_preserves_installed_locator_for_snapshot_child() {
        let executable = Path::new("/private/upgrade-snapshot/temote-mcp");
        let installed_locator = Path::new("/private/installed/temote-mcp");
        let command = codex_plugin_reconcile_command(executable, installed_locator);

        assert_eq!(command.get_program(), executable);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["codex", "plugin", "install"]
        );
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == INTERNAL_INSTALLED_LOCATOR_ENV)
                .and_then(|(_, value)| value),
            Some(installed_locator.as_os_str())
        );
    }

    #[test]
    fn same_version_zero_session_handoff_requires_new_boot_generation() {
        let source_boot = uuid::Uuid::new_v4().to_string();
        let unchanged = json!({
            "version": "2026.8.0",
            "pid": 42,
            "boot_generation": source_boot,
        });
        assert!(!supervisor_handoff_identity_changed(
            &unchanged,
            "2026.8.0",
            42,
            unchanged["boot_generation"].as_str(),
        ));

        let replaced = json!({
            "version": "2026.8.0",
            "pid": 42,
            "boot_generation": uuid::Uuid::new_v4().to_string(),
        });
        assert!(supervisor_handoff_identity_changed(
            &replaced,
            "2026.8.0",
            42,
            unchanged["boot_generation"].as_str(),
        ));
    }

    #[test]
    fn local_upgrade_rejects_nonterminal_remote_runtime_owner() {
        let active = crate::upgrade_transaction::UpgradeTransaction::new(
            "2026.8.0", "2026.9.0", "host-a", "boot-a", true, true, true,
        );
        assert!(ensure_no_remote_upgrade_owns_runtime(std::slice::from_ref(&active)).is_err());

        let mut completed = active;
        completed.state = crate::upgrade_transaction::UpgradeTransactionState::Completed;
        assert!(ensure_no_remote_upgrade_owns_runtime(&[completed]).is_ok());
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
                permission_mode: config::PermissionMode::Ask,
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
                permission_mode: Some(config::PermissionMode::Yolo),
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
                permission_mode: Some(config::PermissionMode::Ask),
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
    async fn manual_restart_preserves_agent_and_ask_permission_modes() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let agent_id = format!("restart-agent-{}", uuid::Uuid::new_v4());
        let ask_id = format!("restart-ask-{}", uuid::Uuid::new_v4());

        supervisor
            .start_with_mode_with_environment(
                "src/repo",
                Some(&agent_id),
                config::PermissionMode::Agent,
                CapturedStartEnvironment::default(),
            )
            .await
            .unwrap();
        supervisor
            .start_with_mode_with_environment(
                "src/repo",
                Some(&ask_id),
                config::PermissionMode::Ask,
                CapturedStartEnvironment::default(),
            )
            .await
            .unwrap();

        restart_session(
            &supervisor,
            &agent_id,
            CapturedStartEnvironment::default(),
            false,
        )
        .await
        .unwrap();
        restart_session(
            &supervisor,
            &ask_id,
            CapturedStartEnvironment::default(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            config::read_session_metadata(&agent_id)
                .await
                .unwrap()
                .permission_mode,
            config::PermissionMode::Agent,
            "manual restart must not downgrade agent to ask"
        );
        assert_eq!(
            config::read_session_metadata(&ask_id)
                .await
                .unwrap()
                .permission_mode,
            config::PermissionMode::Ask,
            "manual restart must not migrate an explicit ask session"
        );

        supervisor.shutdown().await.unwrap();
        cleanup(&agent_id).await;
        cleanup(&ask_id).await;
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
            metadata: std::collections::BTreeMap::new(),
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
                    metadata: std::collections::BTreeMap::new(),
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
                                false,
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

    #[test]
    fn permission_mode_control_request_accepts_legacy_yolo_and_current_mode() {
        let legacy: ControlRequest = serde_json::from_str(
            r#"{"command":"permission_mode","session_id":"legacy","yolo":true}"#,
        )
        .unwrap();
        match legacy {
            ControlRequest::PermissionMode {
                permission_mode,
                yolo,
                ..
            } => {
                assert!(permission_mode.is_none());
                assert!(yolo);
            }
            _ => panic!("unexpected request variant"),
        }

        let current: ControlRequest = serde_json::from_str(
            r#"{"command":"permission_mode","session_id":"current","permission_mode":"agent","yolo":false}"#,
        )
        .unwrap();
        match current {
            ControlRequest::PermissionMode {
                permission_mode, ..
            } => {
                assert_eq!(permission_mode, Some(config::PermissionMode::Agent));
            }
            _ => panic!("unexpected request variant"),
        }

        let encoded = serde_json::to_value(ControlRequest::PermissionMode {
            session_id: "mirror".to_owned(),
            permission_mode: Some(config::PermissionMode::Agent),
            yolo: false,
        })
        .unwrap();
        assert_eq!(encoded["permission_mode"], "agent");
        assert_eq!(encoded["yolo"], false);
    }

    #[test]
    fn upgrade_session_identity_rejects_same_count_different_session() {
        let planned = vec![crate::upgrade_transaction::UpgradePlannedSession {
            session_id: "session-a".to_owned(),
            source_process_id: 100,
            source_started_at: 10,
        }];
        let active = vec![crate::upgrade_transaction::UpgradePlannedSession {
            session_id: "session-b".to_owned(),
            source_process_id: 200,
            source_started_at: 20,
        }];
        let error =
            validate_planned_upgrade_session_identities(&planned, &active, false).unwrap_err();
        assert!(error.to_string().contains("session set changed"));
    }

    #[test]
    fn upgrade_session_identity_rejects_replaced_source_instance() {
        let planned = vec![crate::upgrade_transaction::UpgradePlannedSession {
            session_id: "session-a".to_owned(),
            source_process_id: 100,
            source_started_at: 10,
        }];
        let replacement = vec![crate::upgrade_transaction::UpgradePlannedSession {
            session_id: "session-a".to_owned(),
            source_process_id: 101,
            source_started_at: 11,
        }];
        let error =
            validate_planned_upgrade_session_identities(&planned, &replacement, true).unwrap_err();
        assert!(error.to_string().contains("session instance changed"));
        validate_planned_upgrade_session_identities(&planned, &replacement, false).unwrap();
    }

    const GC_TEST_CWD: &str = "/tmp";
    static GC_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn gc_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
        GC_TEST_LOCK.lock().await
    }

    fn gc_test_id(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::new_v4())
    }

    async fn cleanup_gc_paths(ids: &[String]) {
        for id in ids {
            if let Ok(path) = config::session_path(id) {
                let _ = tokio::fs::remove_file(path).await;
            }
            if let Ok(path) = config::session_lifecycle_path(id) {
                let _ = tokio::fs::remove_file(path).await;
            }
            if let Ok(path) = config::socket_path(id) {
                let _ = tokio::fs::remove_file(path).await;
            }
        }
    }

    async fn write_gc_metadata(id: &str) -> PathBuf {
        let cwd = config::canonical_directory(Path::new(GC_TEST_CWD)).unwrap();
        let session = config::Session {
            id: id.to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 10,
            process_id: std::process::id(),
            permission_mode: config::PermissionMode::Agent,
            grants: config::SessionGrants::default(),
        };
        config::save_session(&session).await.unwrap();
        config::session_path(id).unwrap()
    }

    async fn write_gc_terminal_lifecycle(id: &str, stopped_at: u64) -> PathBuf {
        let mut lifecycle = SessionLifecycle::starting(10, None);
        lifecycle.status = LifecycleStatus::Stopped;
        lifecycle.stopped_at = Some(stopped_at);
        config::save_session_lifecycle(id, &lifecycle)
            .await
            .unwrap();
        config::session_lifecycle_path(id).unwrap()
    }

    fn backdate_file(path: &Path, age_secs: u64) {
        let modified = std::time::SystemTime::now()
            .checked_sub(Duration::from_secs(age_secs))
            .unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(modified).unwrap();
    }

    async fn spawn_active_session_socket(id: &str) -> tokio::task::JoinHandle<()> {
        let path = config::socket_path(id).unwrap();
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut line = String::new();
                let mut reader = BufReader::new(&mut stream);
                if reader.read_line(&mut line).await.is_ok() {
                    let _ = stream.write_all(b"active\n").await;
                }
            }
        })
    }

    struct GcFixture {
        missing_json: String,
        missing_state: String,
        owned: String,
        live: String,
        pair: String,
        fresh: String,
        symlinked: String,
        malformed: String,
        mismatched: String,
    }

    impl GcFixture {
        fn all_ids(&self) -> Vec<String> {
            vec![
                self.missing_json.clone(),
                self.missing_state.clone(),
                self.owned.clone(),
                self.live.clone(),
                self.pair.clone(),
                self.fresh.clone(),
                self.symlinked.clone(),
                self.malformed.clone(),
                self.mismatched.clone(),
            ]
        }
    }

    async fn gc_fixture() -> GcFixture {
        let old = SESSION_ORPHAN_GRACE_SECONDS + 3600;
        let missing_json = gc_test_id("gc-missing-json");
        let state = write_gc_terminal_lifecycle(&missing_json, 20).await;
        backdate_file(&state, old);

        let missing_state = gc_test_id("gc-missing-state");
        let metadata = write_gc_metadata(&missing_state).await;
        backdate_file(&metadata, old);

        let owned = gc_test_id("gc-owned");
        let owned_state = write_gc_terminal_lifecycle(&owned, 20).await;
        backdate_file(&owned_state, old);

        let live = gc_test_id("gc-live");
        let live_state = write_gc_terminal_lifecycle(&live, 20).await;
        backdate_file(&live_state, old);

        let pair = gc_test_id("gc-pair");
        let pair_metadata = write_gc_metadata(&pair).await;
        let pair_state = write_gc_terminal_lifecycle(&pair, 20).await;
        backdate_file(&pair_metadata, old);
        backdate_file(&pair_state, old);

        let fresh = gc_test_id("gc-fresh");
        let _fresh_state = write_gc_terminal_lifecycle(&fresh, 20).await;

        let symlinked = gc_test_id("gc-symlink");
        let symlink_path = config::session_path(&symlinked).unwrap();
        std::os::unix::fs::symlink(&pair_metadata, &symlink_path).unwrap();

        let malformed = gc_test_id("gc-malformed");
        let malformed_state = config::session_lifecycle_path(&malformed).unwrap();
        std::fs::write(&malformed_state, b"not json").unwrap();
        backdate_file(&malformed_state, old);

        let mismatched = gc_test_id("gc-mismatch");
        let mismatched_metadata = config::session_path(&mismatched).unwrap();
        let cwd = config::canonical_directory(Path::new(GC_TEST_CWD)).unwrap();
        let foreign = config::Session {
            id: gc_test_id("gc-foreign"),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 10,
            process_id: std::process::id(),
            permission_mode: config::PermissionMode::Agent,
            grants: config::SessionGrants::default(),
        };
        std::fs::write(&mismatched_metadata, serde_json::to_vec(&foreign).unwrap()).unwrap();
        backdate_file(&mismatched_metadata, old);

        GcFixture {
            missing_json,
            missing_state,
            owned,
            live,
            pair,
            fresh,
            symlinked,
            malformed,
            mismatched,
        }
    }

    fn gc_candidate_ids(entries: &[SessionGcEntry]) -> BTreeSet<String> {
        entries
            .iter()
            .map(|entry| entry.session_id.clone())
            .collect()
    }

    #[tokio::test]
    async fn session_gc_metadata_diagnostics_explain_orphan_classes() {
        let _guard = gc_test_lock().await;
        let missing_json = gc_test_id("gc-diag-missing-json");
        let state = write_gc_terminal_lifecycle(&missing_json, 20).await;
        backdate_file(&state, SESSION_ORPHAN_GRACE_SECONDS + 3600);
        let missing_state = gc_test_id("gc-diag-missing-state");
        let metadata = write_gc_metadata(&missing_state).await;
        backdate_file(&metadata, SESSION_ORPHAN_GRACE_SECONDS + 3600);

        let diagnostics = session_metadata_diagnostics().await.unwrap();
        assert!(diagnostics.missing_json_count >= 1);
        assert!(diagnostics.missing_state_count >= 1);
        assert!(
            diagnostics.invalid_orphan_count
                >= diagnostics.missing_json_count + diagnostics.missing_state_count
        );
        cleanup_gc_paths(&[missing_json, missing_state]).await;
    }

    #[test]
    fn session_gc_limit_is_bounded() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let too_small = runtime.block_on(run_session_gc(&HashSet::new(), true, 0));
        assert!(too_small.is_err());
        let too_large = runtime.block_on(run_session_gc(&HashSet::new(), true, 1001));
        assert!(too_large.is_err());
    }

    /// Host/CI-only session-GC liveness acceptance.
    ///
    /// These tests bind and probe Unix-domain sockets (`session_is_active`), which
    /// the local-agent sandbox denies. They are deliberately separated from the
    /// pure policy tests so `just sandboxed-check` can skip the whole module and
    /// report it as NOT RUN instead of faking a PASS.
    mod host_liveness_tests {
        use super::*;

        #[tokio::test]
        async fn session_gc_dry_run_reports_reviewed_orphans_and_mutates_nothing() {
            let _guard = gc_test_lock().await;
            let fixture = gc_fixture().await;
            let live_socket = spawn_active_session_socket(&fixture.live).await;
            let mut owned = HashSet::new();
            owned.insert(fixture.owned.clone());

            let report = run_session_gc(&owned, true, 100).await.unwrap();

            let candidates = gc_candidate_ids(&report.candidates);
            assert!(candidates.contains(&fixture.missing_json));
            assert!(candidates.contains(&fixture.missing_state));
            for excluded in [
                &fixture.owned,
                &fixture.live,
                &fixture.pair,
                &fixture.fresh,
                &fixture.symlinked,
                &fixture.malformed,
                &fixture.mismatched,
            ] {
                assert!(
                    !candidates.contains(excluded),
                    "{excluded} must not be eligible"
                );
            }
            assert!(report.dry_run);
            assert!(report.removed.is_empty());
            assert!(report.skipped.is_empty());
            assert!(!report.truncated);
            assert!(report.missing_json_orphans >= 1);
            assert!(report.missing_state_orphans >= 1);

            // dry-run leaves every file in place
            assert!(
                config::session_lifecycle_path(&fixture.missing_json)
                    .unwrap()
                    .exists()
            );
            assert!(
                config::session_path(&fixture.missing_state)
                    .unwrap()
                    .exists()
            );
            assert!(!config::session_path(&fixture.owned).unwrap().exists());
            assert!(
                config::session_lifecycle_path(&fixture.owned)
                    .unwrap()
                    .exists()
            );
            live_socket.abort();
            cleanup_gc_paths(&fixture.all_ids()).await;
        }

        #[tokio::test]
        async fn session_gc_apply_removes_only_reviewed_orphans() {
            let _guard = gc_test_lock().await;
            let fixture = gc_fixture().await;
            let live_socket = spawn_active_session_socket(&fixture.live).await;
            let mut owned = HashSet::new();
            owned.insert(fixture.owned.clone());

            let report = run_session_gc(&owned, false, 100).await.unwrap();

            assert!(!report.dry_run);
            let removed = gc_candidate_ids(&report.removed);
            assert!(removed.contains(&fixture.missing_json));
            assert!(removed.contains(&fixture.missing_state));
            assert!(report.skipped.is_empty());

            assert!(
                !config::session_lifecycle_path(&fixture.missing_json)
                    .unwrap()
                    .exists()
            );
            assert!(
                !config::session_path(&fixture.missing_state)
                    .unwrap()
                    .exists()
            );
            assert!(
                config::session_lifecycle_path(&fixture.owned)
                    .unwrap()
                    .exists()
            );
            assert!(
                config::session_lifecycle_path(&fixture.live)
                    .unwrap()
                    .exists()
            );
            assert!(config::session_path(&fixture.pair).unwrap().exists());
            assert!(
                config::session_lifecycle_path(&fixture.pair)
                    .unwrap()
                    .exists()
            );
            assert!(
                config::session_lifecycle_path(&fixture.fresh)
                    .unwrap()
                    .exists()
            );
            assert!(config::session_path(&fixture.symlinked).unwrap().exists());
            assert!(
                config::session_lifecycle_path(&fixture.malformed)
                    .unwrap()
                    .exists()
            );
            assert!(config::session_path(&fixture.mismatched).unwrap().exists());
            live_socket.abort();
            cleanup_gc_paths(&fixture.all_ids()).await;
        }

        #[tokio::test]
        async fn session_gc_grace_period_boundary_is_respected() {
            let _guard = gc_test_lock().await;
            let inside = gc_test_id("gc-inside-grace");
            let inside_state = write_gc_terminal_lifecycle(&inside, 20).await;
            backdate_file(
                &inside_state,
                SESSION_ORPHAN_GRACE_SECONDS.saturating_sub(3600),
            );

            let outside = gc_test_id("gc-outside-grace");
            let outside_state = write_gc_terminal_lifecycle(&outside, 20).await;
            backdate_file(&outside_state, SESSION_ORPHAN_GRACE_SECONDS + 3600);

            let report = run_session_gc(&HashSet::new(), true, 100).await.unwrap();
            let candidates = gc_candidate_ids(&report.candidates);

            assert!(!candidates.contains(&inside));
            assert!(candidates.contains(&outside));
            cleanup_gc_paths(&[inside, outside]).await;
        }

        #[tokio::test]
        async fn session_gc_ordering_and_limit_are_deterministic() {
            let _guard = gc_test_lock().await;
            let mut expected = Vec::new();
            let mut ids = Vec::new();
            for age in [
                7200_u64 + SESSION_ORPHAN_GRACE_SECONDS,
                3600 + SESSION_ORPHAN_GRACE_SECONDS,
                60 + SESSION_ORPHAN_GRACE_SECONDS,
            ] {
                let id = gc_test_id("gc-order");
                let state = write_gc_terminal_lifecycle(&id, 20).await;
                backdate_file(&state, age);
                expected.push(id.clone());
                ids.push(id);
            }

            let report = run_session_gc(&HashSet::new(), true, 2).await.unwrap();
            assert!(report.truncated);
            assert_eq!(report.candidates.len(), 2);
            assert_eq!(
                report
                    .candidates
                    .iter()
                    .map(|entry| &entry.session_id)
                    .collect::<Vec<_>>(),
                vec![&expected[0], &expected[1]]
            );
            assert!(
                report
                    .candidates
                    .iter()
                    .all(|entry| { entry.reason == SessionGcReason::MissingJson })
            );

            let repeat = run_session_gc(&HashSet::new(), true, 2).await.unwrap();
            assert_eq!(
                gc_candidate_ids(&repeat.candidates),
                gc_candidate_ids(&report.candidates)
            );

            let limited = run_session_gc(&HashSet::new(), true, 1).await.unwrap();
            assert_eq!(limited.candidates.first().unwrap().session_id, expected[0]);
            cleanup_gc_paths(&ids).await;
        }

        #[tokio::test]
        async fn session_gc_apply_skips_drift_and_concurrent_start() {
            let _guard = gc_test_lock().await;
            let drift = gc_test_id("gc-drift");
            let drift_state = write_gc_terminal_lifecycle(&drift, 20).await;
            backdate_file(&drift_state, SESSION_ORPHAN_GRACE_SECONDS + 3600);
            let drifted_plan = build_session_gc_plan(&HashSet::new(), 100).await.unwrap();
            assert!(gc_candidate_ids(&drifted_plan.report.candidates).contains(&drift));

            // drift: the orphan file is touched after the plan was built
            backdate_file(&drift_state, SESSION_ORPHAN_GRACE_SECONDS + 60);
            let (removed, skipped) = apply_session_gc_plan(&HashSet::new(), &drifted_plan)
                .await
                .unwrap();
            assert!(!gc_candidate_ids(&removed).contains(&drift));
            assert!(gc_candidate_ids(&skipped).contains(&drift));
            assert!(drift_state.exists());

            // drift: the counterpart appears after the plan was built
            let counterpart = gc_test_id("gc-counterpart");
            let counterpart_state = write_gc_terminal_lifecycle(&counterpart, 20).await;
            backdate_file(&counterpart_state, SESSION_ORPHAN_GRACE_SECONDS + 3600);
            let counterpart_plan = build_session_gc_plan(&HashSet::new(), 100).await.unwrap();
            assert!(gc_candidate_ids(&counterpart_plan.report.candidates).contains(&counterpart));
            write_gc_metadata(&counterpart).await;
            let (removed, skipped) = apply_session_gc_plan(&HashSet::new(), &counterpart_plan)
                .await
                .unwrap();
            assert!(!gc_candidate_ids(&removed).contains(&counterpart));
            assert!(gc_candidate_ids(&skipped).contains(&counterpart));
            assert!(counterpart_state.exists());

            // concurrent start: the session became supervisor-owned after the plan
            let starting = gc_test_id("gc-starting");
            let starting_state = write_gc_terminal_lifecycle(&starting, 20).await;
            backdate_file(&starting_state, SESSION_ORPHAN_GRACE_SECONDS + 3600);
            let starting_plan = build_session_gc_plan(&HashSet::new(), 100).await.unwrap();
            assert!(gc_candidate_ids(&starting_plan.report.candidates).contains(&starting));
            let owned = HashSet::from([starting.clone()]);
            let (removed, skipped) = apply_session_gc_plan(&owned, &starting_plan).await.unwrap();
            assert!(!gc_candidate_ids(&removed).contains(&starting));
            assert!(gc_candidate_ids(&skipped).contains(&starting));
            assert!(starting_state.exists());
            cleanup_gc_paths(&[drift, counterpart, starting]).await;
        }
    }
}
