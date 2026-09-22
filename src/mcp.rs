use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::line_protocol::{
    BoundedLine, MAX_JSON_LINE_BYTES, next_bounded_line, validate_child_tool_call,
};
use crate::{
    activity_runtime, apply_patch, approvals, checkpoints, child_env, codex_app_server, config,
    dev_tool, evidence, friction, local_agent, managed_worktree, onepassword_cli, onepassword_mcp,
    onepassword_sdk, recall, sandbox, session_control, session_control::SessionBackend,
    work_handoff,
};
use temote_mcp::activity::contract::{
    ActivityCancellationReason, ActivityErrorKind, ActivityOperation, ActivityRemote,
    ActivityResult, ActivitySummary,
};
use temote_mcp::activity::scope::ActivityScope;

const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ACTIVE_JOBS_PER_SESSION: usize = 8;
const MAX_JOB_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);
const COMPLETED_JOB_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_COMPLETED_JOBS_PER_SESSION: usize = 128;
const MAX_COMPLETED_JOBS_TOTAL: usize = 1024;
pub(crate) const MAX_GIT_ADD_PATHS: usize = 256;
const MAX_PATH_ARGUMENT_BYTES: usize = 4096;
const MAX_RPC_METHOD_BYTES: usize = 256;
const MAX_RPC_ID_STRING_BYTES: usize = 256;
const MAX_MCP_TOOL_NAME_BYTES: usize = 256;
const MAX_COMMAND_ARGUMENTS: usize = 256;
const MAX_COMMAND_ARGUMENT_BYTES: usize = 32 * 1024;
const MAX_COMMAND_TOTAL_BYTES: usize = 128 * 1024;
pub(crate) const MAX_GIT_COMMIT_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_GIT_TAG_NAME_BYTES: usize = 255;
const MAX_GIT_BRANCH_NAME_BYTES: usize = 255;
const MAX_GIT_BASE_REF_BYTES: usize = 512;
const MAX_GIT_WORKTREE_NAME_BYTES: usize = 64;
const MAX_MANAGED_WORKTREE_LIST_ENTRIES: usize = 128;
const MAX_GITHUB_WORKFLOW_BYTES: usize = 255;
const MAX_GITHUB_REF_BYTES: usize = 255;
const MAX_GITHUB_REMOTE_URL_BYTES: usize = 2048;
const MAX_GIT_REMOTE_DESTINATIONS: usize = 32;
const MAX_GIT_REMOTE_DESTINATIONS_BYTES: usize = 32 * MAX_GITHUB_REMOTE_URL_BYTES;
const MAX_GIT_CONFIG_VALUES: usize = 32;
const MAX_GIT_CONFIG_OUTPUT_BYTES: usize = 16 * 1024;
const GIT_PULL_UPSTREAM_CONFIGURATION_ERROR: &str =
    "Git pull upstream configuration is unavailable";
const GIT_REMOTE_DESTINATION_ERROR: &str = "Git remote destination is unavailable";
const GITHUB_CREDENTIAL_MAPPING_ERROR: &str = "GitHub repository credential mapping is unavailable";
const GITHUB_CREDENTIAL_UNAVAILABLE_ERROR: &str = "GitHub repository credential is unavailable";
const GITHUB_CREDENTIAL_PERMISSION_ERROR: &str =
    "GitHub repository credential lacks required permission";
const GITHUB_NETWORK_GIT_ERROR: &str = "GitHub repository network Git operation failed";
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_MCP_RESPONSE_BYTES: usize = 52 * 1024 * 1024;
const MAX_TEXT_FILE_BYTES: usize = 8 * 1024 * 1024;
const MIN_RETURN_OUTPUT_BYTES: usize = 256;
const MAX_RETURN_OUTPUT_BYTES: usize = sandbox::MAX_COMMAND_OUTPUT_BYTES;
const MAX_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_DIRECTORY_LIST_BYTES: usize = 1024 * 1024;
const MAX_SESSION_LIST_ENTRIES: usize = 256;
const MAX_SESSION_LIST_BYTES: usize = 4 * 1024 * 1024;
const LATEST_LEGACY_PROTOCOL_VERSION: &str = "2025-06-18";
pub(crate) const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
const SUPPORTED_LEGACY_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const SERVER_INSTRUCTIONS: &str = "Call session_list first. When the local session supervisor has no session for the required project, create one with session_start using a configured named-root path, then call session_info before normal tools. Existing tools require session_id except session_list and session_start.";
const PROCESS_IDENTITY_META_KEY: &str = "io.temote/processIdentity";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivityOwner {
    McpCall,
    JobWorker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivitySuccess {
    Completed,
    Accepted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActivityToolCoverage {
    name: &'static str,
    operation: ActivityOperation,
    owner: ActivityOwner,
    success: ActivitySuccess,
    fixture: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivityNonDispatchOwner {
    Supervisor,
    Excluded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActivityNonDispatchCoverage {
    name: &'static str,
    owner: ActivityNonDispatchOwner,
    fixture: &'static str,
}

const ACTIVITY_NON_DISPATCH_COVERAGE: &[ActivityNonDispatchCoverage] = &[
    ActivityNonDispatchCoverage {
        name: "session_list",
        owner: ActivityNonDispatchOwner::Excluded,
        fixture: "viewer query excluded",
    },
    ActivityNonDispatchCoverage {
        name: "session_start",
        owner: ActivityNonDispatchOwner::Supervisor,
        fixture: "activity lifecycle",
    },
    ActivityNonDispatchCoverage {
        name: "session_stop",
        owner: ActivityNonDispatchOwner::Supervisor,
        fixture: "activity lifecycle",
    },
    ActivityNonDispatchCoverage {
        name: "session_restart",
        owner: ActivityNonDispatchOwner::Supervisor,
        fixture: "activity lifecycle",
    },
    ActivityNonDispatchCoverage {
        name: "session_info",
        owner: ActivityNonDispatchOwner::Excluded,
        fixture: "viewer query excluded",
    },
];

const ACTIVITY_TOOL_COVERAGE: &[ActivityToolCoverage] = &[
    activity_tool("read_file", ActivityOperation::ReadFile, "file read"),
    activity_tool(
        "evidence_read",
        ActivityOperation::EvidenceRead,
        "evidence read",
    ),
    activity_tool(
        "codex_status",
        ActivityOperation::CodexStatus,
        "Codex status",
    ),
    activity_tool_accepted(
        "codex_task_start",
        ActivityOperation::CodexTaskStart,
        "Codex start acceptance",
    ),
    activity_tool(
        "codex_task_get",
        ActivityOperation::CodexTaskGet,
        "Codex get",
    ),
    activity_tool_accepted(
        "codex_task_control",
        ActivityOperation::CodexTaskControl,
        "Codex control acceptance",
    ),
    activity_job_tool(
        "local_agent_run",
        ActivityOperation::LocalAgentRun,
        "fake local agent worker",
    ),
    activity_job_tool(
        "dev_tool_run",
        ActivityOperation::DevToolRun,
        "fake developer tool worker",
    ),
    activity_tool("get_image", ActivityOperation::GetImage, "image read"),
    activity_tool(
        "list_directory",
        ActivityOperation::ListDirectory,
        "directory listing",
    ),
    activity_tool("write_file", ActivityOperation::WriteFile, "file write"),
    activity_tool("apply_patch", ActivityOperation::ApplyPatch, "patch apply"),
    activity_tool("git_add", ActivityOperation::GitAdd, "Git local"),
    activity_tool("git_commit", ActivityOperation::GitCommit, "Git local"),
    activity_tool("git_fetch", ActivityOperation::GitFetch, "Git network"),
    activity_tool("git_pull", ActivityOperation::GitPull, "Git network"),
    activity_tool("git_push", ActivityOperation::GitPush, "Git network"),
    activity_tool("git_push_tag", ActivityOperation::GitPush, "Git network"),
    activity_tool(
        "git_branch_create",
        ActivityOperation::GitBranchCreate,
        "Git branch",
    ),
    activity_tool("git_switch", ActivityOperation::GitSwitch, "Git branch"),
    activity_tool(
        "git_worktree_add",
        ActivityOperation::GitWorktreeAdd,
        "Git worktree",
    ),
    activity_tool(
        "git_worktree_create",
        ActivityOperation::GitWorktreeCreate,
        "Git worktree",
    ),
    activity_tool(
        "git_worktree_list",
        ActivityOperation::GitWorktreeList,
        "Git worktree list",
    ),
    activity_tool(
        "git_worktree_remove",
        ActivityOperation::GitWorktreeRemove,
        "Git worktree remove",
    ),
    activity_tool(
        "git_worktree_prune",
        ActivityOperation::GitWorktreePrune,
        "Git worktree prune",
    ),
    activity_tool(
        "github_workflow_dispatch",
        ActivityOperation::GithubWorkflowDispatch,
        "GitHub workflow dispatch",
    ),
    activity_tool(
        "github_workflow_run_get",
        ActivityOperation::GithubWorkflowRunGet,
        "GitHub workflow status",
    ),
    activity_job_tool(
        "execute",
        ActivityOperation::Execute,
        "sandbox command worker",
    ),
    activity_job_tool(
        "start_command",
        ActivityOperation::StartCommand,
        "sandbox command worker",
    ),
    activity_tool("poll_job", ActivityOperation::PollJob, "job poll"),
    activity_tool("job_list", ActivityOperation::JobList, "job list"),
    activity_tool(
        "checkpoint_save",
        ActivityOperation::CheckpointSave,
        "checkpoint save",
    ),
    activity_tool(
        "checkpoint_load",
        ActivityOperation::CheckpointLoad,
        "checkpoint load",
    ),
    activity_tool(
        "work_handoff",
        ActivityOperation::WorkHandoff,
        "work handoff",
    ),
    activity_tool(
        "friction_summary",
        ActivityOperation::FrictionSummary,
        "friction summary",
    ),
    activity_tool(
        "learning_candidate_list",
        ActivityOperation::LearningCandidateList,
        "learning candidates",
    ),
    activity_tool("recall", ActivityOperation::Recall, "learning recall"),
    activity_tool(
        "recall_feedback",
        ActivityOperation::RecallFeedback,
        "recall feedback",
    ),
    activity_tool(
        "stop_job",
        ActivityOperation::StopJob,
        "job cancellation request",
    ),
    activity_tool(
        "onepassword_mcp_discover",
        ActivityOperation::OnePasswordMcpDiscover,
        "1Password MCP discovery",
    ),
    activity_tool(
        "onepassword_mcp_read_resource",
        ActivityOperation::OnePasswordMcpReadResource,
        "1Password MCP resource",
    ),
    activity_tool(
        "onepassword_mcp_call",
        ActivityOperation::OnePasswordMcpCall,
        "1Password MCP outer call",
    ),
    activity_tool(
        "onepassword_item_get",
        ActivityOperation::OnePasswordItemGet,
        "1Password item outer call",
    ),
    activity_tool(
        "onepassword_secret_resolve",
        ActivityOperation::OnePasswordSecretResolve,
        "1Password SDK outer call",
    ),
    activity_tool(
        "onepassword_service_account_status",
        ActivityOperation::OnePasswordServiceAccountStatus,
        "1Password service-account status",
    ),
    activity_tool(
        "onepassword_service_account_run",
        ActivityOperation::OnePasswordServiceAccountRun,
        "1Password service-account outer call",
    ),
    activity_tool(
        "kintone_mcp_status",
        ActivityOperation::KintoneMcpStatus,
        "kintone MCP status",
    ),
    activity_tool(
        "kintone_mcp_discover",
        ActivityOperation::KintoneMcpDiscover,
        "kintone MCP discovery",
    ),
    activity_tool(
        "kintone_mcp_call",
        ActivityOperation::KintoneMcpCall,
        "kintone MCP outer call",
    ),
    activity_tool(
        "kintone_cli_status",
        ActivityOperation::KintoneCliStatus,
        "cli-kintone status",
    ),
    activity_tool(
        "kintone_cli_run",
        ActivityOperation::KintoneCliRun,
        "cli-kintone outer call",
    ),
    activity_tool(
        "without_sandbox",
        ActivityOperation::WithoutSandbox,
        "host command",
    ),
];

const fn activity_tool(
    name: &'static str,
    operation: ActivityOperation,
    fixture: &'static str,
) -> ActivityToolCoverage {
    ActivityToolCoverage {
        name,
        operation,
        owner: ActivityOwner::McpCall,
        success: ActivitySuccess::Completed,
        fixture,
    }
}

const fn activity_tool_accepted(
    name: &'static str,
    operation: ActivityOperation,
    fixture: &'static str,
) -> ActivityToolCoverage {
    ActivityToolCoverage {
        success: ActivitySuccess::Accepted,
        ..activity_tool(name, operation, fixture)
    }
}

const fn activity_job_tool(
    name: &'static str,
    operation: ActivityOperation,
    fixture: &'static str,
) -> ActivityToolCoverage {
    ActivityToolCoverage {
        owner: ActivityOwner::JobWorker,
        ..activity_tool(name, operation, fixture)
    }
}

#[derive(Clone)]
enum CachedJobResult {
    Success {
        text: String,
        evidence: Option<evidence::EvidenceRef>,
    },
    Error {
        text: String,
        evidence: Option<evidence::EvidenceRef>,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OutputPolicy {
    output_limit_bytes: Option<usize>,
    status_only: bool,
}

#[derive(Default)]
struct JobCompletion {
    result: Option<CachedJobResult>,
    completed_at: Option<Instant>,
    activity: Option<ActivityScope>,
    activity_terminal: bool,
}

#[derive(Clone, Copy)]
enum JobActivityFailure {
    ChildFailed,
    SandboxSetupFailed,
}

impl JobActivityFailure {
    const fn error_kind(self) -> ActivityErrorKind {
        match self {
            Self::ChildFailed => ActivityErrorKind::ChildFailed,
            Self::SandboxSetupFailed => ActivityErrorKind::SandboxSetupFailed,
        }
    }
}

#[derive(Clone, Copy)]
enum JobActivityOutcome {
    Completed,
    Failed(JobActivityFailure),
    Cancelled(ActivityCancellationReason),
}

struct Job {
    session_id: String,
    command: String,
    handle: JoinHandle<()>,
    completion: Arc<Mutex<JobCompletion>>,
    output_policy: OutputPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveJobAdmission {
    cwd: PathBuf,
    worktree_root: Option<PathBuf>,
}

struct JobSlot {
    session_id: String,
    admission_id: Uuid,
    _reservation: Option<managed_worktree::WorktreeReservation>,
}

impl Drop for JobSlot {
    fn drop(&mut self) {
        release_job_slot(&self.session_id, self.admission_id);
    }
}

struct JobState {
    jobs: HashMap<Uuid, Job>,
    active_by_session: HashMap<String, usize>,
    active_admissions: HashMap<Uuid, ActiveJobAdmission>,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub(crate) struct JobSummary {
    pub job_id: String,
    pub status: String,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub(crate) struct JobListSnapshot {
    pub jobs: Vec<JobSummary>,
    pub truncated: bool,
}

fn jobs() -> &'static Mutex<JobState> {
    static JOBS: OnceLock<Mutex<JobState>> = OnceLock::new();
    JOBS.get_or_init(|| {
        Mutex::new(JobState {
            jobs: HashMap::new(),
            active_by_session: HashMap::new(),
            active_admissions: HashMap::new(),
        })
    })
}

pub async fn serve() -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    loop {
        let line = match next_bounded_line(&mut input, MAX_JSON_LINE_BYTES).await? {
            Some(BoundedLine::Line(line)) => line,
            Some(BoundedLine::TooLarge) => {
                write_message(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("MCP request exceeds {MAX_JSON_LINE_BYTES} bytes")}}),
                )
                .await?;
                continue;
            }
            Some(BoundedLine::InvalidUtf8) => {
                write_message(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"MCP request must be valid UTF-8"}}),
                )
                .await?;
                continue;
            }
            None => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                write_message(&mut stdout, &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}})).await?;
                continue;
            }
        };
        if let Err(error) = validate_rpc_request_shape(&request) {
            write_message(
                &mut stdout,
                &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":format!("{error:#}")}}),
            )
            .await?;
            continue;
        }
        if request.get("id").is_none() {
            continue;
        }
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let response = match dispatch(&request).await {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":format!("{error:#}")}})
            }
        };
        write_message(&mut stdout, &response).await?;
    }
    Ok(())
}

fn encode_json_line_with_limit(message: &Value, max_bytes: usize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(message).context("failed to serialize MCP response")?;
    let wire_bytes = bytes
        .len()
        .checked_add(1)
        .context("MCP response size overflow")?;
    anyhow::ensure!(
        wire_bytes <= max_bytes,
        "MCP response exceeds {max_bytes} bytes"
    );
    bytes.push(b'\n');
    Ok(bytes)
}

fn bounded_mcp_response_line(message: &Value, max_bytes: usize) -> Result<Vec<u8>> {
    match encode_json_line_with_limit(message, max_bytes) {
        Ok(bytes) => Ok(bytes),
        Err(_) => {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            encode_json_line_with_limit(
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32000,
                        "message": format!("MCP response exceeds {max_bytes} bytes")
                    }
                }),
                max_bytes,
            )
        }
    }
}

async fn write_message(stdout: &mut tokio::io::Stdout, message: &Value) -> Result<()> {
    let line = bounded_mcp_response_line(message, MAX_MCP_RESPONSE_BYTES)?;
    stdout.write_all(&line).await?;
    stdout.flush().await?;
    Ok(())
}

pub(crate) async fn dispatch(request: &Value) -> Result<Value> {
    dispatch_with_mode(request, false, None).await
}

#[cfg(feature = "network")]
pub(crate) async fn dispatch_public(
    request: &Value,
    sessions: Option<&SessionBackend>,
) -> Result<Value> {
    dispatch_with_mode(request, true, sessions).await
}

async fn dispatch_with_mode(
    request: &Value,
    public: bool,
    sessions: Option<&SessionBackend>,
) -> Result<Value> {
    validate_rpc_request_shape(request)?;
    let modern = modern_request(request);
    if modern || request.get("method").and_then(Value::as_str) == Some("server/discover") {
        validate_modern_request(request)?;
    }

    let result = match request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "initialize" => {
            let mut result = json!({
                "protocolVersion": negotiate_protocol_version(request),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "temote-mcp", "title": "Temote MCP", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Call session_list first. On the public serve endpoint, use session_start with a configured named-root path when the required project session is absent, then call session_info before normal tools. Managed sessions are always normal sandboxed sessions; remote clients cannot create yolo sessions or self-approve host operations. A CLI session started locally with `temote-mcp start <session-id> --yolo` remains a separate local choice. The session mode does not control confirmation or authorization enforced by the MCP client."
            });
            result["_meta"] = process_identity_meta();
            Ok(result)
        }
        "server/discover" => Ok(discover_result()),
        "ping" => {
            let mut result = json!({});
            result["_meta"] = process_identity_meta();
            Ok(result)
        }
        "tools/list" => Ok(json!({"tools": tools(public, sessions.is_some())})),
        "tools/call" => {
            call_tool(
                request.get("params").unwrap_or(&Value::Null),
                public,
                sessions,
            )
            .await
        }
        method => anyhow::bail!("method not found: {method}"),
    }?;

    if modern {
        Ok(modernize_result(
            request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            result,
        ))
    } else {
        Ok(result)
    }
}

fn valid_rpc_id(value: &Value) -> bool {
    match value {
        Value::Null | Value::Number(_) => true,
        Value::String(value) => value.len() <= MAX_RPC_ID_STRING_BYTES,
        Value::Bool(_) | Value::Array(_) | Value::Object(_) => false,
    }
}

pub(crate) fn validate_rpc_request_shape(request: &Value) -> Result<()> {
    let object = request
        .as_object()
        .context("JSON-RPC request must be an object")?;
    anyhow::ensure!(
        object.get("jsonrpc").and_then(Value::as_str) == Some("2.0"),
        "JSON-RPC request must declare jsonrpc=2.0"
    );
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .context("JSON-RPC method must be a string")?;
    anyhow::ensure!(!method.is_empty(), "JSON-RPC method must not be empty");
    anyhow::ensure!(
        method.len() <= MAX_RPC_METHOD_BYTES,
        "JSON-RPC method exceeds {MAX_RPC_METHOD_BYTES} bytes"
    );
    if let Some(id) = object.get("id") {
        anyhow::ensure!(valid_rpc_id(id), "JSON-RPC id is invalid or too large");
    }
    Ok(())
}

fn validate_mcp_tool_name(name: &str) -> Result<()> {
    anyhow::ensure!(!name.is_empty(), "tool name must not be empty");
    anyhow::ensure!(
        name.len() <= MAX_MCP_TOOL_NAME_BYTES,
        "tool name exceeds {MAX_MCP_TOOL_NAME_BYTES} bytes"
    );
    Ok(())
}

fn negotiate_protocol_version(request: &Value) -> &'static str {
    let requested = request
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str);
    SUPPORTED_LEGACY_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .find(|version| Some(*version) == requested)
        .unwrap_or(LATEST_LEGACY_PROTOCOL_VERSION)
}

fn modern_request(request: &Value) -> bool {
    let Some(meta) = request.pointer("/params/_meta").and_then(Value::as_object) else {
        return false;
    };
    [
        "io.modelcontextprotocol/protocolVersion",
        "io.modelcontextprotocol/clientCapabilities",
        "io.modelcontextprotocol/clientInfo",
        "io.modelcontextprotocol/logLevel",
    ]
    .iter()
    .any(|key| meta.contains_key(*key))
}

fn validate_modern_request(request: &Value) -> Result<()> {
    let meta = request
        .pointer("/params/_meta")
        .and_then(Value::as_object)
        .context("modern MCP requests require params._meta")?;
    let version = meta
        .get("io.modelcontextprotocol/protocolVersion")
        .and_then(Value::as_str)
        .context("modern MCP requests require io.modelcontextprotocol/protocolVersion")?;
    anyhow::ensure!(
        version == MODERN_PROTOCOL_VERSION,
        "unsupported MCP protocol version: {version}"
    );
    anyhow::ensure!(
        meta.get("io.modelcontextprotocol/clientCapabilities")
            .is_some_and(Value::is_object),
        "modern MCP requests require io.modelcontextprotocol/clientCapabilities as an object"
    );
    Ok(())
}

/// Bounded, non-secret process identity that a reconnecting MCP client can use to
/// confirm it reached the intended host, version, and boot generation. It is not a
/// credential and is never derived from session or environment secret material.
fn process_identity() -> Value {
    json!({
        "host_id": crate::host_identity::resolve().unwrap_or_else(|_| "unknown".to_owned()),
        "version": env!("CARGO_PKG_VERSION"),
        "boot_generation": crate::boot_identity::generation(),
    })
}

fn process_identity_meta() -> Value {
    let mut meta = serde_json::Map::new();
    meta.insert(PROCESS_IDENTITY_META_KEY.to_owned(), process_identity());
    Value::Object(meta)
}

fn server_info() -> Value {
    json!({
        "name": "temote-mcp",
        "title": "Temote MCP",
        "version": env!("CARGO_PKG_VERSION")
    })
}

fn discover_result() -> Value {
    let mut meta = serde_json::Map::new();
    meta.insert(
        "io.modelcontextprotocol/serverInfo".to_owned(),
        server_info(),
    );
    meta.insert(PROCESS_IDENTITY_META_KEY.to_owned(), process_identity());
    meta.insert(
        "dev.temote/contractFingerprint".to_owned(),
        json!(public_contract_fingerprint()),
    );
    json!({
        "resultType": "complete",
        "supportedVersions": [MODERN_PROTOCOL_VERSION],
        "capabilities": {"tools": {"listChanged": false}},
        "instructions": SERVER_INSTRUCTIONS,
        "ttlMs": 0,
        "cacheScope": "private",
        "_meta": Value::Object(meta)
    })
}

fn modernize_result(method: &str, mut result: Value) -> Value {
    if method == "server/discover" {
        return result;
    }
    let Some(object) = result.as_object_mut() else {
        return result;
    };
    object.insert("resultType".to_owned(), json!("complete"));
    let meta = object
        .entry("_meta".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(serde_json::Map::new());
    }
    meta.as_object_mut().unwrap().insert(
        "io.modelcontextprotocol/serverInfo".to_owned(),
        server_info(),
    );
    if method == "tools/list" {
        object.insert("ttlMs".to_owned(), json!(0));
        object.insert("cacheScope".to_owned(), json!("private"));
    }
    result
}

/// Public contract advertised to repository-controlled MCP surfaces.
///
/// The contract covers the public tool names with their exact input schemas
/// and annotations plus the routed protocol versions. Model-facing prose
/// (`title`, `description`) is stripped so parity checks compare behavior
/// rather than wording. This is the same value checked in as
/// `gateway/contract/routed-tools.json`.
fn strip_gateway_contract_prose(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("title");
            object.remove("description");
            for child in object.values_mut() {
                strip_gateway_contract_prose(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_gateway_contract_prose(item);
            }
        }
        _ => {}
    }
}

fn routed_gateway_contract() -> Value {
    let mut routed_tools = tools(true, true).as_array().unwrap().to_owned();
    let host_property = json!({"type": "string"});
    for tool in &mut routed_tools {
        let name = tool["name"].as_str().unwrap_or_default().to_owned();
        if name == "session_list" {
            tool["inputSchema"] = json!({
                "type": "object",
                "properties": {"host_id": host_property.clone()},
                "additionalProperties": false
            });
            continue;
        }
        if let Some(properties) = tool
            .pointer_mut("/inputSchema/properties")
            .and_then(Value::as_object_mut)
        {
            properties.insert("host_id".to_owned(), host_property.clone());
        }
        if name == "session_start" {
            tool["inputSchema"]["required"] = json!(["host_id", "path"]);
        }
    }
    routed_tools.insert(0, json!({
        "name": "host_info",
        "title": "Inspect a federated Temote host",
        "description": "Show one currently leased federated host.",
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
        "inputSchema": {
            "type": "object",
            "properties": {"host_id": host_property.clone()},
            "required": ["host_id"],
            "additionalProperties": false
        }
    }));
    routed_tools.insert(0, json!({
        "name": "host_list",
        "title": "List federated Temote hosts",
        "description": "List currently leased federated hosts.",
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
    }));
    let mut routed_tools = Value::Array(routed_tools);
    strip_gateway_contract_prose(&mut routed_tools);
    json!({
        "latestLegacyProtocolVersion": LATEST_LEGACY_PROTOCOL_VERSION,
        "supportedLegacyProtocolVersions": SUPPORTED_LEGACY_PROTOCOL_VERSIONS,
        "modernProtocolVersion": MODERN_PROTOCOL_VERSION,
        "tools": routed_tools,
    })
}

/// Deterministic JSON with recursively sorted object keys.
///
/// `serde_json` maps are only order-preserving when the `preserve_order`
/// feature is enabled by some dependency, so sorting is explicit here. The
/// gateway reimplements the same canonicalization; the fingerprints must match
/// byte for byte over the canonical UTF-8 text.
fn canonical_contract_json(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(left, _)| *left);
            let rendered = entries
                .into_iter()
                .map(|(key, child)| {
                    format!(
                        "{}:{}",
                        Value::String(key.clone()),
                        canonical_contract_json(child)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{rendered}}}")
        }
        Value::Array(items) => {
            let rendered = items
                .iter()
                .map(canonical_contract_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{rendered}]")
        }
        _ => value.to_string(),
    }
}

/// Bounded public contract fingerprint shared by diagnostics surfaces.
///
/// The local server reports this from `session_info` and `server/discover`;
/// the gateway reports it from `/healthz`. Operators compare either value
/// against `gateway/contract/public-tools.fingerprint`, which the snapshot test
/// keeps in sync with `gateway/contract/routed-tools.json`.
pub(crate) fn public_contract_fingerprint() -> &'static str {
    static FINGERPRINT: OnceLock<String> = OnceLock::new();
    FINGERPRINT.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let text = canonical_contract_json(&routed_gateway_contract());
        format!("{:x}", Sha256::digest(text.as_bytes()))
    })
}

fn client_checkpoint_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": {"type":"string","maxLength":256},
            "base_commit": {"anyOf":[{"type":"string","pattern":"^(?:[0-9A-Fa-f]{40}|[0-9A-Fa-f]{64})$"},{"type":"null"}]},
            "steps": {
                "type":"array","maxItems":64,
                "items": {
                    "type":"object",
                    "properties": {
                        "id":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,64}$"},
                        "description":{"type":"string","maxLength":256},
                        "reported_status":{"type":"string","enum":["pending","in_progress","implemented","verified","blocked"]}
                    },
                    "required":["id","description","reported_status"],
                    "additionalProperties":false
                }
            },
            "checks": {
                "type":"array","maxItems":64,
                "items": {
                    "type":"object",
                    "properties": {
                        "step_id":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,64}$"},
                        "name":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,64}$"},
                        "reported_result":{"type":"string","enum":["pass","fail","not_run"]},
                        "commit":{"anyOf":[{"type":"string","pattern":"^(?:[0-9A-Fa-f]{40}|[0-9A-Fa-f]{64})$"},{"type":"null"}]}
                    },
                    "required":["step_id","name","reported_result","commit"],
                    "additionalProperties":false
                }
            },
            "next_step_id":{"anyOf":[{"type":"string","pattern":"^[A-Za-z0-9_-]{1,64}$"},{"type":"null"}]}
        },
        "required":["title","base_commit","steps","checks","next_step_id"],
        "additionalProperties":false
    })
}

fn checkpoint_save_input_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "session_id":{"type":"string"},
            "operation_id":{"type":"string","format":"uuid"},
            "checkpoint_id":{"type":"string"},
            "expected_revision":{"type":"integer","minimum":0},
            "checkpoint":client_checkpoint_schema()
        },
        "required":["session_id","operation_id","expected_revision","checkpoint"],
        "additionalProperties":false
    })
}

fn checkpoint_load_input_schema() -> Value {
    json!({
        "type":"object",
        "properties":{"session_id":{"type":"string"},"checkpoint_id":{"type":"string"}},
        "required":["session_id","checkpoint_id"],
        "additionalProperties":false
    })
}

fn work_handoff_input_schema() -> Value {
    json!({
        "type":"object",
        "properties":{"session_id":{"type":"string"},"checkpoint_id":{"type":"string"}},
        "required":["session_id"],
        "additionalProperties":false
    })
}

fn local_agent_input_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "session_id":{"type":"string"},
            "agent":{"type":"string","enum":["codex","opencode"]},
            "task":{"type":"string","minLength":1,"maxLength":local_agent::MAX_TASK_BYTES},
            "cwd":{"type":"string"},
            "worktree":{
                "type":"object",
                "properties":{
                    "branch":{"type":"string","minLength":1,"maxLength":MAX_GIT_BRANCH_NAME_BYTES},
                    "task":{"type":"string","minLength":1,"maxLength":managed_worktree::MAX_MANAGED_TASK_BYTES}
                },
                "required":["branch"],
                "additionalProperties":false
            },
            "access":{"type":"string","enum":["read_only","workspace_write"]},
            "model":{"type":"string","minLength":1,"maxLength":256},
            "effort":{"type":"string","minLength":1,"maxLength":128},
            "profile":{"type":"string","minLength":1,"maxLength":128}
        },
        "required":["session_id","agent","task","access"],
        "additionalProperties":false,
        "allOf":[
            {
                "if":{
                    "properties":{"agent":{"const":"opencode"}},
                    "required":["agent"]
                },
                "then":{
                    "properties":{"task":{"maxLength":local_agent::MAX_OPENCODE_TASK_BYTES}}
                }
            }
        ]
    })
}

fn dev_tool_input_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "session_id":{"type":"string"},
            "tool":{"type":"string","enum":["cargo","vp","uv","npm","pnpm","go"]},
            "operation":{"type":"string","minLength":1,"maxLength":dev_tool::MAX_DEV_TOOL_OPERATION_BYTES},
            "args":{"type":"array","items":{"type":"string","maxLength":dev_tool::MAX_DEV_TOOL_ARGUMENT_BYTES},"maxItems":dev_tool::MAX_DEV_TOOL_ARGUMENTS},
            "cwd":{"type":"string"}
        },
        "required":["session_id","tool","operation"],
        "additionalProperties":false
    })
}

fn tools(public: bool, managed_sessions: bool) -> Value {
    let mut tools = json!([
        {"name":"session_list","title":"List Temote MCP sessions","description":"List active temote-mcp sessions and surface sessions whose liveness or workspace cannot be safely determined (status degraded). Returns session IDs, working directories, start times, status, and permission mode (ask/agent/yolo).","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
        {"name":"session_start","title":"Start a managed Temote MCP session","description":"Start a normal sandboxed session under a host-configured named root. Path must be <root-name> or <root-name>/<relative-path>; absolute paths and yolo creation are unavailable.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"path":{"type":"string"},"session_id":{"type":"string"}},"required":["path"],"additionalProperties":false}},
        {"name":"session_stop","title":"Stop a managed Temote MCP session","description":"Gracefully stop a session created through the authenticated HTTP endpoint and owned by the local Temote session supervisor. Local CLI/yolo sessions cannot be stopped remotely.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"session_restart","title":"Restart a managed Temote MCP session","description":"Restart an active normal sandboxed session created through the authenticated HTTP endpoint. Local CLI/yolo sessions cannot be restarted remotely.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"session_info","title":"Inspect a Temote MCP session","description":"Show durable lifecycle state, working directory, permission mode, exit reason, and last error for a temote-mcp session.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"read_file","title":"Read a local file","description":"Read a UTF-8 regular file up to 8 MiB. With optional start_line/end_line or offset_bytes plus max_bytes, return bounded range metadata with an unambiguous UTF-8 next offset. Omitting range arguments preserves whole-file behavior.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1},"offset_bytes":{"type":"integer","minimum":0},"max_bytes":{"type":"integer","minimum":4,"maximum":8388608}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"evidence_read","title":"Read scoped Temote evidence","description":"Read a bounded UTF-8 chunk from an opaque expiring evidence record previously returned by Temote. Evidence is in-memory, session-owned, canonical-scope-bound, and cannot address arbitrary filesystem paths.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"evidence_id":{"type":"string","format":"uuid"},"offset_bytes":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":65536,"default":16384}},"required":["session_id","evidence_id"],"additionalProperties":false}},
        {"name":"codex_status","title":"Check Codex app-server compatibility","description":"Check the locally installed Codex app-server through stdio, validate the concrete protocol response shapes Temote consumes, and return bounded model/effort plus best-effort version diagnostics without a release-number allowlist.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"codex_task_start","title":"Start a scoped Codex task","description":"Accept an idempotent scoped Codex task mutation, persist acceptance before child side effects, then start a workspace-write Codex app-server thread/turn. operation_id is mandatory; no sandbox escape option is exposed.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"operation_id":{"type":"string","format":"uuid"},"task":{"type":"string","minLength":1,"maxLength":1048576},"model":{"type":"string","minLength":1,"maxLength":256},"effort":{"type":"string","minLength":1,"maxLength":256}},"required":["session_id","operation_id","task","model","effort"],"additionalProperties":false}},
        {"name":"codex_task_get","title":"Read a scoped Codex task","description":"Read and reconcile a retained Codex task owned by the full Temote session instance and canonical scope. Detailed thread data is exposed only through bounded scoped evidence.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"after_revision":{"type":"integer","minimum":0}},"required":["session_id","task_id"],"additionalProperties":false}},
        {"name":"codex_task_control","title":"Control a scoped Codex task","description":"Idempotently steer, resume, or interrupt a retained scoped Codex task. Acceptance is persisted before the app-server side effect; uncertain crash gaps return reconciliation_required rather than replaying blindly.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid"},"action":{"type":"string","enum":["steer","resume","interrupt"]},"input":{"type":"string","minLength":1,"maxLength":1048576}},"required":["session_id","task_id","operation_id","action"],"additionalProperties":false}},
        {"name":"local_agent_run","title":"Run a local coding agent","description":"Run a verified Codex or OpenCode non-interactive agent in the selected session with canonical workspace scope, bounded task/output, isolated agent state, and local approval. The caller supplies a task and access mode, not an executable, raw argv, environment, or network policy. With worktree.branch, Temote binds the run to the selected repository's managed worktree (<configured src root>/worktrees/<repo>/<task>) and derives the path itself: it reuses only a verified managed worktree of that repository and branch, otherwise creates one through the approved path, and rejects cwd combined with worktree.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":local_agent_input_schema()},
        {"name":"dev_tool_run","title":"Run a structured developer tool operation","description":"Run a validated Cargo, Vite+, uv, npm, pnpm, or Go operation through the developer broker with canonical workspace scope and narrowly scoped tool cache state. Offline development operations run with network disabled; dependency/network operations use an explicitly classified network profile. Package-manager operations use a narrow fixed subcommand contract and do not expose arbitrary executables or raw host commands.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":dev_tool_input_schema()},
        {"name":"get_image","title":"Read a local image","description":"Read a local image up to 32 MiB and return it as MCP image content. Relative paths use the session working directory.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string","description":"Path to a PNG, JPEG, GIF, WebP, BMP, TIFF, or AVIF image."}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"list_directory","title":"List a local directory","description":"List up to 10,000 entries from a local directory, with at most 1 MiB of rendered names. Relative paths use the session working directory.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"write_file","title":"Write a local file","description":"Write a UTF-8 regular file using the selected session permission mode. Existing special-file targets are rejected. Normal sessions are restricted to permitted roots and use the temote-mcp sandbox; yolo sessions may write anywhere the local user can.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"},"content":{"type":"string"}},"required":["session_id","path","content"],"additionalProperties":false}},
        {"name":"apply_patch","title":"Apply a bounded multi-file patch","description":"Parse a Codex-style *** Begin Patch patch, preflight every source and destination inside the session roots, request approval once for normal sessions, then apply add/update/move/delete operations without invoking a shell parser. Partial I/O failure reports the exact committed operations.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"patch":{"type":"string","minLength":1,"maxLength":1048576}},"required":["session_id","patch"],"additionalProperties":false}},
        {"name":"git_add","title":"Stage files with Git","description":"Stage existing files or directories in the session repository with git add. Only the specified paths are staged; Git hooks and network access are unavailable.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":256},"cwd":{"type":"string"}},"required":["session_id","paths"],"additionalProperties":false}},
        {"name":"git_commit","title":"Create a local Git commit","description":"Create a local commit from the current Git index. This does not push, hooks and signing are disabled, and network access is unavailable.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"message":{"type":"string","minLength":1,"maxLength":16384},"cwd":{"type":"string"}},"required":["session_id","message"],"additionalProperties":false}},
        {"name":"git_fetch","title":"Fetch Git remote updates","description":"Run git fetch --prune for a configured remote on the host. The remote must be a safe configured name and arbitrary URLs and refspecs are not accepted. A GitHub HTTPS remote additionally requires the repository-local managed Git credential mapping and never uses the ambient active gh account. temote-mcp requests local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"remote":{"type":"string","default":"origin"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"git_pull","title":"Fast-forward Git branch","description":"Run git pull --ff-only for the current branch and its configured upstream on the host. Hooks are disabled. A GitHub HTTPS upstream additionally requires the repository-local managed Git credential mapping and never uses the ambient active gh account. temote-mcp requests local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"git_push","title":"Push current Git branch","description":"Push the current branch on the host without force options. Optionally set origin (or another safe configured remote) as the upstream. Hooks are disabled. A GitHub HTTPS remote additionally requires the repository-local managed Git credential mapping and never uses the ambient active gh account. temote-mcp requests local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"remote":{"type":"string"},"set_upstream":{"type":"boolean","default":false}},"required":["session_id"],"additionalProperties":false}},
        {"name":"git_push_tag","title":"Push an exact Git tag ref","description":"Push one exact commit SHA to refs/tags/<tag> on a configured remote using force-with-lease safety. Omitting expected_remote_sha is create-only; supplying it permits an update only when the remote tag still equals that exact SHA. Arbitrary refspecs, URLs, and unconditional force are unavailable.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"remote":{"type":"string","default":"origin"},"tag":{"type":"string","minLength":1,"maxLength":255},"source_sha":{"type":"string","minLength":40,"maxLength":64},"expected_remote_sha":{"type":"string","minLength":40,"maxLength":64}},"required":["session_id","tag","source_sha"],"additionalProperties":false}},
        {"name":"git_branch_create","title":"Create a local Git branch","description":"Create one validated local branch from HEAD or a validated local/fetched repository ref. The operation exposes no force/reset/refspec/URL input and does not switch the current worktree.","annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"branch":{"type":"string","minLength":1,"maxLength":255},"base":{"type":"string","minLength":1,"maxLength":512}},"required":["session_id","branch"],"additionalProperties":false}},
        {"name":"git_switch","title":"Switch to an existing local Git branch","description":"Switch the current worktree to one validated existing local branch without force/reset/stash. Git refuses an unsafe switch when dirty files would be overwritten.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"branch":{"type":"string","minLength":1,"maxLength":255}},"required":["session_id","branch"],"additionalProperties":false}},
        {"name":"git_worktree_add","title":"Create a repository-owned Git worktree","description":"Create a linked worktree only at <repository>/.wt/<name>. If base is provided, create the validated branch from that local/fetched repository ref; otherwise attach an existing validated local branch. Arbitrary paths and force options are unavailable.","annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"name":{"type":"string","minLength":1,"maxLength":64},"branch":{"type":"string","minLength":1,"maxLength":255},"base":{"type":"string","minLength":1,"maxLength":512}},"required":["session_id","name","branch"],"additionalProperties":false}},
        {"name":"git_worktree_create","title":"Create a Temote-managed Git worktree","description":"Create a linked worktree only below the selected repository's exact managed root (<configured src root>/worktrees/<repository>/<task>, normally ~/src/worktrees/<repo>/<task>). Only one validated existing local branch can be attached; create a new branch with git_branch_create first. The task directory is derived from the branch when task is omitted; branch '/' never becomes directory hierarchy. Callers cannot choose a filesystem path, cwd or base, and legacy worktrees such as <repository>/.wt/<name> are never moved, adopted or deleted.","annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"repository":{"type":"string","minLength":1,"maxLength":255},"branch":{"type":"string","minLength":1,"maxLength":255},"task":{"type":"string","minLength":1,"maxLength":64}},"required":["session_id","branch"],"additionalProperties":false}},
        {"name":"git_worktree_list","title":"List repository worktrees by Temote classification","description":"List the selected repository's registered worktrees as primary, managed (canonically contained below the exact trusted managed root with matching repository identity) or legacy. Read-only; legacy worktrees are reported but never moved, adopted or deleted.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"repository":{"type":"string","minLength":1,"maxLength":255}},"required":["session_id"],"additionalProperties":false}},
        {"name":"git_worktree_remove","title":"Remove a clean Temote-managed Git worktree","description":"Remove one known Temote-managed linked worktree of the selected repository below the exact trusted managed root. The target is always derived by broker policy: task selects the direct child and an optional path is accepted only when it equals that derived path. Primary checkouts, legacy worktrees, unknown or wrong-repository targets, symlinked or swapped paths, the current session working directory, worktrees owned by another active session or running job, and dirty or untracked worktrees are refused. Branches and remote refs are never deleted, no stash/reset/clean/force is performed, and sibling worktrees are preserved.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"repository":{"type":"string","minLength":1,"maxLength":255},"task":{"type":"string","minLength":1,"maxLength":64},"path":{"type":"string","minLength":1,"maxLength":4096}},"required":["session_id"],"additionalProperties":false}},
        {"name":"git_worktree_prune","title":"Prune stale Git worktree metadata","description":"Run a bounded git worktree prune for the selected repository's canonical primary checkout. Only Git-classified stale worktree metadata is removed; filesystem directories are never deleted, and live managed, dirty, active-session and legacy worktrees are preserved. Caller input is path-free and the result reports bounded before/after identities and counts.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"repository":{"type":"string","minLength":1,"maxLength":255}},"required":["session_id"],"additionalProperties":false}},
        {"name":"github_workflow_dispatch","title":"Dispatch a GitHub Actions workflow","description":"Dispatch an exact workflow file or numeric workflow ID at an exact branch/tag ref for the GitHub repository resolved from a configured remote. Requires the repository-local managed Git credential mapping, never the ambient active gh account, and returns the created workflow run ID without exposing tokens.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"remote":{"type":"string","default":"origin"},"workflow":{"type":"string","minLength":1,"maxLength":255},"ref":{"type":"string","minLength":1,"maxLength":255}},"required":["session_id","workflow","ref"],"additionalProperties":false}},
        {"name":"github_workflow_run_get","title":"Read a GitHub Actions workflow run","description":"Read bounded status for one exact workflow run ID in the GitHub repository resolved from a configured remote. Requires the same repository-local managed Git credential mapping and never exposes tokens.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"remote":{"type":"string","default":"origin"},"run_id":{"type":"string","minLength":1,"maxLength":20}},"required":["session_id","run_id"],"additionalProperties":false}},
        {"name":"execute","title":"Run a command","description":"Execute argv without a shell using the selected session permission mode. Optional output_limit_bytes or status_only bounds the parent-facing result while preserving scoped evidence for omitted captured output. Returns the normal result when it finishes within 30 seconds; otherwise returns a job_id.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"},"output_limit_bytes":{"type":"integer","minimum":256,"maximum":1048576},"status_only":{"type":"boolean","default":false}},"required":["session_id","command"],"additionalProperties":false}},
        {"name":"start_command","title":"Start a command","description":"Start argv immediately as a background job using the selected session permission mode. Optional output_limit_bytes or status_only becomes the default completed-result view for later polls.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"},"output_limit_bytes":{"type":"integer","minimum":256,"maximum":1048576},"status_only":{"type":"boolean","default":false}},"required":["session_id","command"],"additionalProperties":false}},
        {"name":"poll_job","title":"Poll a sandbox job","description":"Poll a background command returned by execute or start_command. Optional output_limit_bytes or status_only can request a stricter completed-result view; omitted options reuse the job's stored default view.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"job_id":{"type":"string"},"output_limit_bytes":{"type":"integer","minimum":256,"maximum":1048576},"status_only":{"type":"boolean"}},"required":["session_id","job_id"],"additionalProperties":false}},
        {"name":"job_list","title":"List current-session sandbox jobs","description":"Return a bounded redacted snapshot of in-memory sandbox jobs owned by this session. Command text and job output are never included.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":128,"default":50}},"required":["session_id"],"additionalProperties":false}},
        {"name":"checkpoint_save","title":"Save a scoped work checkpoint","description":"Persist a bounded client-reported work checkpoint scoped to the current canonical working directory. A mandatory operation_id makes exact retries idempotent. Normal sessions require local approval.","annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":checkpoint_save_input_schema()},
        {"name":"checkpoint_load","title":"Load a scoped work checkpoint","description":"Read one client-reported checkpoint only when it belongs to the current canonical working directory.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":checkpoint_load_input_schema()},
        {"name":"work_handoff","title":"Read a work handoff snapshot","description":"Project scoped client-reported checkpoint state together with a redacted live snapshot of current-session jobs and best-effort local learning recall for a selected checkpoint. This tool does not execute or revalidate reported work.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":work_handoff_input_schema()},
        {"name":"friction_summary","title":"Summarize execution friction","description":"Return a bounded, explainable score derived only from secret-free execution metadata observed for the current session and scope. Command argv, output, file contents, prompts, and approval bodies are not stored in the friction event stream.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"learning_candidate_list","title":"List derived learning candidates","description":"Derive review-only learning candidates from bounded friction events. Candidates never become authoritative learning automatically and contain no transcript or command output.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"recall","title":"Recall repo-managed learnings","description":"Rebuild a deterministic local index from bounded Markdown files under a configured knowledge root inside the session roots and return explainable matches. No network or embedding service is used.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"query":{"type":"string","minLength":1,"maxLength":2048},"knowledge_root":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":20,"default":5}},"required":["session_id","query"],"additionalProperties":false}},
        {"name":"recall_feedback","title":"Record a recall knowledge-gap signal","description":"Persist only a client-reported no-hit signal, with an optional opaque retry-group UUID. Query text and recall results are not persisted. A no-hit signal alone never creates a learning candidate. Normal sessions require local approval.","annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"outcome":{"type":"string","enum":["no_hit"]},"retry_group":{"type":"string","format":"uuid"}},"required":["session_id","outcome"],"additionalProperties":false}},
        {"name":"stop_job","title":"Stop a sandbox job","description":"Stop a background command returned by execute or start_command.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"job_id":{"type":"string"}},"required":["session_id","job_id"],"additionalProperties":false}},
        {"name":"onepassword_mcp_discover","title":"Discover 1Password MCP","description":"List resources and tool schemas exposed by the official local 1Password Environments MCP server. Start with this tool before using 1Password MCP tools.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"onepassword_mcp_read_resource","title":"Read a 1Password MCP resource","description":"Read a documentation resource exposed by the official local 1Password Environments MCP server.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"uri":{"type":"string"}},"required":["session_id","uri"],"additionalProperties":false}},
        {"name":"onepassword_mcp_call","title":"Call a 1Password MCP tool","description":"Call a tool exposed by the official local 1Password Environments MCP server. Non-read-only child tools require temote-mcp approval unless the session is in yolo mode. Raw secrets remain governed by 1Password's MCP server contract.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"tool_name":{"type":"string"},"arguments":{"type":"object","additionalProperties":true}},"required":["session_id","tool_name","arguments"],"additionalProperties":false}},
        {"name":"onepassword_item_get","title":"Batch-read 1Password items","description":"Read up to 100 1Password items by exact ID or title through the official op CLI. Temote resolves the requested items and fetches them in one batch; returned JSON may contain secret values. Normal sessions require local approval.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"items":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":100},"vault":{"type":"string"},"account":{"type":"string"}},"required":["session_id","items"],"additionalProperties":false}},
        {"name":"onepassword_secret_resolve","title":"Resolve 1Password secrets","description":"Resolve up to 100 op:// secret references. On macOS, Temote reuses a persistent 1Password Desktop SDK sidecar when authorized and falls back to one batched official op CLI path when the SDK is unavailable. Returned strings are secrets; normal sessions require local approval.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"account":{"type":"string"},"references":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":100}},"required":["session_id","account","references"],"additionalProperties":false}},
        {"name":"onepassword_service_account_status","title":"Check 1Password service account","description":"Check whether this temote-mcp session was started with a 1Password service-account token and whether a process-isolated 1Password CLI accepts it. The token is never returned.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"onepassword_service_account_run","title":"Run with 1Password service-account secrets","description":"Resolve reviewed op:// inputs with the service-account token held only by a process-inspection-protected Temote process, then launch the target without OP_SERVICE_ACCOUNT_TOKEN. Linux raw-token CLI calls additionally require a protected setgid 1Password CLI installation, and Linux service-account targets run with a private PID namespace/private /proc. Optional allowed_locators exposes only pre-resolved exact references through a process-tree-bound per-invocation Linux broker. Normal sessions require local approval; yolo sessions do not.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"},"env_files":{"type":"array","items":{"type":"string"}},"environment":{"type":"object","additionalProperties":{"type":"string"},"description":"Environment variable names mapped to op:// secret references. Plaintext values are rejected."},"allowed_locators":{"type":"array","items":{"type":"string"},"maxItems":128,"description":"Exact op:// references the launched process may resolve after startup through the per-invocation Temote secret resolver. Linux only; omitted keeps existing behavior."}},"required":["session_id","command"],"additionalProperties":false}},
        {"name":"kintone_mcp_status","title":"Check kintone MCP","description":"Check whether the selected temote-mcp session has the official kintone MCP server executable and required authentication configuration. Credential values are never returned.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"kintone_mcp_discover","title":"Discover kintone MCP","description":"List tool schemas exposed by the official kintone MCP server using credentials retained only by the selected temote-mcp start process.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"kintone_mcp_call","title":"Call a kintone MCP tool","description":"Call a tool exposed by the official kintone MCP server. All child tool calls are host-approval-gated in normal temote-mcp sessions because the upstream server does not currently annotate read-only versus mutating tools.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"tool_name":{"type":"string"},"arguments":{"type":"object","additionalProperties":true}},"required":["session_id","tool_name","arguments"],"additionalProperties":false}},
        {"name":"kintone_cli_status","title":"Check cli-kintone","description":"Check whether the selected temote-mcp session has cli-kintone plus kintone authentication configuration, and list the supported API-backed command pairs. Credential values and tenant URL are never returned.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"kintone_cli_run","title":"Run cli-kintone","description":"Run an allow-listed API-backed cli-kintone command using credentials held only by the temote-mcp start process. Supports record export/import/delete, customize export/apply, and plugin upload. Secret-bearing connection/auth options are rejected; file arguments and optional stdout_path must stay within permitted roots in normal sessions. All runs require local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"arguments":{"type":"array","items":{"type":"string"},"minItems":2,"description":"cli-kintone arguments excluding the executable, beginning with a supported command pair such as [\"record\",\"export\",...]."},"cwd":{"type":"string"},"stdout_path":{"type":"string","description":"Optional file path for record export stdout. Written atomically on success; rejected for other command pairs."}},"required":["session_id","arguments"],"additionalProperties":false}},
        {"name":"without_sandbox","title":"Run a host command","description":"Execute argv directly on the host with the local user's permissions and network access. temote-mcp requests local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"],"additionalProperties":false}}
    ]);
    if public {
        tools
            .as_array_mut()
            .unwrap()
            .retain(|tool| tool["name"] != "without_sandbox");
    }
    if !managed_sessions {
        tools.as_array_mut().unwrap().retain(|tool| {
            !matches!(
                tool["name"].as_str(),
                Some("session_start" | "session_stop" | "session_restart")
            )
        });
    }
    tools
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .filter(|tool| tool["name"] != "session_list")
        .for_each(|tool| {
            if let Some(session_id) = tool
                .pointer_mut("/inputSchema/properties/session_id")
                .and_then(Value::as_object_mut)
            {
                session_id.remove("format");
            }
        });
    tools
}

async fn call_tool(
    params: &Value,
    public: bool,
    sessions: Option<&SessionBackend>,
) -> Result<Value> {
    call_tool_with_local_agent_executable(params, public, sessions, None).await
}

#[cfg(test)]
async fn call_tool_with_test_local_agent_executable(
    params: &Value,
    public: bool,
    sessions: Option<&SessionBackend>,
    executable: &Path,
) -> Result<Value> {
    call_tool_with_local_agent_executable(params, public, sessions, Some(executable)).await
}

async fn call_tool_with_local_agent_executable(
    params: &Value,
    public: bool,
    sessions: Option<&SessionBackend>,
    local_agent_executable: Option<&Path>,
) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("missing tool name")?;
    validate_mcp_tool_name(name)?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    reap_jobs();
    if name == "session_list" {
        assert_non_dispatch_activity_owner(name, ActivityNonDispatchOwner::Excluded);
        anyhow::ensure!(
            args.as_object().is_some_and(|object| object.is_empty()),
            "session_list takes no arguments"
        );
        return session_list(sessions).await;
    }
    if name == "session_start" {
        assert_non_dispatch_activity_owner(name, ActivityNonDispatchOwner::Supervisor);
        anyhow::ensure!(
            public,
            "session_start is available only from temote-mcp serve"
        );
        let sessions = sessions.context("session supervisor is unavailable")?;
        let object = args
            .as_object()
            .context("session_start arguments must be an object")?;
        anyhow::ensure!(
            object
                .keys()
                .all(|key| matches!(key.as_str(), "path" | "session_id")),
            "session_start accepts only path and session_id"
        );
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .context("missing path")?;
        validate_path_argument(path, "path")?;
        let session_id = object
            .get("session_id")
            .map(|value| value.as_str().context("session_id must be a string"))
            .transpose()?;
        let info = sessions.start(path, session_id).await?;
        return text_result(serde_json::to_string_pretty(&info)?);
    }
    if name == "session_restart" {
        assert_non_dispatch_activity_owner(name, ActivityNonDispatchOwner::Supervisor);
        anyhow::ensure!(
            public,
            "session_restart is available only from temote-mcp serve"
        );
        let sessions = sessions.context("session supervisor is unavailable")?;
        let object = args
            .as_object()
            .context("session_restart arguments must be an object")?;
        anyhow::ensure!(
            object.keys().all(|key| key == "session_id"),
            "session_restart accepts only session_id"
        );
        let session_id = object
            .get("session_id")
            .and_then(Value::as_str)
            .context("missing session_id")?;
        config::validate_session_id(session_id)?;
        let info = sessions.restart(session_id).await?;
        return text_result(serde_json::to_string_pretty(&info)?);
    }
    if name == "session_stop" {
        assert_non_dispatch_activity_owner(name, ActivityNonDispatchOwner::Supervisor);
        anyhow::ensure!(
            public,
            "session_stop is available only from temote-mcp serve"
        );
        let sessions = sessions.context("session supervisor is unavailable")?;
        let object = args
            .as_object()
            .context("session_stop arguments must be an object")?;
        anyhow::ensure!(
            object.keys().all(|key| key == "session_id"),
            "session_stop accepts only session_id"
        );
        let session_id = object
            .get("session_id")
            .and_then(Value::as_str)
            .context("missing session_id")?;
        sessions.stop(session_id).await?;
        return text_result(serde_json::to_string_pretty(&json!({
            "session_id": session_id,
            "status": "stopped"
        }))?);
    }
    anyhow::ensure!(
        !public || name != "without_sandbox",
        "without_sandbox is unavailable on the public MCP endpoint"
    );
    let session_id = required_session_id(&args)?;
    if name == "session_info" {
        assert_non_dispatch_activity_owner(name, ActivityNonDispatchOwner::Excluded);
        let view = crate::session_control::inspect_session(&session_id).await?;
        if matches!(view.status.as_str(), "starting" | "active" | "stopping") {
            approvals::activity(&view.session_id, "Read session info", None).await;
        }
        let mut rendered = serde_json::to_value(&view)?;
        rendered["server_contract_fingerprint"] = json!(public_contract_fingerprint());
        return text_result(serde_json::to_string_pretty(&rendered)?);
    }
    let session = config::load_session(&session_id).await?;
    anyhow::ensure!(
        !public || !session.yolo(),
        "yolo sessions are unavailable on the public MCP endpoint"
    );
    let coverage = activity_tool_coverage(name);
    let activity = if let Some(coverage) = coverage {
        tool_activity_scope(
            &session,
            coverage.operation,
            activity_tool_summary(&args, coverage.operation),
        )
        .await
    } else {
        None
    };
    let result = async {
        match name {
            "get_image" => {
                let path = config::resolve_existing_path(&session, &required_path(&args, "path")?)?;
                let result = get_image(&path).await;
                report_result(
                    &session.id,
                    format!("Read image {}", display_path(&path, &session.cwd)),
                    &result,
                )
                .await;
                result
            }
            "read_file" => read_file_tool(&args, &session).await,
            "evidence_read" => evidence_read_tool(&args, &session),
            "codex_status" => {
                let (detail, metadata) = codex_status_approval();
                authorize_codex_operation(
                    &session,
                    "codex_status",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &codex_app_server::status(&session).await?,
                )?)
            }
            "codex_task_start" => {
                let (detail, metadata) = codex_task_start_approval(&args);
                authorize_codex_operation(
                    &session,
                    "codex_task_start",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &codex_app_server::task_start(&args, &session).await?,
                )?)
            }
            "codex_task_get" => text_result(serde_json::to_string_pretty(
                &codex_app_server::task_get(&args, &session).await?,
            )?),
            "codex_task_control" => {
                let (detail, metadata) = codex_task_control_approval(&args);
                authorize_codex_operation(
                    &session,
                    "codex_task_control",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &codex_app_server::task_control(&args, &session).await?,
                )?)
            }
            "local_agent_run" => {
                local_agent_run(&args, &session, local_agent_executable, activity.clone()).await
            }
            "dev_tool_run" => dev_tool_run(&args, &session, activity.clone()).await,
            "list_directory" => {
                let path = config::resolve_existing_path(&session, &required_path(&args, "path")?)?;
                let result = list_directory(&path).await;
                report_result(
                    &session.id,
                    format!("Listed {}", display_path(&path, &session.cwd)),
                    &result,
                )
                .await;
                text_result(result?)
            }
            "write_file" => write_file(&args, &session, activity.as_ref()).await,
            "apply_patch" => {
                let request = apply_patch::parse_request(&args)?;
                let outcome =
                    apply_patch::apply_with_activity(&session, request, activity.as_ref()).await?;
                if outcome.status == "partial_failure"
                    && let Some(activity) = activity.as_ref()
                {
                    let _ = activity.fail_with_summary(ActivitySummary::failure(
                        ActivityErrorKind::OperationFailed,
                    ));
                }
                text_result(serde_json::to_string_pretty(&outcome)?)
            }
            name @ ("git_add"
            | "git_commit"
            | "git_fetch"
            | "git_pull"
            | "git_push"
            | "git_push_tag"
            | "git_branch_create"
            | "git_switch"
            | "git_worktree_add"
            | "git_worktree_create"
            | "git_worktree_list"
            | "git_worktree_remove"
            | "git_worktree_prune") => {
                let operation =
                    git_activity_operation(name).expect("matched Git activity operation");
                match operation {
                    ActivityOperation::GitAdd => git_add(&args, &session, activity.as_ref()).await,
                    ActivityOperation::GitCommit => {
                        git_commit(&args, &session, activity.as_ref()).await
                    }
                    ActivityOperation::GitFetch => {
                        git_fetch(&args, &session, activity.as_ref()).await
                    }
                    ActivityOperation::GitPull => {
                        git_pull(&args, &session, activity.as_ref()).await
                    }
                    ActivityOperation::GitPush if name == "git_push_tag" => {
                        git_push_tag(&args, &session, activity.as_ref()).await
                    }
                    ActivityOperation::GitPush => {
                        git_push(&args, &session, activity.as_ref()).await
                    }
                    ActivityOperation::GitBranchCreate => {
                        git_branch_create(&args, &session, activity.as_ref()).await
                    }
                    ActivityOperation::GitSwitch => {
                        git_switch(&args, &session, activity.as_ref()).await
                    }
                    ActivityOperation::GitWorktreeAdd => {
                        git_worktree_add(&args, &session, activity.as_ref()).await
                    }
                    ActivityOperation::GitWorktreeCreate => {
                        git_worktree_create(&args, &session, activity.as_ref()).await
                    }
                    ActivityOperation::GitWorktreeList => {
                        git_worktree_list(&args, &session, activity.as_ref()).await
                    }
                    ActivityOperation::GitWorktreeRemove => {
                        git_worktree_remove(&args, &session, activity.as_ref()).await
                    }
                    ActivityOperation::GitWorktreePrune => {
                        git_worktree_prune(&args, &session, activity.as_ref()).await
                    }
                    _ => unreachable!("Git operation mapping returned a non-Git variant"),
                }
            }
            "github_workflow_dispatch" => {
                github_workflow_dispatch(&args, &session, activity.as_ref()).await
            }
            "github_workflow_run_get" => {
                github_workflow_run_get(&args, &session, activity.as_ref()).await
            }
            "execute" => execute(&args, &session, activity.clone()).await,
            "start_command" => start_command(&args, &session, activity.clone()).await,
            "poll_job" => poll_job(&args, &session).await,
            "job_list" => job_list(&args, &session),
            "checkpoint_save" => {
                let request = checkpoints::parse_save_request(&args)?;
                anyhow::ensure!(request.session_id == session.id, "session ID mismatch");
                let approval_detail = checkpoints::approval_detail(&request.checkpoint);
                request_activity_approval(
                    &session,
                    ActivityApprovalRequest {
                        class: approvals::ApprovalClass::LocalStructured,
                        operation: "checkpoint_save",
                        detail: approval_detail,
                        cwd: session.cwd.clone(),
                        metadata: BTreeMap::new(),
                        denial: "user denied checkpoint save",
                    },
                    activity.as_ref(),
                )
                .await?;
                let store = checkpoints::Store::default_store()?;
                let (saved, activity_detail) =
                    save_checkpoint_after_approval(&session, request, &store, true)?;
                approvals::activity(
                    &session.id,
                    "Saved client-reported checkpoint",
                    Some(activity_detail),
                )
                .await;
                text_result(serde_json::to_string_pretty(&saved)?)
            }
            "checkpoint_load" => {
                let request = checkpoints::parse_load_request(&args)?;
                anyhow::ensure!(request.session_id == session.id, "session ID mismatch");
                let loaded = checkpoints::load(&session, request.checkpoint_id)?;
                approvals::activity(&session.id, "Loaded client-reported checkpoint", None).await;
                text_result(serde_json::to_string_pretty(&loaded)?)
            }
            "work_handoff" => {
                let request = work_handoff::parse_request(&args)?;
                text_result(work_handoff::render(&session, request)?)
            }
            "friction_summary" => {
                let store = friction::Store::default_store()?;
                let summary = store.summary(&session)?;
                text_result(serde_json::to_string_pretty(&summary)?)
            }
            "learning_candidate_list" => {
                let store = friction::Store::default_store()?;
                let candidates = store.candidates(&session)?;
                text_result(serde_json::to_string_pretty(&candidates)?)
            }
            "recall" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .context("missing query")?;
                let knowledge_root = args.get("knowledge_root").and_then(Value::as_str);
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(5) as usize;
                let response = recall::search(&session, query, knowledge_root, limit)?;
                text_result(serde_json::to_string_pretty(&response)?)
            }
            "recall_feedback" => {
                anyhow::ensure!(
                    args.get("outcome").and_then(Value::as_str) == Some("no_hit"),
                    "recall_feedback outcome must be no_hit"
                );
                let retry_group = args
                    .get("retry_group")
                    .and_then(Value::as_str)
                    .map(Uuid::parse_str)
                    .transpose()
                    .context("retry_group must be a UUID")?;
                request_activity_approval(
                    &session,
                    ActivityApprovalRequest {
                        class: approvals::ApprovalClass::LocalStructured,
                        operation: "recall_feedback",
                        detail: "signal: no_hit; query/content: not persisted".to_owned(),
                        cwd: session.cwd.clone(),
                        metadata: BTreeMap::new(),
                        denial: "user denied recall feedback persistence",
                    },
                    activity.as_ref(),
                )
                .await?;
                let event = friction::record_client_reported_recall_miss(&session, retry_group)?;
                text_result(serde_json::to_string_pretty(&event)?)
            }
            "stop_job" => stop_job_with_activity(&args, &session, activity.as_ref()).await,
            "onepassword_mcp_discover" => {
                let result = onepassword_mcp::discover(&session).await?;
                text_result(serde_json::to_string_pretty(&result)?)
            }
            "onepassword_mcp_read_resource" => {
                let uri = args
                    .get("uri")
                    .and_then(Value::as_str)
                    .context("missing uri")?;
                let result = onepassword_mcp::read_resource(&session, uri).await?;
                text_result(serde_json::to_string_pretty(&result)?)
            }
            "onepassword_mcp_call" => {
                let tool_name = args
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .context("missing tool_name")?;
                let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
                onepassword_mcp::call_tool_with_activity(
                    &session,
                    tool_name,
                    arguments,
                    activity.as_ref(),
                )
                .await
            }
            "onepassword_item_get" => {
                let items = required_string_array(&args, "items")?;
                let vault = args
                    .get("vault")
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .context("vault must be a string")
                    })
                    .transpose()?;
                let account = args
                    .get("account")
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .context("account must be a string")
                    })
                    .transpose()?;
                let request = onepassword_cli::ItemGetRequest::new(items, vault, account)?;
                request_activity_approval(
                    &session,
                    ActivityApprovalRequest {
                        class: approvals::ApprovalClass::Integration,
                        operation: "onepassword_item_get",
                        detail: request.approval_summary(),
                        cwd: session.cwd.clone(),
                        metadata: BTreeMap::new(),
                        denial: "user denied 1Password item read",
                    },
                    activity.as_ref(),
                )
                .await?;
                match onepassword_cli::item_get_coalesced(&session, &request).await {
                    Ok(items) => {
                        approvals::activity(
                            &session.id,
                            format!("Read {} 1Password item(s)", items.len()),
                            None,
                        )
                        .await;
                        text_result(serde_json::to_string_pretty(&items)?)
                    }
                    Err(error) => {
                        approvals::activity(&session.id, "1Password item read failed", None).await;
                        Err(error)
                    }
                }
            }
            "onepassword_secret_resolve" => {
                let account = args
                    .get("account")
                    .and_then(Value::as_str)
                    .context("missing account")?
                    .to_owned();
                let references = required_string_array(&args, "references")?;
                let request = onepassword_sdk::ResolveRequest::new(account, references)?;
                request_activity_approval(
                    &session,
                    ActivityApprovalRequest {
                        class: approvals::ApprovalClass::Integration,
                        operation: "onepassword_secret_resolve",
                        detail: request.approval_summary(),
                        cwd: session.cwd.clone(),
                        metadata: BTreeMap::new(),
                        denial: "user denied 1Password secret resolution",
                    },
                    activity.as_ref(),
                )
                .await?;
                match onepassword_sdk::resolve(&session, &request).await {
                    Ok(values) => {
                        approvals::activity(
                            &session.id,
                            format!("Resolved {} 1Password secret(s)", values.len()),
                            None,
                        )
                        .await;
                        text_result(serde_json::to_string_pretty(&values)?)
                    }
                    Err(error) => {
                        approvals::activity(
                            &session.id,
                            "1Password secret resolution failed",
                            None,
                        )
                        .await;
                        Err(error)
                    }
                }
            }
            "onepassword_service_account_status" => {
                let result = approvals::onepassword_service_account_status(&session.id).await?;
                text_result(serde_json::to_string_pretty(&result)?)
            }
            "onepassword_service_account_run" => {
                let command = required_command(&args)?;
                let cwd = cwd(&args, &session)?;
                let env_files = args
                    .get("env_files")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .map(|item| {
                                let value =
                                    item.as_str().context("env_files entries must be strings")?;
                                bounded_path(value, "env_files entry")
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                let environment = args
                    .get("environment")
                    .map(|value| {
                        value
                            .as_object()
                            .context("environment must be an object")?
                            .iter()
                            .map(|(name, value)| {
                                value
                                    .as_str()
                                    .map(|value| (name.clone(), value.to_owned()))
                                    .context("environment values must be strings")
                            })
                            .collect::<Result<std::collections::BTreeMap<_, _>>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                let allowed_locators = args
                    .get("allowed_locators")
                    .map(|_| required_string_array(&args, "allowed_locators"))
                    .transpose()?
                    .unwrap_or_default();
                approvals::validate_service_account_run_input(
                    &command,
                    &env_files,
                    &environment,
                    &allowed_locators,
                )?;
                let detail = service_account_approval_detail(
                    &command,
                    &env_files,
                    &environment,
                    &allowed_locators,
                )?;
                request_activity_approval(
                    &session,
                    ActivityApprovalRequest {
                        class: approvals::ApprovalClass::Integration,
                        operation: "onepassword_service_account_run",
                        detail,
                        cwd: cwd.clone(),
                        metadata: BTreeMap::new(),
                        denial: "user denied 1Password service-account command",
                    },
                    activity.as_ref(),
                )
                .await?;
                let result = approvals::onepassword_service_account_run(
                    &session.id,
                    cwd,
                    command,
                    env_files,
                    environment,
                    allowed_locators,
                )
                .await?;
                text_result(serde_json::to_string_pretty(&result)?)
            }
            "kintone_mcp_status" => {
                let result = approvals::kintone_mcp_status(&session.id).await?;
                text_result(serde_json::to_string_pretty(&result)?)
            }
            "kintone_mcp_discover" => {
                let result = approvals::kintone_mcp_discover(&session.id).await?;
                approvals::activity(&session.id, "Discovered kintone MCP capabilities", None).await;
                text_result(serde_json::to_string_pretty(&result)?)
            }
            "kintone_mcp_call" => {
                let tool_name = args
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .context("missing tool_name")?;
                let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
                validate_child_tool_call(tool_name, &arguments)
                    .context("invalid kintone MCP tool call")?;
                let listed = approvals::kintone_mcp_discover(&session.id).await?;
                let known = listed["tools"].as_array().is_some_and(|tools| {
                    tools
                        .iter()
                        .any(|tool| tool["name"].as_str() == Some(tool_name))
                });
                anyhow::ensure!(known, "unknown kintone MCP tool: {tool_name}");
                request_activity_approval(
                    &session,
                    ActivityApprovalRequest {
                        class: approvals::ApprovalClass::Integration,
                        operation: "kintone_mcp_call",
                        detail: safe_child_call_summary(tool_name, &arguments),
                        cwd: session.cwd.clone(),
                        metadata: BTreeMap::new(),
                        denial: "user denied kintone MCP tool call",
                    },
                    activity.as_ref(),
                )
                .await?;
                let result = approvals::kintone_mcp_call(&session.id, tool_name, arguments).await?;
                approvals::activity(
                    &session.id,
                    format!("Called kintone MCP tool {tool_name}"),
                    None,
                )
                .await;
                Ok(result)
            }
            "kintone_cli_status" => {
                let result = approvals::kintone_cli_status(&session.id).await?;
                text_result(serde_json::to_string_pretty(&result)?)
            }
            "kintone_cli_run" => {
                let arguments = args
                    .get("arguments")
                    .and_then(Value::as_array)
                    .context("missing arguments")?
                    .iter()
                    .map(|argument| {
                        argument
                            .as_str()
                            .map(str::to_owned)
                            .context("arguments entries must be strings")
                    })
                    .collect::<Result<Vec<_>>>()?;
                anyhow::ensure!(
                    arguments.len() >= 2,
                    "kintone_cli_run requires a cli-kintone command pair"
                );
                validate_command_budget(&arguments)?;
                let cwd = cwd(&args, &session)?;
                let stdout_path = args
                    .get("stdout_path")
                    .map(|value| {
                        let value = value.as_str().context("stdout_path must be a string")?;
                        bounded_path(value, "stdout_path")
                    })
                    .transpose()?;
                request_activity_approval(
                    &session,
                    ActivityApprovalRequest {
                        class: approvals::ApprovalClass::Integration,
                        operation: "kintone_cli_run",
                        detail: safe_kintone_cli_summary(&arguments, stdout_path.as_deref()),
                        cwd: cwd.clone(),
                        metadata: BTreeMap::new(),
                        denial: "user denied cli-kintone command",
                    },
                    activity.as_ref(),
                )
                .await?;
                let result =
                    approvals::kintone_cli_run(&session.id, cwd, arguments.clone(), stdout_path)
                        .await?;
                approvals::activity(
                    &session.id,
                    format!("Ran cli-kintone {} {}", arguments[0], arguments[1]),
                    None,
                )
                .await;
                text_result(serde_json::to_string_pretty(&result)?)
            }
            "without_sandbox" => without_sandbox(&args, &session, activity.as_ref()).await,
            _ => anyhow::bail!("unknown tool: {name}"),
        }
    }
    .await;
    finish_covered_tool_activity(coverage, activity.as_ref(), &result);
    result
}

async fn session_list(sessions: Option<&SessionBackend>) -> Result<Value> {
    let views = match sessions {
        Some(backend) => backend.list().await?,
        None => crate::session_control::session_views_for_mcp().await?,
    };
    let mut sessions = Vec::new();
    let mut session_bytes = 0usize;
    for session in views {
        push_session_list_entry(
            &mut sessions,
            &mut session_bytes,
            json!({
                "session_id": session.session_id,
                "cwd": session.cwd,
                "started_at": session.started_at,
                "stopped_at": session.stopped_at,
                "status": session.status,
                "pid": session.pid,
                "exit_reason": session.exit_reason,
                "last_error": session.last_error,
                "permission_mode": session.permission_mode,
                "yolo": session.yolo,
            }),
            MAX_SESSION_LIST_ENTRIES,
            MAX_SESSION_LIST_BYTES,
        )?;
    }
    let rendered = serde_json::to_string_pretty(&sessions)?;
    anyhow::ensure!(
        rendered.len() <= MAX_SESSION_LIST_BYTES,
        "session list exceeds {MAX_SESSION_LIST_BYTES} bytes"
    );
    text_result(rendered)
}

fn push_session_list_entry(
    sessions: &mut Vec<Value>,
    rendered_bytes: &mut usize,
    session: Value,
    max_entries: usize,
    max_bytes: usize,
) -> Result<()> {
    anyhow::ensure!(
        sessions.len() < max_entries,
        "session list exceeds {max_entries} entries"
    );
    let entry_bytes = serde_json::to_string_pretty(&session)?.len();
    let charged = entry_bytes
        .checked_add(64)
        .context("session list entry size overflow")?;
    let next = rendered_bytes
        .checked_add(charged)
        .context("session list size overflow")?;
    anyhow::ensure!(next <= max_bytes, "session list exceeds {max_bytes} bytes");
    sessions.push(session);
    *rendered_bytes = next;
    Ok(())
}

async fn report_result<T>(session_id: &str, title: String, result: &Result<T>) {
    let detail = result
        .as_ref()
        .err()
        .map(|error| format!("└ Error: {error:#}"));
    approvals::activity(session_id, title, detail).await;
}

fn display_path<'a>(path: &'a Path, session_cwd: &Path) -> std::borrow::Cow<'a, str> {
    path.strip_prefix(session_cwd)
        .unwrap_or(path)
        .to_string_lossy()
}

async fn authorize_codex_operation(
    session: &config::Session,
    action: &str,
    detail: String,
    metadata: BTreeMap<String, String>,
    activity: Option<&ActivityScope>,
) -> Result<()> {
    let approved = approvals::ensure_local_approval_with_activity(
        session,
        approvals::ApprovalClass::CodexAppServer,
        action,
        detail,
        session.cwd.clone(),
        metadata,
        activity,
    )
    .await?;
    finish_activity_approval(approved, activity, "user denied Codex operation")?;
    Ok(())
}

fn finish_activity_approval(
    approved: bool,
    activity: Option<&ActivityScope>,
    denial: &'static str,
) -> Result<()> {
    if !approved {
        if let Some(activity) = activity {
            let _ = activity
                .fail_with_summary(ActivitySummary::failure(ActivityErrorKind::ApprovalDenied));
        }
        anyhow::bail!(denial);
    }
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    Ok(())
}

struct ActivityApprovalRequest<'a> {
    class: approvals::ApprovalClass,
    operation: &'a str,
    detail: String,
    cwd: PathBuf,
    metadata: BTreeMap<String, String>,
    denial: &'static str,
}

async fn request_activity_approval(
    session: &config::Session,
    request: ActivityApprovalRequest<'_>,
    activity: Option<&ActivityScope>,
) -> Result<()> {
    let ActivityApprovalRequest {
        class,
        operation,
        detail,
        cwd,
        metadata,
        denial,
    } = request;
    let approved = match activity {
        Some(activity) => {
            approvals::ensure_local_approval_with_activity(
                session,
                class,
                operation,
                detail,
                cwd,
                metadata,
                Some(activity),
            )
            .await?
        }
        None => {
            approvals::ensure_local_approval(session, class, operation, detail, cwd, metadata)
                .await?
        }
    };
    finish_activity_approval(approved, activity, denial)
}

fn codex_status_approval() -> (String, BTreeMap<String, String>) {
    (
        "Codex delegation request\naccess: read-only\nscope: current session\nresult: model and effort compatibility metadata".to_owned(),
        codex_approval_metadata("codex_status", "status", false, "session_scope"),
    )
}

fn codex_task_start_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let operation_id = safe_codex_argument(args, "operation_id");
    let model = safe_codex_argument(args, "model");
    let effort = safe_codex_argument(args, "effort");
    let mut metadata =
        codex_approval_metadata("codex_task_start", "task_start", true, "session_scope");
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("model".to_owned(), model.clone());
    metadata.insert("effort".to_owned(), effort.clone());
    metadata.insert("task_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "Codex delegation request\noperation: start task\nmutation: workspace-write\nscope: current session working directory\nmodel: {model}\neffort: {effort}\noperation_id: {operation_id}\ntask input: omitted"
        ),
        metadata,
    )
}

fn codex_task_control_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let task_id = safe_codex_argument(args, "task_id");
    let operation_id = safe_codex_argument(args, "operation_id");
    let action = safe_codex_argument(args, "action");
    let mut metadata = codex_approval_metadata(
        "codex_task_control",
        "task_control",
        true,
        &format!("task:{task_id}"),
    );
    metadata.insert("task_id".to_owned(), task_id.clone());
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("action".to_owned(), action.clone());
    metadata.insert("control_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "Codex delegation request\noperation: control task\naction: {action}\nmutation: task control\ntarget: task {task_id}\nscope: current session working directory\noperation_id: {operation_id}\ncontrol input: omitted"
        ),
        metadata,
    )
}

fn codex_approval_metadata(
    tool: &str,
    operation_type: &str,
    mutation: bool,
    target: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("provenance".to_owned(), "codex_delegation".to_owned()),
        ("source".to_owned(), "codex_delegation".to_owned()),
        ("tool".to_owned(), tool.to_owned()),
        ("operation_type".to_owned(), operation_type.to_owned()),
        ("target".to_owned(), target.to_owned()),
        ("mutation".to_owned(), mutation.to_string()),
        ("read_only".to_owned(), (!mutation).to_string()),
        ("scope".to_owned(), "session_cwd".to_owned()),
    ])
}

fn safe_codex_argument(args: &Value, key: &str) -> String {
    let Some(value) = args.get(key).and_then(Value::as_str) else {
        return "(not provided)".to_owned();
    };
    let mut rendered = String::new();
    for character in value.chars() {
        let part = if character.is_control() {
            if character.is_ascii() {
                format!("\\x{:02x}", character as u32)
            } else {
                format!("\\u{{{:x}}}", character as u32)
            }
        } else {
            character.to_string()
        };
        if rendered.len().saturating_add(part.len()) > 256 {
            rendered.push('…');
            break;
        }
        rendered.push_str(&part);
    }
    rendered
}

fn text_result(text: String) -> Result<Value> {
    Ok(json!({"content":[{"type":"text","text":text}]}))
}

fn activity_tool_coverage(name: &str) -> Option<&'static ActivityToolCoverage> {
    ACTIVITY_TOOL_COVERAGE
        .iter()
        .find(|coverage| coverage.name == name)
}

fn assert_non_dispatch_activity_owner(name: &str, owner: ActivityNonDispatchOwner) {
    let coverage = ACTIVITY_NON_DISPATCH_COVERAGE
        .iter()
        .find(|coverage| coverage.name == name)
        .expect("known non-dispatch tool must have activity coverage");
    assert_eq!(coverage.owner, owner);
    assert!(!coverage.fixture.is_empty());
}

fn activity_tool_summary(args: &Value, operation: ActivityOperation) -> ActivitySummary {
    git_activity_summary(args, operation)
}

fn git_activity_summary(args: &Value, operation: ActivityOperation) -> ActivitySummary {
    match operation {
        ActivityOperation::GitFetch | ActivityOperation::GitPull | ActivityOperation::GitPush => {
            let remote = match args.get("remote").and_then(Value::as_str) {
                None | Some("origin") => ActivityRemote::Origin,
                Some(_) => ActivityRemote::Other,
            };
            ActivitySummary::git(remote)
        }
        _ => ActivitySummary::empty(),
    }
}

async fn tool_activity_scope(
    session: &config::Session,
    operation: ActivityOperation,
    summary: ActivitySummary,
) -> Option<ActivityScope> {
    activity_runtime::emitter(session)
        .await
        .ok()
        .map(|emitter| ActivityScope::with_summary(operation, summary, emitter))
}

#[cfg(test)]
fn finish_tool_activity(activity: Option<&ActivityScope>, result: &Result<Value>) {
    let Some(activity) = activity else {
        return;
    };
    let _ = if result.is_ok() {
        activity.complete()
    } else {
        activity.fail_with_summary(ActivitySummary::failure(ActivityErrorKind::OperationFailed))
    };
}

fn finish_tool_activity_on_error(activity: Option<&ActivityScope>, result: &Result<Value>) {
    if result.is_err()
        && let Some(activity) = activity
    {
        let _ = activity
            .fail_with_summary(ActivitySummary::failure(ActivityErrorKind::OperationFailed));
    }
}

fn finish_covered_tool_activity(
    coverage: Option<&ActivityToolCoverage>,
    activity: Option<&ActivityScope>,
    result: &Result<Value>,
) {
    let (Some(coverage), Some(activity)) = (coverage, activity) else {
        return;
    };
    if result.is_err() {
        finish_tool_activity_on_error(Some(activity), result);
        return;
    }
    if coverage.owner == ActivityOwner::JobWorker {
        return;
    }
    let _ = match coverage.success {
        ActivitySuccess::Completed => activity.complete(),
        ActivitySuccess::Accepted => {
            activity.complete_with_summary(ActivitySummary::result(ActivityResult::Accepted))
        }
    };
}

async fn read_file_tool(args: &Value, session: &config::Session) -> Result<Value> {
    let path = config::resolve_existing_path(session, &required_path(args, "path")?)?;
    let result = read_text_file(&path).await;
    report_result(
        &session.id,
        format!("Read {}", display_path(&path, &session.cwd)),
        &result,
    )
    .await;
    let text = result?;
    if !has_read_range_args(args) {
        return text_result(text);
    }
    let ranged = ranged_text_result(args, &text)?;
    text_result(serde_json::to_string(&ranged)?)
}

fn has_read_range_args(args: &Value) -> bool {
    ["start_line", "end_line", "offset_bytes", "max_bytes"]
        .iter()
        .any(|key| args.get(*key).is_some())
}

fn ranged_text_result(args: &Value, text: &str) -> Result<Value> {
    let start_line = optional_usize(args, "start_line")?;
    let end_line = optional_usize(args, "end_line")?;
    let offset_bytes = optional_usize(args, "offset_bytes")?;
    let max_bytes = optional_usize(args, "max_bytes")?.unwrap_or(MAX_TEXT_FILE_BYTES);
    anyhow::ensure!(
        (4..=MAX_TEXT_FILE_BYTES).contains(&max_bytes),
        "max_bytes must be 4..={MAX_TEXT_FILE_BYTES}"
    );
    anyhow::ensure!(
        !(offset_bytes.is_some() && (start_line.is_some() || end_line.is_some())),
        "offset_bytes cannot be combined with start_line or end_line"
    );
    if let Some(start_line) = start_line {
        anyhow::ensure!(start_line >= 1, "start_line must be at least 1");
    }
    if let Some(end_line) = end_line {
        anyhow::ensure!(end_line >= 1, "end_line must be at least 1");
        anyhow::ensure!(
            end_line >= start_line.unwrap_or(1),
            "end_line must not precede start_line"
        );
    }

    let start = if let Some(offset) = offset_bytes {
        anyhow::ensure!(offset <= text.len(), "offset_bytes exceeds file length");
        anyhow::ensure!(
            text.is_char_boundary(offset),
            "offset_bytes is not a UTF-8 boundary"
        );
        offset
    } else {
        line_start_offset(text, start_line.unwrap_or(1))
            .context("start_line exceeds file length")?
    };
    let range_end = if offset_bytes.is_some() {
        text.len()
    } else if let Some(end_line) = end_line {
        line_end_offset(text, end_line).context("end_line exceeds file length")?
    } else {
        text.len()
    };
    anyhow::ensure!(start <= range_end, "requested range is empty or reversed");

    let desired_end = start.saturating_add(max_bytes).min(range_end);
    let mut end = desired_end;
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    let content = text[start..end].to_owned();
    let truncated = end < range_end;
    Ok(json!({
        "content": content,
        "file_bytes": text.len(),
        "start_offset_bytes": start,
        "returned_bytes": end.saturating_sub(start),
        "range_end_offset_bytes": range_end,
        "truncated": truncated,
        "next_offset_bytes": truncated.then_some(end),
        "utf8_boundary": true
    }))
}

fn optional_usize(args: &Value, key: &str) -> Result<Option<usize>> {
    args.get(key)
        .map(|value| {
            let value = value
                .as_u64()
                .with_context(|| format!("{key} must be a non-negative integer"))?;
            usize::try_from(value).with_context(|| format!("{key} is too large"))
        })
        .transpose()
}

fn line_start_offset(text: &str, line: usize) -> Option<usize> {
    if line == 1 {
        return Some(0);
    }
    let mut current = 1usize;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            current += 1;
            if current == line {
                return Some(index + 1);
            }
        }
    }
    None
}

fn line_end_offset(text: &str, line: usize) -> Option<usize> {
    let start = line_start_offset(text, line)?;
    match text[start..].find('\n') {
        Some(relative) => Some(start + relative + 1),
        None => Some(text.len()),
    }
}

fn evidence_read_tool(args: &Value, session: &config::Session) -> Result<Value> {
    let evidence_id = args
        .get("evidence_id")
        .and_then(Value::as_str)
        .context("missing evidence_id")
        .and_then(|value| Uuid::parse_str(value).context("invalid evidence_id"))?;
    let offset_bytes = optional_usize(args, "offset_bytes")?.unwrap_or(0);
    let max_bytes = optional_usize(args, "max_bytes")?.unwrap_or(evidence::DEFAULT_READ_BYTES);
    let chunk = evidence::read(
        &session.id,
        &session.cwd,
        evidence_id,
        offset_bytes,
        max_bytes,
    )?;
    text_result(serde_json::to_string(&chunk)?)
}

async fn read_text_file(path: &Path) -> Result<String> {
    let file = open_readonly_nofollow(path, "file")?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect file {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "path is not a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_TEXT_FILE_BYTES as u64,
        "file exceeds {MAX_TEXT_FILE_BYTES} bytes: {}",
        path.display()
    );
    let file = tokio::fs::File::from_std(file);
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_TEXT_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("cannot read file {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_TEXT_FILE_BYTES,
        "file exceeds {MAX_TEXT_FILE_BYTES} bytes: {}",
        path.display()
    );
    String::from_utf8(bytes).context("file is not valid UTF-8")
}

fn open_readonly_nofollow(path: &Path, label: &str) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .with_context(|| format!("cannot open {label} {} safely", path.display()))
}

async fn ensure_regular_write_target(path: &Path) -> Result<()> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_file(),
            "write target is not a regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot inspect write target {}", path.display()));
        }
    }
    Ok(())
}

async fn get_image(path: &Path) -> Result<Value> {
    let file = open_readonly_nofollow(path, "image")?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect image {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "image path is not a file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_IMAGE_BYTES as u64,
        "image exceeds {MAX_IMAGE_BYTES} bytes: {}",
        path.display()
    );
    let file = tokio::fs::File::from_std(file);
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("cannot read image {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_IMAGE_BYTES,
        "image exceeds {MAX_IMAGE_BYTES} bytes: {}",
        path.display()
    );
    let mime_type = image_mime_type(&bytes)
        .with_context(|| format!("unsupported image format: {}", path.display()))?;
    Ok(json!({
        "content": [{
            "type": "image",
            "data": STANDARD.encode(bytes),
            "mimeType": mime_type
        }]
    }))
}

fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Some("image/tiff")
    } else if bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(&bytes[8..12], b"avif" | b"avis")
    {
        Some("image/avif")
    } else {
        None
    }
}

fn validate_path_argument(value: &str, name: &str) -> Result<()> {
    anyhow::ensure!(
        value.len() <= MAX_PATH_ARGUMENT_BYTES,
        "{name} exceeds {MAX_PATH_ARGUMENT_BYTES} bytes"
    );
    Ok(())
}

fn bounded_path(value: &str, name: &str) -> Result<PathBuf> {
    validate_path_argument(value, name)?;
    Ok(PathBuf::from(value))
}

fn required_path(args: &Value, name: &str) -> Result<PathBuf> {
    let value = args
        .get(name)
        .and_then(Value::as_str)
        .context(format!("missing {name}"))?;
    bounded_path(value, name)
}

fn required_session_id(args: &Value) -> Result<String> {
    let value = args
        .get("session_id")
        .and_then(Value::as_str)
        .context("missing session_id; ask the user to run `temote-mcp start` and provide its ID")?;
    config::validate_session_id(value)?;
    Ok(value.to_owned())
}

fn cwd(args: &Value, session: &config::Session) -> Result<PathBuf> {
    let path = args
        .get("cwd")
        .map(|value| {
            let value = value.as_str().context("cwd must be a string")?;
            bounded_path(value, "cwd")
        })
        .transpose()?;
    config::resolve_cwd(session, path.as_deref())
}

async fn list_directory(path: &Path) -> Result<String> {
    let mut entries = tokio::fs::read_dir(path).await?;
    let mut names = Vec::new();
    let mut rendered_bytes = 0;
    while let Some(entry) = entries.next_entry().await? {
        let suffix = if entry.file_type().await?.is_dir() {
            "/"
        } else {
            ""
        };
        let name = format!("{}{}", entry.file_name().to_string_lossy(), suffix);
        push_directory_listing_entry(
            &mut names,
            &mut rendered_bytes,
            name,
            MAX_DIRECTORY_ENTRIES,
            MAX_DIRECTORY_LIST_BYTES,
        )?;
    }
    names.sort();
    Ok(names.join("\n"))
}

fn push_directory_listing_entry(
    names: &mut Vec<String>,
    rendered_bytes: &mut usize,
    name: String,
    max_entries: usize,
    max_bytes: usize,
) -> Result<()> {
    anyhow::ensure!(
        names.len() < max_entries,
        "directory contains more than {max_entries} entries"
    );
    let separator = usize::from(!names.is_empty());
    let next_bytes = rendered_bytes
        .checked_add(separator)
        .and_then(|value| value.checked_add(name.len()))
        .context("directory listing size overflow")?;
    anyhow::ensure!(
        next_bytes <= max_bytes,
        "directory listing exceeds {max_bytes} bytes"
    );
    names.push(name);
    *rendered_bytes = next_bytes;
    Ok(())
}

async fn write_file(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let absolute = config::resolve_write_path(session, &required_path(args, "path")?)?;
    ensure_regular_write_target(&absolute).await?;
    let parent = absolute.parent().context("file has no parent directory")?;
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("parent does not exist: {}", parent.display()))?;
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .context("missing content")?;
    let previous = read_text_file(&absolute).await.unwrap_or_default();
    let command = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "cat > \"$1\"".to_owned(),
        "temote-mcp-write".to_owned(),
        absolute.display().to_string(),
    ];
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    let result = if session.yolo() {
        tokio::fs::write(&absolute, content)
            .await
            .with_context(|| format!("failed to write {}", absolute.display()))
            .map(|_| json!({"exit_code":0,"stdout":"","stderr":"","truncated":false}).to_string())
    } else {
        sandbox::run(
            &command,
            &parent,
            std::slice::from_ref(&parent),
            Some(content.as_bytes()),
        )
        .await
        .and_then(render_output)
    };
    let (added, removed, diff) = render_diff(&previous, content);
    let title = format!(
        "Edited {} (+{added} -{removed})",
        display_path(&absolute, &session.cwd)
    );
    let detail = match &result {
        Ok(_) => (!diff.is_empty()).then_some(diff),
        Err(error) => Some(format!("└ Error: {error:#}")),
    };
    approvals::activity(&session.id, title, detail).await;
    text_result(result?)
}

fn git_activity_operation(name: &str) -> Option<ActivityOperation> {
    match name {
        "git_add" => Some(ActivityOperation::GitAdd),
        "git_commit" => Some(ActivityOperation::GitCommit),
        "git_fetch" => Some(ActivityOperation::GitFetch),
        "git_pull" => Some(ActivityOperation::GitPull),
        "git_push" => Some(ActivityOperation::GitPush),
        "git_push_tag" => Some(ActivityOperation::GitPush),
        "git_branch_create" => Some(ActivityOperation::GitBranchCreate),
        "git_switch" => Some(ActivityOperation::GitSwitch),
        "git_worktree_add" => Some(ActivityOperation::GitWorktreeAdd),
        "git_worktree_create" => Some(ActivityOperation::GitWorktreeCreate),
        "git_worktree_list" => Some(ActivityOperation::GitWorktreeList),
        "git_worktree_remove" => Some(ActivityOperation::GitWorktreeRemove),
        "git_worktree_prune" => Some(ActivityOperation::GitWorktreePrune),
        _ => None,
    }
}

async fn git_add(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let paths = required_string_array(args, "paths")?;
    anyhow::ensure!(!paths.is_empty(), "paths must not be empty");
    anyhow::ensure!(
        paths.len() <= MAX_GIT_ADD_PATHS,
        "paths must contain at most {MAX_GIT_ADD_PATHS} entries"
    );

    let mut command = vec!["git".to_owned(), "add".to_owned(), "--".to_owned()];
    for path in paths {
        command.push(resolve_git_add_path(session, &path)?);
    }
    run_git_and_report(session, cwd, command, "Stage files", activity).await
}

async fn git_commit(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .context("missing message")?;
    validate_git_commit_message(message)?;
    ensure_staged_paths_are_permitted(session, &cwd).await?;

    let command = build_git_commit_command(message);
    run_git_and_report(session, cwd, command, "Create Git commit", activity).await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitRemoteOperation {
    Fetch,
    Pull,
    Push,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitRemoteDestinations {
    urls: Vec<String>,
}

impl GitRemoteDestinations {
    fn requires_github_credential_mapping(&self) -> bool {
        self.urls.iter().any(|url| is_github_https_destination(url))
    }
}

fn is_github_https_destination(url: &str) -> bool {
    if url.is_empty()
        || url.len() > MAX_GITHUB_REMOTE_URL_BYTES
        || url.chars().any(char::is_control)
    {
        return false;
    }
    let Some((scheme, authority_and_path)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.is_empty() || authority.chars().any(char::is_control) {
        return false;
    }
    let host_and_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host_and_port)| host_and_port);
    let host = if let Some(bracketed_host) = host_and_port.strip_prefix('[') {
        bracketed_host
            .split_once(']')
            .map_or(bracketed_host, |(host, _)| host)
    } else {
        host_and_port
            .split_once(':')
            .map_or(host_and_port, |(host, _)| host)
    };
    host.trim_end_matches('.')
        .eq_ignore_ascii_case("github.com")
}

async fn git_fetch(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let remote = optional_git_remote(args)?;
    let output = git_fetch_output(session, cwd, remote, activity).await?;
    text_result(render_output(output)?)
}

/// Runs the validated `fetch --prune` contract and returns the raw process
/// outcome. Shared by the structured `git_fetch` tool and the local-agent Git
/// broker so both surfaces use exactly one authority.
pub(crate) async fn git_fetch_output(
    session: &config::Session,
    cwd: PathBuf,
    remote: Option<String>,
    activity: Option<&ActivityScope>,
) -> Result<sandbox::Output> {
    let remote = remote.unwrap_or_else(|| "origin".to_owned());
    validate_git_remote(&remote)?;
    let destinations =
        resolve_git_remote_destinations(session, &cwd, &remote, GitRemoteOperation::Fetch).await?;
    ensure_github_https_destinations_credential_mapping(session, &cwd, &destinations).await?;
    let command = build_git_fetch_command(&remote);
    run_approved_git_network_output(
        session,
        cwd,
        command,
        "git_fetch",
        activity,
        destinations.requires_github_credential_mapping(),
    )
    .await
}

async fn git_pull(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let output = git_pull_output(session, cwd, activity).await?;
    text_result(render_output(output)?)
}

/// Runs the validated `pull --ff-only` contract and returns the raw process
/// outcome. Shared by the structured `git_pull` tool and the local-agent Git
/// broker.
pub(crate) async fn git_pull_output(
    session: &config::Session,
    cwd: PathBuf,
    activity: Option<&ActivityScope>,
) -> Result<sandbox::Output> {
    let remote = git_current_upstream_remote(session, &cwd).await?;
    let remote = remote.context(GIT_PULL_UPSTREAM_CONFIGURATION_ERROR)?;
    let destinations =
        resolve_git_remote_destinations(session, &cwd, &remote, GitRemoteOperation::Pull).await?;
    ensure_github_https_destinations_credential_mapping(session, &cwd, &destinations).await?;
    let command = build_git_pull_command();
    run_approved_git_network_output(
        session,
        cwd,
        command,
        "git_pull",
        activity,
        destinations.requires_github_credential_mapping(),
    )
    .await
}

/// Resolves the configured upstream remote of the current branch.
///
/// This reads the current branch's fixed remote/merge config directly instead
/// of requiring an already-created remote-tracking ref. The pull command never
/// receives a caller-supplied remote.
async fn git_current_upstream_remote(
    session: &config::Session,
    cwd: &Path,
) -> Result<Option<String>> {
    let branch = git_current_branch_name(session, cwd)
        .await?
        .context(GIT_PULL_UPSTREAM_CONFIGURATION_ERROR)?;
    let remote_key = format!("branch.{branch}.remote");
    let remote_values = git_config_values(session, cwd, &remote_key).await?;
    anyhow::ensure!(
        remote_values.len() == 1,
        GIT_PULL_UPSTREAM_CONFIGURATION_ERROR
    );
    let remote = remote_values
        .into_iter()
        .next()
        .context(GIT_PULL_UPSTREAM_CONFIGURATION_ERROR)?;
    if remote != "." {
        validate_git_remote(&remote)
            .map_err(|_| anyhow::anyhow!(GIT_PULL_UPSTREAM_CONFIGURATION_ERROR))?;
    }
    let merge_key = format!("branch.{branch}.merge");
    let merge_values = git_config_values(session, cwd, &merge_key).await?;
    anyhow::ensure!(
        merge_values.len() == 1,
        GIT_PULL_UPSTREAM_CONFIGURATION_ERROR
    );
    let merge = merge_values
        .into_iter()
        .next()
        .context(GIT_PULL_UPSTREAM_CONFIGURATION_ERROR)?;
    validate_git_merge_ref(&merge)?;
    Ok(Some(remote))
}

async fn resolve_git_remote_destinations(
    session: &config::Session,
    cwd: &Path,
    remote: &str,
    operation: GitRemoteOperation,
) -> Result<GitRemoteDestinations> {
    validate_git_remote(remote)?;
    if operation == GitRemoteOperation::Pull && remote == "." {
        return Ok(GitRemoteDestinations { urls: Vec::new() });
    }

    let command = match operation {
        GitRemoteOperation::Fetch | GitRemoteOperation::Pull => vec![
            "git".to_owned(),
            "remote".to_owned(),
            "get-url".to_owned(),
            remote.to_owned(),
        ],
        GitRemoteOperation::Push => vec![
            "git".to_owned(),
            "remote".to_owned(),
            "get-url".to_owned(),
            "--push".to_owned(),
            "--all".to_owned(),
            remote.to_owned(),
        ],
    };
    let output =
        map_git_remote_inspection_error(run_host_git_inspection(session, cwd, &command).await)?;
    anyhow::ensure!(
        output.status == 0,
        "Git remote {remote:?} is not configured"
    );
    let urls = parse_git_remote_destinations(&output, operation)?;
    Ok(GitRemoteDestinations { urls })
}

fn parse_git_remote_destinations(
    output: &sandbox::Output,
    operation: GitRemoteOperation,
) -> Result<Vec<String>> {
    anyhow::ensure!(output.status == 0, GIT_REMOTE_DESTINATION_ERROR);
    anyhow::ensure!(!output.truncated, GIT_REMOTE_DESTINATION_ERROR);
    anyhow::ensure!(
        !output.stdout.is_empty() && output.stdout.len() <= MAX_GIT_REMOTE_DESTINATIONS_BYTES,
        GIT_REMOTE_DESTINATION_ERROR
    );

    let mut lines = output.stdout.split('\n').collect::<Vec<_>>();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    anyhow::ensure!(
        !lines.is_empty() && lines.len() <= MAX_GIT_REMOTE_DESTINATIONS,
        GIT_REMOTE_DESTINATION_ERROR
    );
    if matches!(
        operation,
        GitRemoteOperation::Fetch | GitRemoteOperation::Pull
    ) {
        anyhow::ensure!(lines.len() == 1, GIT_REMOTE_DESTINATION_ERROR);
    }

    let mut total_bytes = 0usize;
    let mut urls = Vec::with_capacity(lines.len());
    for line in lines {
        anyhow::ensure!(
            !line.is_empty()
                && line.len() <= MAX_GITHUB_REMOTE_URL_BYTES
                && line == line.trim()
                && !line.chars().any(char::is_control),
            GIT_REMOTE_DESTINATION_ERROR
        );
        total_bytes = total_bytes
            .checked_add(line.len())
            .context(GIT_REMOTE_DESTINATION_ERROR)?;
        anyhow::ensure!(
            total_bytes <= MAX_GIT_REMOTE_DESTINATIONS_BYTES,
            GIT_REMOTE_DESTINATION_ERROR
        );
        urls.push(line.to_owned());
    }
    Ok(urls)
}

fn map_git_remote_inspection_error<T>(result: Result<T>) -> Result<T> {
    result.map_err(|_| anyhow::anyhow!(GIT_REMOTE_DESTINATION_ERROR))
}

fn validate_git_merge_ref(merge: &str) -> Result<()> {
    anyhow::ensure!(
        merge.len() > "refs/heads/".len()
            && merge.len() <= MAX_GIT_BASE_REF_BYTES
            && merge.starts_with("refs/heads/")
            && merge == merge.trim()
            && !merge.chars().any(char::is_control),
        GIT_PULL_UPSTREAM_CONFIGURATION_ERROR
    );
    let branch = &merge["refs/heads/".len()..];
    anyhow::ensure!(
        branch.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '/' | '.' | '_' | '-')
        }) && !branch.starts_with('/')
            && !branch.ends_with('/')
            && !branch.contains("..")
            && !branch.contains("//")
            && !branch.contains("@{")
            && !branch.ends_with(".lock"),
        GIT_PULL_UPSTREAM_CONFIGURATION_ERROR
    );
    Ok(())
}

/// Resolves the effective push remote of the current branch, if any.
///
/// Mirrors Git's own destination selection order (`@{push}`, `pushRemote`,
/// `remote.pushDefault`, `branch.<name>.remote`, then `origin`) so the
/// repository-local credential gate is applied to the same remote Git would
/// contact. A local destination (`"."`) needs no credential. When nothing
/// resolves, the existing structured `git push` behavior is unchanged and Git
/// itself decides.
async fn git_current_push_remote(session: &config::Session, cwd: &Path) -> Result<Option<String>> {
    if let Some(remote) = git_remote_for_symbolic_rev(session, cwd, "@{push}").await? {
        return Ok(Some(remote));
    }
    if let Some(branch) = git_current_branch_name(session, cwd).await? {
        for key in [
            format!("branch.{branch}.pushRemote"),
            "remote.pushDefault".to_owned(),
            format!("branch.{branch}.remote"),
        ] {
            let Some(remote) = git_config_value(session, cwd, &key).await? else {
                continue;
            };
            if remote == "." {
                // A local destination never contacts a remote.
                return Ok(None);
            }
            validate_git_remote(&remote)?;
            return Ok(Some(remote));
        }
    }
    let origin = run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "remote".to_owned(),
            "get-url".to_owned(),
            "origin".to_owned(),
        ],
    )
    .await?;
    if origin.status == 0 && !origin.stdout.trim().is_empty() {
        return Ok(Some("origin".to_owned()));
    }
    Ok(None)
}

async fn git_current_branch_name(session: &config::Session, cwd: &Path) -> Result<Option<String>> {
    let output = run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "symbolic-ref".to_owned(),
            "--quiet".to_owned(),
            "--short".to_owned(),
            "HEAD".to_owned(),
        ],
    )
    .await?;
    if output.status != 0 {
        return Ok(None);
    }
    let branch = output.stdout.trim();
    if branch.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        branch.len() <= MAX_GIT_BRANCH_NAME_BYTES && !branch.chars().any(char::is_control),
        "current Git branch name is invalid"
    );
    Ok(Some(branch.to_owned()))
}

async fn git_config_value(
    session: &config::Session,
    cwd: &Path,
    key: &str,
) -> Result<Option<String>> {
    let values = git_config_values(session, cwd, key).await?;
    match values.as_slice() {
        [] => Ok(None),
        [value] => Ok(Some(value.to_owned())),
        _ => anyhow::bail!(GIT_PULL_UPSTREAM_CONFIGURATION_ERROR),
    }
}

/// Resolves `<remote>/<ref>` for one symbolic revision to a validated remote
/// name without exposing the revision to the caller.
async fn git_remote_for_symbolic_rev(
    session: &config::Session,
    cwd: &Path,
    rev: &str,
) -> Result<Option<String>> {
    let output = run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "rev-parse".to_owned(),
            "--abbrev-ref".to_owned(),
            rev.to_owned(),
        ],
    )
    .await?;
    if output.status != 0 {
        return Ok(None);
    }
    let resolved = output.stdout.trim();
    let (remote, _) = match resolved.split_once('/') {
        Some(parts) => parts,
        None => return Ok(None),
    };
    if remote.is_empty() || remote == "." {
        // A local destination never contacts a network remote.
        return Ok(None);
    }
    validate_git_remote(remote)?;
    Ok(Some(remote.to_owned()))
}

async fn git_push(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let remote = optional_git_remote(args)?;
    let set_upstream = args
        .get("set_upstream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let output = git_push_output(session, cwd, remote, set_upstream, activity).await?;
    text_result(render_output(output)?)
}

/// Runs the validated current-branch push contract and returns the raw process
/// outcome. Shared by the structured `git_push` tool and the local-agent Git
/// broker. Force, refspecs and arbitrary URLs stay unavailable.
pub(crate) async fn git_push_output(
    session: &config::Session,
    cwd: PathBuf,
    remote: Option<String>,
    set_upstream: bool,
    activity: Option<&ActivityScope>,
) -> Result<sandbox::Output> {
    let selected_remote = if set_upstream {
        Some(remote.clone().unwrap_or_else(|| "origin".to_owned()))
    } else if remote.is_some() {
        remote.clone()
    } else {
        // No explicit remote: resolve the effective push remote only to decide
        // whether the repository-local managed credential mapping is required.
        // The command itself stays `git push` and never receives this value.
        git_current_push_remote(session, &cwd).await?
    };
    let destinations = if let Some(remote) = &selected_remote {
        validate_git_remote(remote)?;
        let destinations =
            resolve_git_remote_destinations(session, &cwd, remote, GitRemoteOperation::Push)
                .await?;
        ensure_github_https_destinations_credential_mapping(session, &cwd, &destinations).await?;
        Some(destinations)
    } else {
        None
    };
    let command = build_git_push_command(remote, set_upstream);
    run_approved_git_network_output(
        session,
        cwd,
        command,
        "git_push",
        activity,
        destinations
            .as_ref()
            .is_some_and(GitRemoteDestinations::requires_github_credential_mapping),
    )
    .await
}

/// The exact non-force fetch shape: fixed hooks/submodule hardening plus the
/// validated configured remote name.
pub(crate) fn build_git_fetch_command(remote: &str) -> Vec<String> {
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "-c".to_owned(),
        "fetch.recurseSubmodules=false".to_owned(),
        "fetch".to_owned(),
        "--prune".to_owned(),
        remote.to_owned(),
    ]
}

/// The exact fast-forward-only pull shape.
pub(crate) fn build_git_pull_command() -> Vec<String> {
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "-c".to_owned(),
        "fetch.recurseSubmodules=false".to_owned(),
        "pull".to_owned(),
        "--ff-only".to_owned(),
        "--recurse-submodules=no".to_owned(),
    ]
}

/// The exact current-branch push shape. `HEAD` is the only refspec and no force
/// option exists.
pub(crate) fn build_git_push_command(remote: Option<String>, set_upstream: bool) -> Vec<String> {
    let mut command = vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "-c".to_owned(),
        "push.recurseSubmodules=off".to_owned(),
        "push".to_owned(),
    ];
    if set_upstream {
        command.push("--set-upstream".to_owned());
        command.push(remote.unwrap_or_else(|| "origin".to_owned()));
        command.push("HEAD".to_owned());
    } else if let Some(remote) = remote {
        command.push(remote);
        command.push("HEAD".to_owned());
    }
    command
}

/// Requires the repository-local managed GitHub credential mapping before any
/// network Git command may use a GitHub HTTPS remote.
///
/// Non-GitHub and non-HTTPS remotes carry no GitHub credential and are left to
/// the existing configured-remote validation. The mapping is always read from
/// the selected repository's own local config, so concurrent repositories can
/// never borrow each other's identity.
#[allow(dead_code)]
pub(crate) async fn ensure_github_https_remote_credential_mapping(
    session: &config::Session,
    cwd: &Path,
    remote: &str,
) -> Result<()> {
    let destinations =
        resolve_git_remote_destinations(session, cwd, remote, GitRemoteOperation::Fetch).await?;
    ensure_github_https_destinations_credential_mapping(session, cwd, &destinations).await
}

async fn ensure_github_https_destinations_credential_mapping(
    session: &config::Session,
    cwd: &Path,
    destinations: &GitRemoteDestinations,
) -> Result<()> {
    if !destinations.requires_github_credential_mapping() {
        return Ok(());
    }
    let local_helpers = map_github_credential_inspection_error(
        run_host_git_inspection(
            session,
            cwd,
            &[
                "git".to_owned(),
                "config".to_owned(),
                "--local".to_owned(),
                "--includes".to_owned(),
                "--get-all".to_owned(),
                "credential.helper".to_owned(),
            ],
        )
        .await,
    )?;
    let local_use_http_path = map_github_credential_inspection_error(
        run_host_git_inspection(
            session,
            cwd,
            &[
                "git".to_owned(),
                "config".to_owned(),
                "--local".to_owned(),
                "--includes".to_owned(),
                "--get".to_owned(),
                "credential.useHttpPath".to_owned(),
            ],
        )
        .await,
    )?;
    validate_github_credential_mapping_inspection(&local_helpers, &local_use_http_path)?;
    Ok(())
}

fn map_github_credential_inspection_error<T>(result: Result<T>) -> Result<T> {
    result.map_err(|_| anyhow::anyhow!(GITHUB_CREDENTIAL_MAPPING_ERROR))
}

fn validate_github_credential_mapping_inspection(
    local_helpers: &sandbox::Output,
    local_use_http_path: &sandbox::Output,
) -> Result<()> {
    anyhow::ensure!(
        local_helpers.status == 0
            && !local_helpers.truncated
            && local_use_http_path.status == 0
            && !local_use_http_path.truncated
            && repo_scoped_github_credential_mapping_valid(
                &local_helpers.stdout,
                &local_use_http_path.stdout,
            ),
        GITHUB_CREDENTIAL_MAPPING_ERROR
    );
    Ok(())
}

async fn git_push_tag(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let remote = optional_git_remote(args)?.unwrap_or_else(|| "origin".to_owned());
    ensure_configured_git_remote(session, &cwd, &remote).await?;

    let tag = args
        .get("tag")
        .and_then(Value::as_str)
        .context("missing or non-string tag")?;
    validate_git_tag_name(tag)?;
    ensure_git_tag_ref_valid(session, &cwd, tag).await?;

    let source_sha = args
        .get("source_sha")
        .and_then(Value::as_str)
        .context("missing or non-string source_sha")?;
    validate_git_object_id(source_sha, "source_sha")?;
    let source_sha = resolve_exact_git_commit(session, &cwd, source_sha).await?;

    let expected_remote_sha = args
        .get("expected_remote_sha")
        .map(|value| {
            let value = value
                .as_str()
                .context("expected_remote_sha must be a string")?;
            validate_git_object_id(value, "expected_remote_sha")?;
            Ok::<String, anyhow::Error>(value.to_ascii_lowercase())
        })
        .transpose()?;

    let command =
        build_git_push_tag_command(&remote, tag, &source_sha, expected_remote_sha.as_deref());
    run_approved_git_command(session, cwd, command, "git_push_tag", activity).await
}

async fn git_branch_create(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let branch = args
        .get("branch")
        .and_then(Value::as_str)
        .context("missing or non-string branch")?;
    validate_git_branch_name(session, &cwd, branch).await?;
    ensure_local_branch_absent(session, &cwd, branch).await?;
    let base = args.get("base").map(|value| {
        value
            .as_str()
            .context("base must be a string")
            .map(str::to_owned)
    });
    let base = match base.transpose()? {
        Some(base) => resolve_git_base_commit(session, &cwd, &base).await?,
        None => resolve_git_base_commit(session, &cwd, "HEAD").await?,
    };
    approve_local_git_mutation(
        session,
        &cwd,
        "git_branch_create",
        format!("branch={branch} base={base}"),
        activity,
    )
    .await?;
    let command = build_git_branch_create_command(branch, &base);
    run_git_and_report(session, cwd, command, "Create Git branch", activity).await
}

async fn git_switch(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let branch = args
        .get("branch")
        .and_then(Value::as_str)
        .context("missing or non-string branch")?;
    validate_git_branch_name(session, &cwd, branch).await?;
    ensure_local_branch_exists(session, &cwd, branch).await?;
    approve_local_git_mutation(
        session,
        &cwd,
        "git_switch",
        format!("branch={branch}"),
        activity,
    )
    .await?;
    let command = build_git_switch_command(branch);
    run_git_and_report(session, cwd, command, "Switch Git branch", activity).await
}

async fn git_worktree_add(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let repository_root = sandbox::git_worktree_root(&cwd)?;
    config::ensure_permitted(session, &repository_root)
        .context("Git repository root must be inside a permitted session root")?;
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .context("missing or non-string name")?;
    validate_git_worktree_name(name)?;
    let branch = args
        .get("branch")
        .and_then(Value::as_str)
        .context("missing or non-string branch")?;
    validate_git_branch_name(session, &cwd, branch).await?;
    let destination = git_worktree_destination(&repository_root, name)?;
    ensure_git_worktree_destination_available(&repository_root, &destination)?;

    let base = args.get("base").map(|value| {
        value
            .as_str()
            .context("base must be a string")
            .map(str::to_owned)
    });
    let (command, action) = match base.transpose()? {
        Some(base) => {
            ensure_local_branch_absent(session, &cwd, branch).await?;
            let base = resolve_git_base_commit(session, &cwd, &base).await?;
            (
                build_git_worktree_add_create_command(&destination, branch, &base),
                format!("name={name} branch={branch} base={base}"),
            )
        }
        None => {
            ensure_local_branch_exists(session, &cwd, branch).await?;
            (
                build_git_worktree_add_existing_command(&destination, branch),
                format!("name={name} branch={branch}"),
            )
        }
    };
    approve_local_git_mutation(session, &cwd, "git_worktree_add", action, activity).await?;
    ensure_git_worktree_root_exists(&repository_root)?;
    run_git_worktree_add_and_report(session, cwd, command, "Create Git worktree", activity).await
}

/// Resolves the configured `src` named root from `TEMOTE_MCP_ROOTS`. The
/// physical src root always comes from the named-root authority, never from
/// `HOME` and never from a caller-supplied path.
fn configured_src_root() -> Result<PathBuf> {
    managed_worktree::configured_src_root_from_env().with_context(|| {
        format!(
            "managed worktrees require the configured {} named root in TEMOTE_MCP_ROOTS; \
             the managed root is always <{}-root>/worktrees/<repo>",
            managed_worktree::MANAGED_SRC_ROOT_NAME,
            managed_worktree::MANAGED_SRC_ROOT_NAME
        )
    })
}

fn optional_configured_src_root() -> Option<PathBuf> {
    managed_worktree::configured_src_root_from_env()
}

/// Managed-worktree tools accept only the session-selected repository. A
/// caller-supplied `cwd` or `base` is rejected instead of silently ignored so
/// no client can select a nested repository or a creation base.
fn reject_removed_managed_worktree_arguments(args: &Value, removed: &[&str]) -> Result<()> {
    for name in removed {
        anyhow::ensure!(
            args.get(*name).is_none(),
            "git_worktree tools do not accept {name}; the repository and target are derived from the selected session"
        );
    }
    Ok(())
}

/// Resolves the canonical repository identity and its Temote-managed worktree
/// namespace for one session request. Callers never supply a filesystem path;
/// the optional repository input is only cross-checked against the identity.
pub(crate) fn managed_repository_for_requested(
    requested: Option<&str>,
    session: &config::Session,
    cwd: &Path,
    src_root: &Path,
) -> Result<managed_worktree::ManagedRepository> {
    let primary_checkout = sandbox::git_primary_checkout(cwd)?;
    config::ensure_permitted(session, &primary_checkout)
        .context("Git repository root must be inside a permitted session root")?;
    let repository = managed_worktree::ManagedRepository::resolve(&primary_checkout, src_root)?;
    if let Some(requested) = requested {
        repository.ensure_requested_repository(requested)?;
    }
    Ok(repository)
}

async fn git_worktree_create(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let src_root = configured_src_root()?;
    git_worktree_create_in_src_root(args, session, &src_root, activity).await
}

/// One verified managed worktree bound to a session request.
///
/// The repository identity, target, task and branch are pinned from Temote's
/// own canonical resolution; no caller-supplied path participates.
#[derive(Clone, Debug)]
pub(crate) struct ManagedWorktreeBinding {
    repository: managed_worktree::ManagedRepository,
    target: PathBuf,
    branch: String,
}

impl ManagedWorktreeBinding {
    fn repository_name(&self) -> &str {
        self.repository.repository_name()
    }

    fn repository_root(&self) -> &Path {
        self.repository.primary_checkout()
    }

    fn workspace_root(&self) -> &Path {
        &self.target
    }

    /// Derives the sandbox session for this run.
    ///
    /// The selected session already permits the repository's canonical
    /// checkout (`managed_repository_for_requested`). The only addition is the
    /// validated managed worktree this run is bound to, so ordinary session
    /// tools keep their exact on-disk scope and no caller-supplied path can
    /// widen it.
    fn run_session(&self, session: &config::Session) -> config::Session {
        let mut run = session.clone();
        if !run
            .permitted_directories
            .iter()
            .any(|root| self.target == *root || self.target.starts_with(root))
        {
            run.permitted_directories.push(self.target.clone());
            run.permitted_directories.sort();
            run.permitted_directories.dedup();
        }
        run
    }

    /// Re-derives and re-verifies this binding from the configured `src` root
    /// and returns the canonical repository identity that was validated.
    ///
    /// Used immediately before a local agent launch so neither the pre-run
    /// resolution nor model prompt compliance is the enforcement mechanism.
    /// The returned identity is carried to the Git broker and the sandbox
    /// launch, which fail closed unless they re-observe exactly this identity.
    fn revalidate(&self, src_root: &Path) -> Result<sandbox::WorkspaceRepositoryIdentity> {
        let repository =
            managed_worktree::ManagedRepository::resolve(self.repository_root(), src_root)?;
        anyhow::ensure!(
            repository.repository_name() == self.repository_name(),
            "managed worktree repository identity changed while approval was pending"
        );
        let selected_common_dir = sandbox::git_common_dir(self.repository_root())?;
        managed_worktree::verify_reusable_managed_worktree(
            &repository,
            &self.target,
            &self.branch,
            &selected_common_dir,
            self.repository_root(),
        )?;
        let expected = sandbox::WorkspaceRepositoryIdentity::for_workspace(&self.target)
            .context("managed worktree target is no longer a supported Git worktree root")?;
        anyhow::ensure!(
            expected.worktree_root == self.target,
            "managed worktree target is no longer the validated direct child: {}",
            self.target.display()
        );
        anyhow::ensure!(
            expected.common_dir == selected_common_dir
                && expected.primary_checkout == self.repository_root(),
            "managed worktree repository identity changed while approval was pending"
        );
        Ok(expected)
    }
}

async fn git_worktree_create_in_src_root(
    args: &Value,
    session: &config::Session,
    src_root: &Path,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    reject_removed_managed_worktree_arguments(args, &["cwd", "base"])?;
    let branch = args
        .get("branch")
        .and_then(Value::as_str)
        .context("missing or non-string branch")?;
    let task = match args.get("task") {
        Some(value) => Some(value.as_str().context("task must be a string")?),
        None => None,
    };
    let requested_repository = match args.get("repository") {
        Some(value) => Some(value.as_str().context("repository must be a string")?),
        None => None,
    };
    let (_, value) = create_managed_worktree(
        session,
        src_root,
        branch,
        task,
        requested_repository,
        activity,
    )
    .await?;
    Ok(value)
}

/// Creates one managed worktree through the approved host-side path and returns
/// the verified binding plus the tool result document.
///
/// Every pre-approval step is read-only, the caller can never supply a path,
/// and the created target is re-verified against the trusted managed root and
/// repository identity before it is reported as created.
pub(crate) async fn create_managed_worktree(
    session: &config::Session,
    src_root: &Path,
    branch: &str,
    task: Option<&str>,
    requested_repository: Option<&str>,
    activity: Option<&ActivityScope>,
) -> Result<(ManagedWorktreeBinding, Value)> {
    let cwd = config::resolve_cwd(session, None)?;
    let repository =
        managed_repository_for_requested(requested_repository, session, &cwd, src_root)?;
    validate_git_branch_name(session, &cwd, branch).await?;
    ensure_local_branch_exists(session, &cwd, branch).await?;
    let task = match task {
        Some(task) => task.to_owned(),
        None => managed_worktree::derive_task_name(branch)?,
    };
    let target = repository.target(&task)?;
    // Everything above and the inspection below are read-only. Nothing is
    // created before the approval, and the pre-approval inspection result is
    // re-verified after it.
    repository.inspect_target_available(&target)?;
    let selected_common_dir = sandbox::git_common_dir(&cwd)?;
    let selected_primary_checkout = repository.primary_checkout().to_path_buf();
    let command = build_git_worktree_add_existing_command(&target, branch);
    let action = format!(
        "repository={} task={task} branch={branch}",
        repository.repository_name()
    );
    approve_local_git_mutation(
        session,
        repository.primary_checkout(),
        "git_worktree_create",
        action,
        activity,
    )
    .await?;
    repository.prepare_target(&target)?;
    run_managed_git_worktree_create_and_report(
        session,
        repository,
        selected_common_dir,
        selected_primary_checkout,
        target,
        task,
        branch.to_owned(),
        command,
        activity,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
fn managed_worktree_create_result(
    status: &str,
    repository: &managed_worktree::ManagedRepository,
    target: &Path,
    task: &str,
    branch: &str,
    output: &sandbox::Output,
    identity_verified: bool,
    mutation_committed: bool,
    verification_error: Option<&str>,
) -> String {
    let mut value = json!({
        "status": status,
        "repository": repository.repository_name(),
        "path": target.to_string_lossy(),
        "task": task,
        "branch": branch,
        "identity_verified": identity_verified,
        "mutation_committed": mutation_committed,
        "exit_code": output.status,
        "stdout": output.stdout,
        "stderr": output.stderr,
        "truncated": output.truncated,
    });
    if let Some(error) = verification_error {
        value["verification_error"] = json!(error);
    }
    value.to_string()
}

fn observe_created_managed_target(
    repository: &managed_worktree::ManagedRepository,
    target: &Path,
) -> managed_worktree::CreatedTargetObservation {
    let managed_root_metadata = std::fs::symlink_metadata(repository.managed_root());
    managed_worktree::CreatedTargetObservation {
        canonical_target: std::fs::canonicalize(target).ok(),
        canonical_managed_root: std::fs::canonicalize(repository.managed_root()).ok(),
        managed_root_is_normal_directory: managed_root_metadata
            .as_ref()
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink()),
        target_is_symlink: std::fs::symlink_metadata(target)
            .is_ok_and(|metadata| metadata.file_type().is_symlink()),
        observed_common_dir: sandbox::git_common_dir(target).ok(),
        observed_primary_checkout: sandbox::git_primary_checkout(target).ok(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_managed_git_worktree_create_and_report(
    session: &config::Session,
    repository: managed_worktree::ManagedRepository,
    selected_common_dir: PathBuf,
    selected_primary_checkout: PathBuf,
    target: PathBuf,
    task: String,
    branch: String,
    command: Vec<String>,
    activity: Option<&ActivityScope>,
) -> Result<(ManagedWorktreeBinding, Value)> {
    let rendered_command = render_command(&command);
    approvals::activity(
        &session.id,
        "Create managed Git worktree",
        Some(rendered_command.clone()),
    )
    .await;
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    let output = sandbox::run_unrestricted_with_env(
        &command,
        repository.primary_checkout(),
        None,
        &HashMap::new(),
        child_env::SENSITIVE_ENV_NAMES,
    )
    .await;
    let result = match output {
        Ok(output) => {
            let mutation_committed = output.status == 0;
            let verification = if mutation_committed {
                let observation = observe_created_managed_target(&repository, &target);
                managed_worktree::verify_created_managed_target(
                    &repository,
                    &task,
                    &selected_common_dir,
                    &selected_primary_checkout,
                    &observation,
                )
                .map_err(|error| format!("{error:#}"))
            } else {
                Err("Git worktree add did not complete successfully".to_owned())
            };
            match verification {
                Ok(()) => Ok(managed_worktree_create_result(
                    "created",
                    &repository,
                    &target,
                    &task,
                    &branch,
                    &output,
                    true,
                    true,
                    None,
                )),
                Err(error) => Err(anyhow::anyhow!(managed_worktree_create_result(
                    if mutation_committed {
                        "verification_failed"
                    } else {
                        "failed"
                    },
                    &repository,
                    &target,
                    &task,
                    &branch,
                    &output,
                    false,
                    mutation_committed,
                    Some(&error),
                ))),
            }
        }
        Err(error) => Err(error),
    };
    report_command_finished(session.id.clone(), "git", &rendered_command, &result).await;
    let value = text_result(result?)?;
    let binding = ManagedWorktreeBinding {
        repository,
        target,
        branch,
    };
    Ok((binding, value))
}

async fn git_worktree_list(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    git_worktree_list_with_src_root(
        args,
        session,
        optional_configured_src_root().as_deref(),
        activity,
    )
    .await
}

pub(crate) async fn git_worktree_list_with_src_root(
    args: &Value,
    session: &config::Session,
    src_root: Option<&Path>,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    reject_removed_managed_worktree_arguments(args, &["cwd", "base"])?;
    let cwd = config::resolve_cwd(session, None)?;
    let primary_checkout = sandbox::git_primary_checkout(&cwd)?;
    config::ensure_permitted(session, &primary_checkout)
        .context("Git repository root must be inside a permitted session root")?;
    let repository_name = primary_checkout
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .context("canonical repository checkout has no usable directory name")?;
    if let Some(requested) = args.get("repository") {
        managed_worktree::ensure_requested_repository(
            requested.as_str().context("repository must be a string")?,
            &repository_name,
        )?;
    }
    // Managed classification requires the exact configured src-root authority.
    // A missing named root or a non-exact repository layout only disables
    // managed classification; it never errors the read-only listing.
    let repository = src_root.and_then(|src_root| {
        managed_worktree::ManagedRepository::resolve(&primary_checkout, src_root).ok()
    });
    let canonical_managed_root = repository
        .as_ref()
        .and_then(managed_worktree::trusted_canonical_managed_root);
    let identity = sandbox::git_common_dir(&cwd)?;
    let command = vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "worktree".to_owned(),
        "list".to_owned(),
        "--porcelain".to_owned(),
    ];
    let output = run_host_git_inspection(session, &primary_checkout, &command).await?;
    anyhow::ensure!(
        output.status == 0,
        "git worktree list failed: {}",
        output.stderr.trim()
    );
    anyhow::ensure!(
        !output.truncated,
        "git worktree list output was truncated before classification"
    );
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    let registered = managed_worktree::parse_worktree_list(&output.stdout)?;
    let truncated = registered.len() > MAX_MANAGED_WORKTREE_LIST_ENTRIES;
    let entries = registered
        .iter()
        .take(MAX_MANAGED_WORKTREE_LIST_ENTRIES)
        .map(|entry| {
            let canonical_path = std::fs::canonicalize(&entry.path).ok();
            let registered_common_dir = canonical_path
                .as_deref()
                .and_then(|path| sandbox::git_common_dir(path).ok());
            let registered_primary_checkout = canonical_path
                .as_deref()
                .and_then(|path| sandbox::git_primary_checkout(path).ok());
            let classification = managed_worktree::classify_registered_worktree(
                managed_worktree::RegisteredWorktreeIdentity {
                    canonical_path: canonical_path.as_deref(),
                    common_dir: registered_common_dir.as_deref(),
                    primary_checkout: registered_primary_checkout.as_deref(),
                },
                &primary_checkout,
                canonical_managed_root.as_deref(),
                &identity,
            );
            json!({
                "path": entry.path.to_string_lossy(),
                "head": entry.head,
                "branch": entry.branch,
                "classification": classification.as_str(),
                "bare": entry.bare,
                "detached": entry.detached,
                "prunable": entry.prunable,
            })
        })
        .collect::<Vec<_>>();
    text_result(
        json!({
            "repository": repository_name,
            "primary_checkout": primary_checkout.to_string_lossy(),
            "managed_root": canonical_managed_root
                .as_ref()
                .map(|root| json!(root.to_string_lossy()))
                .unwrap_or(Value::Null),
            "worktrees": entries,
            "truncated": truncated,
        })
        .to_string(),
    )
}

/// Resolves one removal/prune target from broker policy only.
///
/// `task` is a validated single path component; `path` is accepted only as a
/// redundant selector that must equal the exact broker-derived direct child of
/// the trusted managed root. Neither input grants filesystem authority: the
/// returned path is always `managed_root/task`. At least one selector is
/// required, and when both are present they must select the same worktree.
fn resolve_managed_worktree_target(
    repository: &managed_worktree::ManagedRepository,
    task: Option<&str>,
    path: Option<&str>,
) -> Result<(String, PathBuf)> {
    let from_task = match task {
        Some(task) => Some((task.to_owned(), repository.target(task)?)),
        None => None,
    };
    let from_path = match path {
        Some(path) => {
            let candidate = Path::new(path);
            anyhow::ensure!(
                candidate.is_absolute(),
                "worktree path must be absolute; Temote derives managed worktree paths"
            );
            let name = candidate
                .file_name()
                .and_then(|name| name.to_str())
                .context("worktree path has no usable final component")?;
            managed_worktree::validate_task_name(name)?;
            let derived = repository.target(name)?;
            anyhow::ensure!(
                candidate == derived,
                "requested worktree path is not the Temote-derived managed path {}",
                derived.display()
            );
            Some((name.to_owned(), derived))
        }
        None => None,
    };
    match (from_task, from_path) {
        (Some((task, target)), Some((path_task, _))) => {
            anyhow::ensure!(
                task == path_task,
                "task and path select different managed worktrees"
            );
            Ok((task, target))
        }
        (Some(pair), None) | (None, Some(pair)) => Ok(pair),
        (None, None) => anyhow::bail!("a managed worktree task or path is required"),
    }
}

/// Reads the private Git metadata directory of one linked worktree from its
/// own `.git` pointer and requires it to live below the selected repository's
/// common Git directory.
fn linked_worktree_private_git_dir(
    target: &Path,
    common_dir: &Path,
    expected_worktree_root: &Path,
) -> Result<PathBuf> {
    let pointer = target.join(".git");
    let metadata = std::fs::symlink_metadata(&pointer).with_context(|| {
        format!(
            "cannot inspect linked worktree pointer {}",
            pointer.display()
        )
    })?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "linked worktree pointer must be a regular file: {}",
        pointer.display()
    );
    anyhow::ensure!(
        metadata.len() <= 4096,
        "linked worktree pointer is too large: {}",
        pointer.display()
    );
    let contents = std::fs::read_to_string(&pointer)
        .with_context(|| format!("cannot read linked worktree pointer {}", pointer.display()))?;
    let raw = contents
        .trim_end()
        .strip_prefix("gitdir: ")
        .context("linked worktree pointer is malformed")?;
    anyhow::ensure!(
        !raw.is_empty() && !raw.chars().any(char::is_control),
        "linked worktree pointer is malformed"
    );
    let private = Path::new(raw);
    anyhow::ensure!(
        private.is_absolute(),
        "linked worktree private metadata must be an absolute path"
    );
    let worktrees_root = common_dir.join("worktrees");
    anyhow::ensure!(
        private.parent() == Some(worktrees_root.as_path())
            && private
                .file_name()
                .is_some_and(|name| name.to_str().is_some()),
        "linked worktree private metadata must be a direct child of {}",
        worktrees_root.display()
    );
    anyhow::ensure!(
        private != expected_worktree_root,
        "linked worktree private metadata must not be the worktree itself"
    );
    Ok(private.to_path_buf())
}

/// Bounded ownership snapshot for one destructive managed-worktree operation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ManagedWorktreeOwnership {
    owning_sessions: Vec<String>,
    owning_jobs: Vec<String>,
}

type ManagedWorktreeOwnershipSnapshots<'a> =
    (&'a [session_control::SessionView], &'a [(String, PathBuf)]);

impl ManagedWorktreeOwnership {
    fn is_empty(&self) -> bool {
        self.owning_sessions.is_empty() && self.owning_jobs.is_empty()
    }
}

fn path_holds_target(candidate: &Path, target: &Path) -> bool {
    candidate == target || candidate.starts_with(target)
}

/// Pure ownership decision used by [`managed_worktree_ownership`].
///
/// The current session, every session whose status is not clearly terminal, and
/// every in-process running job are owners. Terminal session states
/// (`stopped`, `crashed`, `degraded`) never own a worktree.
fn managed_worktree_owners_from(
    session: &config::Session,
    target: &Path,
    views: &[session_control::SessionView],
    jobs: &[(String, PathBuf)],
) -> ManagedWorktreeOwnership {
    let mut ownership = ManagedWorktreeOwnership::default();
    if path_holds_target(&session.cwd, target) {
        ownership.owning_sessions.push(session.id.clone());
    }
    for view in views {
        if view.id == session.id {
            continue;
        }
        if matches!(view.status.as_str(), "stopped" | "crashed" | "degraded") {
            continue;
        }
        let workspace_owned = view
            .workspace
            .as_ref()
            .is_some_and(|workspace| path_holds_target(&workspace.workspace_root, target));
        if workspace_owned || path_holds_target(&view.cwd, target) {
            ownership.owning_sessions.push(view.id.clone());
        }
    }
    for (job_id, cwd) in jobs {
        if path_holds_target(cwd, target) {
            ownership.owning_jobs.push(job_id.clone());
        }
    }
    ownership.owning_sessions.sort();
    ownership.owning_sessions.dedup();
    ownership.owning_jobs.sort();
    ownership.owning_jobs.dedup();
    ownership
}

/// Read-only precondition proof for removing one managed worktree.
#[derive(Clone, Debug)]
struct ManagedWorktreeRemovalPlan {
    repository: managed_worktree::ManagedRepository,
    task: String,
    target: PathBuf,
    branch: Option<String>,
    private_git_dir: PathBuf,
    registered_siblings: Vec<PathBuf>,
}

/// Adds one Temote-validated managed worktree root to a session's permitted
/// roots for host-side Git inspection of that exact validated target.
///
/// The root is always the broker-derived direct child that the caller already
/// proved; no request-supplied path participates.
fn managed_target_session(session: &config::Session, target: &Path) -> config::Session {
    let mut widened = session.clone();
    if !widened
        .permitted_directories
        .iter()
        .any(|root| target == *root || target.starts_with(root))
    {
        widened.permitted_directories.push(target.to_path_buf());
        widened.permitted_directories.sort();
        widened.permitted_directories.dedup();
    }
    widened
}

/// Proves that one managed worktree may be removed.
///
/// Every step is read-only and fail-closed: the target must be a registered,
/// non-prunable managed worktree of the selected repository at the exact
/// direct-child path below the trusted canonical managed root, must not be the
/// current session working directory, must not be owned by another live
/// session or running job, and must have no dirty or untracked files.
async fn inspect_managed_worktree_for_removal(
    session: &config::Session,
    repository: managed_worktree::ManagedRepository,
    task: String,
    target: PathBuf,
) -> Result<ManagedWorktreeRemovalPlan> {
    let views = session_control::session_views_for_mcp()
        .await
        .context("cannot determine whether another session owns this managed worktree")?;
    let jobs = snapshot_active_job_ownerships();
    inspect_managed_worktree_for_removal_inner(session, repository, task, target, &views, &jobs)
        .await
}

async fn inspect_managed_worktree_for_removal_inner(
    session: &config::Session,
    repository: managed_worktree::ManagedRepository,
    task: String,
    target: PathBuf,
    views: &[session_control::SessionView],
    jobs: &[(String, PathBuf)],
) -> Result<ManagedWorktreeRemovalPlan> {
    repository.ensure_authority()?;
    anyhow::ensure!(
        managed_worktree::trusted_canonical_managed_root(&repository).as_deref()
            == Some(repository.managed_root()),
        "managed worktree root is not a trusted normal directory: {}",
        repository.managed_root().display()
    );
    let metadata = std::fs::symlink_metadata(&target)
        .with_context(|| format!("cannot inspect managed worktree {}", target.display()))?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "managed worktree target is not a normal directory: {}",
        target.display()
    );
    let canonical_target = std::fs::canonicalize(&target)
        .with_context(|| format!("cannot resolve managed worktree {}", target.display()))?;
    anyhow::ensure!(
        canonical_target == target,
        "managed worktree target must be canonical and not a swapped path: {}",
        target.display()
    );
    anyhow::ensure!(
        target.parent() == Some(repository.managed_root()),
        "managed worktree target must be a direct child of {}",
        repository.managed_root().display()
    );
    anyhow::ensure!(
        target != repository.primary_checkout(),
        "the canonical primary checkout can never be removed as a managed worktree"
    );

    let selected_common_dir = sandbox::git_common_dir(repository.primary_checkout())?;
    anyhow::ensure!(
        sandbox::git_common_dir(&canonical_target)? == selected_common_dir,
        "managed worktree does not belong to the selected repository (common Git directory mismatch): {}",
        target.display()
    );
    anyhow::ensure!(
        sandbox::git_primary_checkout(&canonical_target)? == repository.primary_checkout(),
        "managed worktree primary checkout mismatch: {}",
        target.display()
    );
    let private_git_dir = linked_worktree_private_git_dir(
        &canonical_target,
        &selected_common_dir,
        &canonical_target,
    )?;

    let registered = registered_worktrees(session, repository.primary_checkout()).await?;
    let mut registered_siblings = Vec::new();
    let mut matched = false;
    for entry in &registered {
        let canonical_path = std::fs::canonicalize(&entry.path).ok();
        let registered_common_dir = canonical_path
            .as_deref()
            .and_then(|path| sandbox::git_common_dir(path).ok());
        let registered_primary_checkout = canonical_path
            .as_deref()
            .and_then(|path| sandbox::git_primary_checkout(path).ok());
        let classification = managed_worktree::classify_registered_worktree(
            managed_worktree::RegisteredWorktreeIdentity {
                canonical_path: canonical_path.as_deref(),
                common_dir: registered_common_dir.as_deref(),
                primary_checkout: registered_primary_checkout.as_deref(),
            },
            repository.primary_checkout(),
            managed_worktree::trusted_canonical_managed_root(&repository).as_deref(),
            &selected_common_dir,
        );
        if canonical_path.as_deref() == Some(canonical_target.as_path()) {
            anyhow::ensure!(
                classification == managed_worktree::WorktreeClassification::Managed,
                "target is not a registered Temote-managed worktree: {}",
                target.display()
            );
            anyhow::ensure!(
                !entry.prunable && !entry.bare,
                "target is not a removable live worktree: {}",
                target.display()
            );
            matched = true;
            continue;
        }
        registered_siblings.push(entry.path.clone());
    }
    anyhow::ensure!(
        matched,
        "target is not a registered worktree of the selected repository: {}",
        target.display()
    );

    let branch = sandbox::git_current_branch(&canonical_target)?;

    let ownership = managed_worktree_owners_from(session, &target, views, jobs);
    anyhow::ensure!(
        ownership.is_empty(),
        "managed worktree is in use by {} session(s) and {} running job(s); refusing removal",
        ownership.owning_sessions.len(),
        ownership.owning_jobs.len()
    );

    let status = run_host_git_inspection(
        &managed_target_session(session, &target),
        &target,
        &[
            "git".to_owned(),
            "status".to_owned(),
            "--porcelain".to_owned(),
            "--untracked-files=all".to_owned(),
            "-z".to_owned(),
        ],
    )
    .await?;
    anyhow::ensure!(
        status.status == 0,
        "cannot inspect the managed worktree for dirty or untracked work: {}",
        status.stderr.trim()
    );
    anyhow::ensure!(
        !status.truncated,
        "managed worktree status exceeded the output limit; refusing removal"
    );
    anyhow::ensure!(
        status.stdout.is_empty(),
        "managed worktree contains modified or untracked files; refusing removal without force"
    );

    Ok(ManagedWorktreeRemovalPlan {
        repository,
        task,
        target,
        branch,
        private_git_dir,
        registered_siblings,
    })
}

async fn registered_worktrees(
    session: &config::Session,
    cwd: &Path,
) -> Result<Vec<managed_worktree::RegisteredWorktree>> {
    let output = run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "-c".to_owned(),
            "core.hooksPath=/dev/null".to_owned(),
            "worktree".to_owned(),
            "list".to_owned(),
            "--porcelain".to_owned(),
        ],
    )
    .await?;
    anyhow::ensure!(
        output.status == 0,
        "git worktree list failed: {}",
        output.stderr.trim()
    );
    anyhow::ensure!(
        !output.truncated,
        "git worktree list output was truncated before classification"
    );
    managed_worktree::parse_worktree_list(&output.stdout)
}

fn build_git_worktree_remove_command(target: &Path) -> Vec<String> {
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "worktree".to_owned(),
        "remove".to_owned(),
        "--".to_owned(),
        target.to_string_lossy().into_owned(),
    ]
}

#[allow(clippy::too_many_arguments)]
fn managed_worktree_remove_result(
    status: &str,
    plan: &ManagedWorktreeRemovalPlan,
    output: &sandbox::Output,
    directory_removed: bool,
    metadata_removed: bool,
    siblings_preserved: bool,
    branch_preserved: bool,
    verification_error: Option<&str>,
) -> String {
    let mut value = json!({
        "status": status,
        "repository": plan.repository.repository_name(),
        "path": plan.target.to_string_lossy(),
        "task": plan.task,
        "branch": plan.branch,
        "mutation_committed": output.status == 0,
        "directory_removed": directory_removed,
        "metadata_removed": metadata_removed,
        "siblings_preserved": siblings_preserved,
        "branch_preserved": branch_preserved,
        "exit_code": output.status,
        "stdout": output.stdout,
        "stderr": output.stderr,
        "truncated": output.truncated,
    });
    if let Some(error) = verification_error {
        value["verification_error"] = json!(error);
    }
    value.to_string()
}

/// Post-remove verification. Git success alone is never enough: the directory,
/// the selected worktree's own private metadata and its registration must all
/// be gone while sibling registrations and the branch ref are unchanged.
async fn verify_managed_worktree_removed(
    session: &config::Session,
    plan: &ManagedWorktreeRemovalPlan,
) -> Result<(bool, bool, bool, bool)> {
    let directory_removed = matches!(
        std::fs::symlink_metadata(&plan.target),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    anyhow::ensure!(
        directory_removed,
        "managed worktree directory still exists after removal: {}",
        plan.target.display()
    );
    let metadata_removed = matches!(
        std::fs::symlink_metadata(&plan.private_git_dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    anyhow::ensure!(
        metadata_removed,
        "managed worktree private metadata still exists after removal: {}",
        plan.private_git_dir.display()
    );

    let remaining = registered_worktrees(session, plan.repository.primary_checkout()).await?;
    let mut remaining_paths = Vec::new();
    for entry in &remaining {
        let canonical_path =
            std::fs::canonicalize(&entry.path).unwrap_or_else(|_| entry.path.clone());
        anyhow::ensure!(
            canonical_path != plan.target,
            "managed worktree is still registered after removal: {}",
            plan.target.display()
        );
        remaining_paths.push(entry.path.clone());
    }
    let siblings_preserved = remaining_paths == plan.registered_siblings;
    anyhow::ensure!(
        siblings_preserved,
        "sibling worktree registrations changed during managed worktree removal"
    );

    let branch_preserved = match &plan.branch {
        Some(branch) => {
            let probe = run_host_git_inspection(
                session,
                plan.repository.primary_checkout(),
                &[
                    "git".to_owned(),
                    "show-ref".to_owned(),
                    "--verify".to_owned(),
                    "--quiet".to_owned(),
                    format!("refs/heads/{branch}"),
                ],
            )
            .await?;
            probe.status == 0
        }
        None => true,
    };
    anyhow::ensure!(
        branch_preserved,
        "branch ref was deleted by managed worktree removal"
    );
    Ok((
        directory_removed,
        metadata_removed,
        siblings_preserved,
        branch_preserved,
    ))
}

async fn git_worktree_remove(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let src_root = configured_src_root()?;
    git_worktree_remove_in_src_root(args, session, &src_root, activity).await
}

pub(crate) async fn git_worktree_remove_in_src_root(
    args: &Value,
    session: &config::Session,
    src_root: &Path,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    git_worktree_remove_in_src_root_impl(args, session, src_root, activity, None).await
}

/// Deterministic managed-worktree remove entry point.  The same precondition
/// and mutation path as production is used, but ownership snapshots are
/// supplied by the caller so focused tests and the Git shim do not need a live
/// supervisor socket.
#[cfg(test)]
pub(crate) async fn git_worktree_remove_in_src_root_with_snapshots(
    args: &Value,
    session: &config::Session,
    src_root: &Path,
    activity: Option<&ActivityScope>,
    views: &[session_control::SessionView],
    jobs: &[(String, PathBuf)],
) -> Result<Value> {
    git_worktree_remove_in_src_root_impl(args, session, src_root, activity, Some((views, jobs)))
        .await
}

async fn git_worktree_remove_in_src_root_impl(
    args: &Value,
    session: &config::Session,
    src_root: &Path,
    activity: Option<&ActivityScope>,
    snapshots: Option<ManagedWorktreeOwnershipSnapshots<'_>>,
) -> Result<Value> {
    reject_removed_managed_worktree_arguments(args, &["cwd", "base"])?;
    let task = match args.get("task") {
        Some(value) => Some(value.as_str().context("task must be a string")?),
        None => None,
    };
    let path = match args.get("path") {
        Some(value) => Some(value.as_str().context("path must be a string")?),
        None => None,
    };
    let requested_repository = match args.get("repository") {
        Some(value) => Some(value.as_str().context("repository must be a string")?),
        None => None,
    };
    let cwd = config::resolve_cwd(session, None)?;
    let repository =
        managed_repository_for_requested(requested_repository, session, &cwd, src_root)?;
    let (task, target) = resolve_managed_worktree_target(&repository, task, path)?;
    let plan = match snapshots {
        Some((views, jobs)) => {
            inspect_managed_worktree_for_removal_inner(
                session, repository, task, target, views, jobs,
            )
            .await?
        }
        None => inspect_managed_worktree_for_removal(session, repository, task, target).await?,
    };
    approve_local_git_mutation(
        session,
        plan.repository.primary_checkout(),
        "git_worktree_remove",
        format!(
            "repository={} task={} branch={}",
            plan.repository.repository_name(),
            plan.task,
            plan.branch.as_deref().unwrap_or("(detached)")
        ),
        activity,
    )
    .await?;
    // The approval boundary is not a trust boundary.  First serialize this
    // target against session/job admission; while the guard is held, re-prove
    // every precondition on freshly observed state before the mutation runs.
    let _repository_reservation = managed_worktree::acquire_shared_repository_reservation_async(
        plan.repository.primary_checkout(),
    )
    .await?;
    let _reservation =
        managed_worktree::try_acquire_worktree_reservation_async(&plan.target).await?;
    let plan = match snapshots {
        Some((views, jobs)) => {
            inspect_managed_worktree_for_removal_inner(
                session,
                plan.repository.clone(),
                plan.task.clone(),
                plan.target.clone(),
                views,
                jobs,
            )
            .await?
        }
        None => {
            inspect_managed_worktree_for_removal(
                session,
                plan.repository.clone(),
                plan.task.clone(),
                plan.target.clone(),
            )
            .await?
        }
    };

    let command = build_git_worktree_remove_command(&plan.target);
    let rendered_command = render_command(&command);
    approvals::activity(
        &session.id,
        "Remove managed Git worktree",
        Some(rendered_command.clone()),
    )
    .await;
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    let output = sandbox::run_unrestricted_with_env(
        &command,
        plan.repository.primary_checkout(),
        None,
        &HashMap::new(),
        child_env::SENSITIVE_ENV_NAMES,
    )
    .await;
    let result = match output {
        Ok(output) => {
            let verification = if output.status == 0 {
                verify_managed_worktree_removed(session, &plan)
                    .await
                    .map_err(|error| format!("{error:#}"))
            } else {
                Err("git worktree remove did not complete successfully".to_owned())
            };
            match verification {
                Ok((directory_removed, metadata_removed, siblings_preserved, branch_preserved)) => {
                    Ok(managed_worktree_remove_result(
                        "removed",
                        &plan,
                        &output,
                        directory_removed,
                        metadata_removed,
                        siblings_preserved,
                        branch_preserved,
                        None,
                    ))
                }
                Err(error) => Err(anyhow::anyhow!(managed_worktree_remove_result(
                    if output.status == 0 {
                        "verification_failed"
                    } else {
                        "failed"
                    },
                    &plan,
                    &output,
                    false,
                    false,
                    false,
                    false,
                    Some(&error),
                ))),
            }
        }
        Err(error) => Err(error),
    };
    report_command_finished(session.id.clone(), "git", &rendered_command, &result).await;
    text_result(result?)
}

/// Result of one bounded `git worktree prune` operation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ManagedWorktreePruneObservation {
    registered: Vec<managed_worktree::RegisteredWorktree>,
    prunable: Vec<PathBuf>,
    existing_paths: Vec<PathBuf>,
}

/// Bounded, path-free observation of the repository's worktree registrations.
async fn observe_managed_worktrees(
    session: &config::Session,
    repository: &managed_worktree::ManagedRepository,
) -> Result<ManagedWorktreePruneObservation> {
    let registered = registered_worktrees(session, repository.primary_checkout()).await?;
    anyhow::ensure!(
        registered.len() <= MAX_MANAGED_WORKTREE_LIST_ENTRIES,
        "repository has more than {MAX_MANAGED_WORKTREE_LIST_ENTRIES} registered worktrees; refusing a bounded prune"
    );
    let mut prunable = Vec::new();
    let mut existing_paths = Vec::new();
    for entry in &registered {
        if entry.prunable {
            prunable.push(entry.path.clone());
        }
        if std::fs::symlink_metadata(&entry.path).is_ok() {
            existing_paths.push(entry.path.clone());
        }
    }
    Ok(ManagedWorktreePruneObservation {
        registered,
        prunable,
        existing_paths,
    })
}

/// Refuses a prune when any Git-stale entry is owned by the current session or
/// by another non-terminal session or running job.
///
/// `git worktree prune` has no per-entry filter, so one owned stale entry
/// rejects the whole bounded operation instead of pruning around it.
fn ensure_prunable_entries_unowned_from(
    session: &config::Session,
    prunable: &[PathBuf],
    views: &[session_control::SessionView],
    jobs: &[(String, PathBuf)],
) -> Result<()> {
    for path in prunable {
        let ownership = managed_worktree_owners_from(session, path, views, jobs);
        anyhow::ensure!(
            ownership.is_empty(),
            "stale worktree metadata is owned by {} session(s) and {} running job(s); refusing prune: {}",
            ownership.owning_sessions.len(),
            ownership.owning_jobs.len(),
            path.display()
        );
    }
    Ok(())
}

async fn ensure_prunable_entries_unowned(
    session: &config::Session,
    prunable: &[PathBuf],
) -> Result<()> {
    let views = session_control::session_views_for_mcp()
        .await
        .context("cannot determine whether another session owns stale worktree metadata")?;
    let jobs = snapshot_active_job_ownerships();
    ensure_prunable_entries_unowned_from(session, prunable, &views, &jobs)
}

fn ensure_prunable_entries_covered(
    reservations: &managed_worktree::WorktreeReservations,
    observed: &[PathBuf],
) -> Result<()> {
    let locked = reservations.identities();
    for path in observed {
        let identity = managed_worktree::worktree_reservation_identity(path)?;
        anyhow::ensure!(
            locked.iter().any(|candidate| *candidate == identity),
            "prunable worktree set changed after reservations were acquired; refusing to prune: {}",
            path.display()
        );
    }
    Ok(())
}

fn build_git_worktree_prune_command() -> Vec<String> {
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "worktree".to_owned(),
        "prune".to_owned(),
        "--expire=now".to_owned(),
    ]
}

fn bounded_path_list(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

const MANAGED_WORKTREE_PRUNE_MUTATION_ERROR: &str =
    "git worktree prune did not complete successfully";
const MANAGED_WORKTREE_PRUNE_OBSERVATION_ERROR: &str = "post-prune observation failed";
const MANAGED_WORKTREE_PRUNE_VERIFICATION_ERROR: &str = "post-prune verification failed";

fn bounded_managed_worktree_prune_error(
    output: &sandbox::Output,
    verification_error: &str,
) -> &'static str {
    if output.status != 0 {
        MANAGED_WORKTREE_PRUNE_MUTATION_ERROR
    } else if verification_error == MANAGED_WORKTREE_PRUNE_OBSERVATION_ERROR {
        MANAGED_WORKTREE_PRUNE_OBSERVATION_ERROR
    } else {
        // Never copy an observation or verification error into the public
        // result: both can contain arbitrary paths or child-process output.
        MANAGED_WORKTREE_PRUNE_VERIFICATION_ERROR
    }
}

fn managed_worktree_prune_result(
    status: &str,
    repository: &managed_worktree::ManagedRepository,
    before: &ManagedWorktreePruneObservation,
    after: &ManagedWorktreePruneObservation,
    output: &sandbox::Output,
    verification_error: Option<&str>,
) -> String {
    let removed = before
        .prunable
        .iter()
        .filter(|path| !after.prunable.contains(path))
        .cloned()
        .collect::<Vec<_>>();
    let mut value = json!({
        "status": status,
        "repository": repository.repository_name(),
        "primary_checkout": repository.primary_checkout().to_string_lossy(),
        "before": {
            "registered_count": before.registered.len(),
            "prunable_count": before.prunable.len(),
            "prunable": bounded_path_list(&before.prunable),
        },
        "after": {
            "registered_count": after.registered.len(),
            "prunable_count": after.prunable.len(),
            "prunable": bounded_path_list(&after.prunable),
        },
        "removed_metadata_entries": bounded_path_list(&removed),
        "removed_metadata_count": removed.len(),
        "filesystem_directories_preserved": true,
        "mutation_committed": output.status == 0,
        "exit_code": output.status,
    });
    if let Some(error) = verification_error {
        value["verification_error"] = json!(bounded_managed_worktree_prune_error(output, error));
        value["filesystem_directories_preserved"] = json!(false);
    }
    value.to_string()
}

/// Post-prune verification: only previously stale registrations disappeared,
/// every live registration survived and no filesystem directory was deleted.
fn verify_managed_worktree_prune(
    before: &ManagedWorktreePruneObservation,
    after: &ManagedWorktreePruneObservation,
) -> Result<()> {
    for path in &after.prunable {
        anyhow::ensure!(
            before.prunable.contains(path),
            "prune left unexpected stale metadata behind: {}",
            path.display()
        );
    }
    let live_before = before
        .registered
        .iter()
        .filter(|entry| !entry.prunable)
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    let live_after = after
        .registered
        .iter()
        .filter(|entry| !entry.prunable)
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        live_before == live_after,
        "prune removed or changed a live worktree registration"
    );
    for path in &before.existing_paths {
        anyhow::ensure!(
            std::fs::symlink_metadata(path).is_ok(),
            "prune deleted a filesystem directory: {}",
            path.display()
        );
    }
    Ok(())
}

async fn git_worktree_prune(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let src_root = configured_src_root()?;
    git_worktree_prune_in_src_root(args, session, &src_root, activity).await
}

async fn git_worktree_prune_in_src_root(
    args: &Value,
    session: &config::Session,
    src_root: &Path,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    git_worktree_prune_in_src_root_impl(args, session, src_root, activity, None).await
}

/// Deterministic managed-worktree prune entry point.  The injected snapshots
/// feed the same ownership decision as production while the Git observation,
/// reservation ordering and post-verification stay unchanged.
#[cfg(test)]
pub(crate) async fn git_worktree_prune_in_src_root_with_snapshots(
    args: &Value,
    session: &config::Session,
    src_root: &Path,
    activity: Option<&ActivityScope>,
    views: &[session_control::SessionView],
    jobs: &[(String, PathBuf)],
) -> Result<Value> {
    git_worktree_prune_in_src_root_impl(args, session, src_root, activity, Some((views, jobs)))
        .await
}

async fn git_worktree_prune_in_src_root_impl(
    args: &Value,
    session: &config::Session,
    src_root: &Path,
    activity: Option<&ActivityScope>,
    snapshots: Option<ManagedWorktreeOwnershipSnapshots<'_>>,
) -> Result<Value> {
    reject_removed_managed_worktree_arguments(args, &["cwd", "base", "task", "path"])?;
    let requested_repository = match args.get("repository") {
        Some(value) => Some(value.as_str().context("repository must be a string")?),
        None => None,
    };
    let cwd = config::resolve_cwd(session, None)?;
    let repository =
        managed_repository_for_requested(requested_repository, session, &cwd, src_root)?;
    repository.ensure_authority()?;
    let before = observe_managed_worktrees(session, &repository).await?;
    match snapshots {
        Some((views, jobs)) => {
            ensure_prunable_entries_unowned_from(session, &before.prunable, views, jobs)?
        }
        None => ensure_prunable_entries_unowned(session, &before.prunable).await?,
    }
    approve_local_git_mutation(
        session,
        repository.primary_checkout(),
        "git_worktree_prune",
        format!(
            "repository={} stale_metadata={}",
            repository.repository_name(),
            before.prunable.len()
        ),
        activity,
    )
    .await?;
    // The approval boundary is not a trust boundary.  Lock every currently
    // prunable entry in stable order, then re-observe the set while those
    // locks are held.  A newly observed entry that was not covered by a lock
    // rejects the entire prune instead of being silently removed.
    let _repository_reservation =
        managed_worktree::try_acquire_repository_reservation_async(repository.primary_checkout())
            .await?;
    let before = observe_managed_worktrees(session, &repository).await?;
    let _reservations =
        managed_worktree::try_acquire_worktree_reservations_async(&before.prunable).await?;
    let before = observe_managed_worktrees(session, &repository).await?;
    ensure_prunable_entries_covered(&_reservations, &before.prunable)?;
    match snapshots {
        Some((views, jobs)) => {
            ensure_prunable_entries_unowned_from(session, &before.prunable, views, jobs)?
        }
        None => ensure_prunable_entries_unowned(session, &before.prunable).await?,
    }

    let command = build_git_worktree_prune_command();
    let rendered_command = render_command(&command);
    approvals::activity(
        &session.id,
        "Prune stale Git worktree metadata",
        Some(rendered_command.clone()),
    )
    .await;
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    let output = sandbox::run_unrestricted_with_env(
        &command,
        repository.primary_checkout(),
        None,
        &HashMap::new(),
        child_env::SENSITIVE_ENV_NAMES,
    )
    .await;
    let result = match output {
        Ok(output) => {
            let verification = if output.status == 0 {
                match observe_managed_worktrees(session, &repository).await {
                    Ok(after) => verify_managed_worktree_prune(&before, &after)
                        .map(|()| after)
                        .map_err(|_| MANAGED_WORKTREE_PRUNE_VERIFICATION_ERROR),
                    Err(_) => Err(MANAGED_WORKTREE_PRUNE_OBSERVATION_ERROR),
                }
            } else {
                Err(MANAGED_WORKTREE_PRUNE_MUTATION_ERROR)
            };
            match verification {
                Ok(after) => Ok(managed_worktree_prune_result(
                    "pruned",
                    &repository,
                    &before,
                    &after,
                    &output,
                    None,
                )),
                Err(error) => Err(anyhow::anyhow!(managed_worktree_prune_result(
                    if output.status == 0 {
                        "verification_failed"
                    } else {
                        "failed"
                    },
                    &repository,
                    &before,
                    &ManagedWorktreePruneObservation::default(),
                    &output,
                    Some(error),
                ))),
            }
        }
        Err(error) => Err(error),
    };
    report_command_finished(session.id.clone(), "git", &rendered_command, &result).await;
    text_result(result?)
}

async fn approve_local_git_mutation(
    session: &config::Session,
    cwd: &Path,
    operation: &str,
    detail: String,
    activity: Option<&ActivityScope>,
) -> Result<()> {
    request_activity_approval(
        session,
        ActivityApprovalRequest {
            class: approvals::ApprovalClass::LocalStructured,
            operation,
            detail,
            cwd: cwd.to_path_buf(),
            metadata: BTreeMap::new(),
            denial: "user denied Git mutation",
        },
        activity,
    )
    .await
}

pub(crate) async fn validate_git_branch_name(
    session: &config::Session,
    cwd: &Path,
    branch: &str,
) -> Result<()> {
    anyhow::ensure!(!branch.is_empty(), "branch must not be empty");
    anyhow::ensure!(
        branch.len() <= MAX_GIT_BRANCH_NAME_BYTES,
        "branch must be at most {MAX_GIT_BRANCH_NAME_BYTES} bytes"
    );
    anyhow::ensure!(
        !branch.starts_with('-') && !branch.starts_with("refs/"),
        "branch must be an unqualified local branch name"
    );
    anyhow::ensure!(
        !branch.chars().any(char::is_control),
        "branch must not contain control characters"
    );
    let output = run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "check-ref-format".to_owned(),
            "--branch".to_owned(),
            branch.to_owned(),
        ],
    )
    .await?;
    anyhow::ensure!(output.status == 0, "invalid Git branch name");
    Ok(())
}

fn validate_git_base_ref(base: &str) -> Result<()> {
    anyhow::ensure!(!base.is_empty(), "base must not be empty");
    anyhow::ensure!(
        base.len() <= MAX_GIT_BASE_REF_BYTES,
        "base must be at most {MAX_GIT_BASE_REF_BYTES} bytes"
    );
    anyhow::ensure!(!base.starts_with('-'), "base must not start with '-'");
    anyhow::ensure!(
        base == "HEAD"
            || base.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '/' | '.' | '_' | '-')
            }),
        "base must be a repository-local ref or exact object ID"
    );
    anyhow::ensure!(
        !base.contains("..")
            && !base.contains("//")
            && !base.contains("@{")
            && !base.ends_with('/')
            && !base.ends_with(".lock"),
        "base contains an unsafe Git revision expression"
    );
    Ok(())
}

pub(crate) async fn resolve_git_base_commit(
    session: &config::Session,
    cwd: &Path,
    base: &str,
) -> Result<String> {
    validate_git_base_ref(base)?;
    let output = run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "rev-parse".to_owned(),
            "--verify".to_owned(),
            "--end-of-options".to_owned(),
            format!("{base}^{{commit}}"),
        ],
    )
    .await?;
    anyhow::ensure!(
        output.status == 0,
        "base does not resolve to a repository-local commit"
    );
    let resolved = output.stdout.trim();
    validate_git_object_id(resolved, "resolved base")?;
    Ok(resolved.to_ascii_lowercase())
}

pub(crate) async fn ensure_local_branch_absent(
    session: &config::Session,
    cwd: &Path,
    branch: &str,
) -> Result<()> {
    let output = git_local_branch_probe(session, cwd, branch).await?;
    anyhow::ensure!(output.status != 0, "local Git branch already exists");
    Ok(())
}

pub(crate) async fn ensure_local_branch_exists(
    session: &config::Session,
    cwd: &Path,
    branch: &str,
) -> Result<()> {
    let output = git_local_branch_probe(session, cwd, branch).await?;
    anyhow::ensure!(output.status == 0, "local Git branch does not exist");
    Ok(())
}

async fn git_local_branch_probe(
    session: &config::Session,
    cwd: &Path,
    branch: &str,
) -> Result<sandbox::Output> {
    run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "show-ref".to_owned(),
            "--verify".to_owned(),
            "--quiet".to_owned(),
            format!("refs/heads/{branch}"),
        ],
    )
    .await
}

fn validate_git_worktree_name(name: &str) -> Result<()> {
    anyhow::ensure!(!name.is_empty(), "worktree name must not be empty");
    anyhow::ensure!(
        name.len() <= MAX_GIT_WORKTREE_NAME_BYTES,
        "worktree name must be at most {MAX_GIT_WORKTREE_NAME_BYTES} bytes"
    );
    anyhow::ensure!(
        name != "."
            && name != ".."
            && !name.starts_with('-')
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || matches!(character, '.' | '_' | '-')),
        "worktree name must be a safe repository-local path component"
    );
    Ok(())
}

fn git_worktree_destination(repository_root: &Path, name: &str) -> Result<PathBuf> {
    validate_git_worktree_name(name)?;
    Ok(repository_root.join(".wt").join(name))
}

fn ensure_git_worktree_destination_available(
    repository_root: &Path,
    destination: &Path,
) -> Result<()> {
    anyhow::ensure!(
        destination.starts_with(repository_root.join(".wt")),
        "worktree destination escaped the repository-owned .wt root"
    );
    match std::fs::symlink_metadata(destination) {
        Ok(_) => anyhow::bail!("worktree destination already exists"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect worktree destination {}",
                    destination.display()
                )
            });
        }
    }
    let root = repository_root.join(".wt");
    if let Ok(metadata) = std::fs::symlink_metadata(&root) {
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            ".wt must be a normal directory"
        );
        let canonical = std::fs::canonicalize(&root)?;
        anyhow::ensure!(
            canonical.starts_with(repository_root),
            ".wt escaped the repository root"
        );
    }
    Ok(())
}

fn ensure_git_worktree_root_exists(repository_root: &Path) -> Result<()> {
    let root = repository_root.join(".wt");
    match std::fs::create_dir(&root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&root)?;
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                ".wt must be a normal directory"
            );
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("failed to create {}", root.display())),
    }
}

pub(crate) fn build_git_branch_create_command(branch: &str, base_sha: &str) -> Vec<String> {
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "branch".to_owned(),
        "--no-track".to_owned(),
        branch.to_owned(),
        base_sha.to_owned(),
    ]
}

pub(crate) fn build_git_switch_command(branch: &str) -> Vec<String> {
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "switch".to_owned(),
        "--no-guess".to_owned(),
        branch.to_owned(),
    ]
}

pub(crate) fn build_git_commit_command(message: &str) -> Vec<String> {
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "-c".to_owned(),
        "commit.gpgSign=false".to_owned(),
        "commit".to_owned(),
        "--no-verify".to_owned(),
        "--no-gpg-sign".to_owned(),
        "-m".to_owned(),
        message.to_owned(),
    ]
}

fn build_git_worktree_add_create_command(
    destination: &Path,
    branch: &str,
    base_sha: &str,
) -> Vec<String> {
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "worktree".to_owned(),
        "add".to_owned(),
        "-b".to_owned(),
        branch.to_owned(),
        destination.to_string_lossy().into_owned(),
        base_sha.to_owned(),
    ]
}

fn build_git_worktree_add_existing_command(destination: &Path, branch: &str) -> Vec<String> {
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "worktree".to_owned(),
        "add".to_owned(),
        destination.to_string_lossy().into_owned(),
        branch.to_owned(),
    ]
}

fn validate_git_tag_name(tag: &str) -> Result<()> {
    anyhow::ensure!(!tag.is_empty(), "tag must not be empty");
    anyhow::ensure!(
        tag.len() <= MAX_GIT_TAG_NAME_BYTES,
        "tag must be at most {MAX_GIT_TAG_NAME_BYTES} bytes"
    );
    anyhow::ensure!(
        !tag.starts_with('-') && !tag.starts_with("refs/"),
        "tag must be an unqualified Git tag name"
    );
    anyhow::ensure!(
        !tag.chars().any(char::is_control),
        "tag must not contain control characters"
    );
    Ok(())
}

fn validate_git_object_id(value: &str, field: &str) -> Result<()> {
    anyhow::ensure!(
        matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{field} must be an exact 40- or 64-hex Git object ID"
    );
    Ok(())
}

async fn ensure_git_tag_ref_valid(session: &config::Session, cwd: &Path, tag: &str) -> Result<()> {
    let output = run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "check-ref-format".to_owned(),
            format!("refs/tags/{tag}"),
        ],
    )
    .await?;
    anyhow::ensure!(output.status == 0, "invalid Git tag name");
    Ok(())
}

async fn resolve_exact_git_commit(
    session: &config::Session,
    cwd: &Path,
    source_sha: &str,
) -> Result<String> {
    let output = run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "rev-parse".to_owned(),
            "--verify".to_owned(),
            "--end-of-options".to_owned(),
            format!("{source_sha}^{{commit}}"),
        ],
    )
    .await?;
    anyhow::ensure!(
        output.status == 0,
        "source_sha does not resolve to a local Git commit"
    );
    let resolved = output.stdout.trim();
    validate_git_object_id(resolved, "resolved source_sha")?;
    anyhow::ensure!(
        resolved.eq_ignore_ascii_case(source_sha),
        "source_sha must name the complete commit object ID"
    );
    Ok(resolved.to_ascii_lowercase())
}

fn build_git_push_tag_command(
    remote: &str,
    tag: &str,
    source_sha: &str,
    expected_remote_sha: Option<&str>,
) -> Vec<String> {
    let tag_ref = format!("refs/tags/{tag}");
    let expected = expected_remote_sha.unwrap_or_default();
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "-c".to_owned(),
        "push.recurseSubmodules=off".to_owned(),
        "push".to_owned(),
        format!("--force-with-lease={tag_ref}:{expected}"),
        remote.to_owned(),
        format!("{source_sha}:{tag_ref}"),
    ]
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GithubRepository {
    owner: String,
    repo: String,
}

async fn github_workflow_dispatch(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let remote = optional_git_remote(args)?.unwrap_or_else(|| "origin".to_owned());
    let repository = configured_github_repository(session, &cwd, &remote).await?;
    let workflow = args
        .get("workflow")
        .and_then(Value::as_str)
        .context("missing or non-string workflow")?;
    validate_github_workflow(workflow)?;
    let git_ref = args
        .get("ref")
        .and_then(Value::as_str)
        .context("missing or non-string ref")?;
    validate_github_ref(git_ref)?;
    ensure_generic_git_ref_valid(session, &cwd, git_ref).await?;

    let path = github_workflow_dispatch_path(&repository, workflow);
    let body = json!({
        "ref": git_ref,
        "return_run_details": true,
    });
    let output = run_approved_github_api(
        session,
        cwd,
        &repository,
        GithubApiCall {
            method: GithubApiMethod::Post,
            path: &path,
            body: Some(&body),
            operation: "github_workflow_dispatch",
        },
        activity,
    )
    .await?;
    let response = parse_github_workflow_dispatch_response(&output)?;
    text_result(serde_json::to_string(&response)?)
}

async fn github_workflow_run_get(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let remote = optional_git_remote(args)?.unwrap_or_else(|| "origin".to_owned());
    let repository = configured_github_repository(session, &cwd, &remote).await?;
    let run_id = args
        .get("run_id")
        .and_then(Value::as_str)
        .context("missing or non-string run_id")?;
    let run_id = validate_github_run_id(run_id)?;
    let path = github_workflow_run_get_path(&repository, run_id);
    let output = run_approved_github_api(
        session,
        cwd,
        &repository,
        GithubApiCall {
            method: GithubApiMethod::Get,
            path: &path,
            body: None,
            operation: "github_workflow_run_get",
        },
        activity,
    )
    .await?;
    let response = parse_github_workflow_run_response(&output, run_id)?;
    text_result(serde_json::to_string(&response)?)
}

async fn configured_github_repository(
    session: &config::Session,
    cwd: &Path,
    remote: &str,
) -> Result<GithubRepository> {
    validate_git_remote(remote)?;
    let output = run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "remote".to_owned(),
            "get-url".to_owned(),
            remote.to_owned(),
        ],
    )
    .await?;
    anyhow::ensure!(output.status == 0, "configured Git remote is unavailable");
    let remote_url = output.stdout.trim();
    anyhow::ensure!(
        !remote_url.is_empty() && remote_url.len() <= MAX_GITHUB_REMOTE_URL_BYTES,
        "configured Git remote URL is invalid"
    );
    github_repository_from_remote_url(remote_url)
}

fn github_repository_from_remote_url(remote_url: &str) -> Result<GithubRepository> {
    let path = if let Some(path) = remote_url.strip_prefix("https://github.com/") {
        path
    } else if let Some(path) = remote_url.strip_prefix("git@github.com:") {
        path
    } else if let Some(path) = remote_url.strip_prefix("ssh://git@github.com/") {
        path
    } else {
        anyhow::bail!("configured remote must resolve to github.com")
    };
    anyhow::ensure!(
        !path.contains(['?', '#', '@']),
        "configured GitHub remote URL is invalid"
    );
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut components = path.split('/');
    let owner = components.next().unwrap_or_default();
    let repo = components.next().unwrap_or_default();
    anyhow::ensure!(
        !owner.is_empty() && !repo.is_empty() && components.next().is_none(),
        "configured GitHub remote must identify exactly one owner/repository"
    );
    validate_github_repository_component(owner, false)?;
    validate_github_repository_component(repo, true)?;
    Ok(GithubRepository {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
    })
}

fn validate_github_repository_component(value: &str, allow_dot_underscore: bool) -> Result<()> {
    anyhow::ensure!(value.len() <= 100, "GitHub repository identity is too long");
    anyhow::ensure!(
        value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '-'
                || (allow_dot_underscore && matches!(character, '.' | '_'))
        }),
        "configured GitHub repository identity contains unsafe characters"
    );
    anyhow::ensure!(!value.contains(".."), "invalid GitHub repository identity");
    Ok(())
}

fn validate_github_workflow(workflow: &str) -> Result<()> {
    anyhow::ensure!(
        !workflow.is_empty() && workflow.len() <= MAX_GITHUB_WORKFLOW_BYTES,
        "workflow must be between 1 and {MAX_GITHUB_WORKFLOW_BYTES} bytes"
    );
    if workflow.bytes().all(|byte| byte.is_ascii_digit()) {
        anyhow::ensure!(
            workflow.parse::<u64>().is_ok(),
            "numeric workflow ID is invalid"
        );
        return Ok(());
    }
    anyhow::ensure!(
        workflow.ends_with(".yml") || workflow.ends_with(".yaml"),
        "workflow must be a numeric ID or workflow filename ending in .yml/.yaml"
    );
    anyhow::ensure!(
        workflow
            .chars()
            .all(|character| character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | '-')),
        "workflow filename contains unsafe characters"
    );
    anyhow::ensure!(!workflow.contains(".."), "workflow filename is invalid");
    Ok(())
}

fn validate_github_ref(git_ref: &str) -> Result<()> {
    anyhow::ensure!(
        !git_ref.is_empty() && git_ref.len() <= MAX_GITHUB_REF_BYTES,
        "ref must be between 1 and {MAX_GITHUB_REF_BYTES} bytes"
    );
    anyhow::ensure!(
        !git_ref.starts_with('-') && !git_ref.starts_with("refs/"),
        "ref must be an unqualified branch or tag name"
    );
    anyhow::ensure!(
        !git_ref.chars().any(char::is_control),
        "ref must not contain control characters"
    );
    Ok(())
}

async fn ensure_generic_git_ref_valid(
    session: &config::Session,
    cwd: &Path,
    git_ref: &str,
) -> Result<()> {
    let output = run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "check-ref-format".to_owned(),
            format!("refs/heads/{git_ref}"),
        ],
    )
    .await?;
    anyhow::ensure!(output.status == 0, "invalid Git branch/tag ref");
    Ok(())
}

fn validate_github_run_id(run_id: &str) -> Result<u64> {
    anyhow::ensure!(
        !run_id.is_empty()
            && run_id.len() <= 20
            && run_id.bytes().all(|byte| byte.is_ascii_digit()),
        "run_id must be a positive decimal integer"
    );
    let parsed = run_id.parse::<u64>().context("run_id is out of range")?;
    anyhow::ensure!(parsed > 0, "run_id must be greater than zero");
    Ok(parsed)
}

fn github_workflow_dispatch_path(repository: &GithubRepository, workflow: &str) -> String {
    format!(
        "repos/{}/{}/actions/workflows/{workflow}/dispatches",
        repository.owner, repository.repo
    )
}

fn github_workflow_run_get_path(repository: &GithubRepository, run_id: u64) -> String {
    format!(
        "repos/{}/{}/actions/runs/{run_id}",
        repository.owner, repository.repo
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GithubApiMethod {
    Get,
    Post,
}

impl GithubApiMethod {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

struct GithubApiCall<'a> {
    method: GithubApiMethod,
    path: &'a str,
    #[cfg_attr(not(feature = "network"), allow(dead_code))]
    body: Option<&'a Value>,
    operation: &'a str,
}

async fn run_approved_github_api(
    session: &config::Session,
    cwd: PathBuf,
    repository: &GithubRepository,
    call: GithubApiCall<'_>,
    activity: Option<&ActivityScope>,
) -> Result<String> {
    let repository_root = sandbox::git_worktree_root(&cwd)?;
    config::ensure_permitted(session, &repository_root)
        .context("Git repository root must be inside a permitted session root")?;
    if !approvals::ensure_local_approval_with_activity(
        session,
        approvals::ApprovalClass::GitNetwork,
        call.operation,
        format!("method={} path={}", call.method.as_str(), call.path),
        repository_root.clone(),
        BTreeMap::new(),
        activity,
    )
    .await?
    {
        if let Some(activity) = activity {
            let _ = activity
                .fail_with_summary(ActivitySummary::failure(ActivityErrorKind::ApprovalDenied));
        }
        anyhow::bail!("user denied {}", call.operation)
    }
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    #[cfg(feature = "network")]
    {
        let token = repo_scoped_github_token(session, &repository_root, repository).await?;
        github_api_request(&token, call.method, call.path, call.body).await
    }
    #[cfg(not(feature = "network"))]
    {
        let _ = (repository, call);
        anyhow::bail!("GitHub API operations require the network feature")
    }
}

#[cfg(feature = "network")]
async fn github_api_request(
    token: &str,
    method: GithubApiMethod,
    path: &str,
    body: Option<&Value>,
) -> Result<String> {
    const MAX_GITHUB_API_RESPONSE_BYTES: u64 = 256 * 1024;
    anyhow::ensure!(
        !path.is_empty()
            && path.len() <= 1024
            && !path.starts_with('/')
            && !path.contains("..")
            && path
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || matches!(character, '/' | '.' | '_' | '-')),
        "GitHub API path is invalid"
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .user_agent(format!("temote-mcp/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to initialize GitHub API client")?;
    let url = format!("https://api.github.com/{path}");
    let mut request = match method {
        GithubApiMethod::Get => client.get(url),
        GithubApiMethod::Post => client.post(url),
    }
    .header("Accept", "application/vnd.github+json")
    .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("GitHub API is unavailable"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!(github_api_error_message(status.as_u16()));
    }
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= MAX_GITHUB_API_RESPONSE_BYTES,
            "GitHub API response is too large"
        );
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| anyhow::anyhow!("GitHub API response could not be read"))?;
    anyhow::ensure!(
        bytes.len() <= MAX_GITHUB_API_RESPONSE_BYTES as usize,
        "GitHub API response is too large"
    );
    String::from_utf8(bytes.to_vec()).context("GitHub API response is not UTF-8")
}

#[cfg_attr(not(feature = "network"), allow(dead_code))]
fn github_api_error_message(status: u16) -> &'static str {
    match status {
        401 => "GitHub repository credential was rejected",
        403 => "GitHub repository credential lacks required permission",
        404 => "GitHub repository workflow/run is unavailable",
        422 => "GitHub workflow request was rejected",
        _ => "GitHub API operation failed",
    }
}

#[cfg(feature = "network")]
async fn repo_scoped_github_token(
    session: &config::Session,
    repository_root: &Path,
    repository: &GithubRepository,
) -> Result<zeroize::Zeroizing<String>> {
    let local_helpers = run_host_git_inspection(
        session,
        repository_root,
        &[
            "git".to_owned(),
            "config".to_owned(),
            "--local".to_owned(),
            "--includes".to_owned(),
            "--get-all".to_owned(),
            "credential.helper".to_owned(),
        ],
    )
    .await?;
    anyhow::ensure!(local_helpers.status == 0, GITHUB_CREDENTIAL_MAPPING_ERROR);
    let local_use_http_path = run_host_git_inspection(
        session,
        repository_root,
        &[
            "git".to_owned(),
            "config".to_owned(),
            "--local".to_owned(),
            "--includes".to_owned(),
            "--get".to_owned(),
            "credential.useHttpPath".to_owned(),
        ],
    )
    .await?;
    anyhow::ensure!(
        local_use_http_path.status == 0
            && repo_scoped_github_credential_mapping_valid(
                &local_helpers.stdout,
                &local_use_http_path.stdout,
            ),
        GITHUB_CREDENTIAL_MAPPING_ERROR
    );

    let path = format!("{}/{}.git", repository.owner, repository.repo);
    let input = format!("protocol=https\nhost=github.com\npath={path}\n\n");
    let environment = HashMap::from([
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        ("GCM_INTERACTIVE".to_owned(), "Never".to_owned()),
        ("GIT_ASKPASS".to_owned(), "/bin/false".to_owned()),
    ]);
    let output = sandbox::run_unrestricted_with_env(
        &github_managed_credential_command(),
        repository_root,
        Some(input.as_bytes()),
        &environment,
        child_env::SENSITIVE_ENV_NAMES,
    )
    .await?;
    anyhow::ensure!(
        output.status == 0,
        "GitHub repository credential is unavailable"
    );
    let credential_stdout = zeroize::Zeroizing::new(output.stdout);
    let token = parse_repo_scoped_github_credential(&credential_stdout)?;
    Ok(zeroize::Zeroizing::new(token))
}

#[cfg_attr(not(feature = "network"), allow(dead_code))]
fn repo_scoped_github_credential_mapping_valid(
    local_helpers_stdout: &str,
    local_use_http_path_stdout: &str,
) -> bool {
    let local_helpers = local_helpers_stdout.lines().collect::<Vec<_>>();
    local_helpers == ["", "!gh git credential --managed"]
        && local_use_http_path_stdout
            .trim()
            .eq_ignore_ascii_case("true")
}

#[cfg_attr(not(feature = "network"), allow(dead_code))]
fn github_managed_credential_command() -> Vec<String> {
    vec![
        "gh".to_owned(),
        "git".to_owned(),
        "credential".to_owned(),
        "--managed".to_owned(),
        "get".to_owned(),
    ]
}

#[cfg_attr(not(feature = "network"), allow(dead_code))]
fn parse_repo_scoped_github_credential(stdout: &str) -> Result<String> {
    const MAX_CREDENTIAL_RESPONSE_BYTES: usize = 16 * 1024;
    const MAX_GITHUB_TOKEN_BYTES: usize = 4096;
    anyhow::ensure!(
        stdout.len() <= MAX_CREDENTIAL_RESPONSE_BYTES,
        "GitHub repository credential response is too large"
    );
    let mut protocol = None;
    let mut host = None;
    let mut username = None;
    let mut password = None;
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .context("GitHub repository credential response is malformed")?;
        anyhow::ensure!(
            !value.chars().any(char::is_control),
            "GitHub repository credential response is malformed"
        );
        let slot = match key {
            "protocol" => &mut protocol,
            "host" => &mut host,
            "username" => &mut username,
            "password" => &mut password,
            _ => continue,
        };
        anyhow::ensure!(
            slot.replace(value.to_owned()).is_none(),
            "GitHub repository credential response is malformed"
        );
    }
    anyhow::ensure!(
        protocol.as_deref() == Some("https"),
        "GitHub repository credential mismatch"
    );
    anyhow::ensure!(
        host.as_deref() == Some("github.com"),
        "GitHub repository credential mismatch"
    );
    let username = username.context("GitHub repository credential is incomplete")?;
    anyhow::ensure!(
        !username.is_empty() && username.len() <= 256,
        "GitHub repository credential is incomplete"
    );
    let password = password.context("GitHub repository credential is incomplete")?;
    anyhow::ensure!(
        !password.is_empty() && password.len() <= MAX_GITHUB_TOKEN_BYTES,
        "GitHub repository credential is incomplete"
    );
    Ok(password)
}

fn parse_github_workflow_dispatch_response(stdout: &str) -> Result<Value> {
    anyhow::ensure!(
        stdout.len() <= 64 * 1024,
        "GitHub dispatch response is too large"
    );
    let value: Value = serde_json::from_str(stdout).context("invalid GitHub dispatch response")?;
    let run_id = value
        .get("workflow_run_id")
        .and_then(Value::as_u64)
        .context("GitHub dispatch response is missing workflow_run_id")?;
    anyhow::ensure!(
        run_id > 0,
        "GitHub dispatch returned invalid workflow_run_id"
    );
    let html_url = bounded_github_html_url(value.get("html_url"))?;
    Ok(json!({
        "workflow_run_id": run_id.to_string(),
        "html_url": html_url,
    }))
}

fn parse_github_workflow_run_response(stdout: &str, expected_run_id: u64) -> Result<Value> {
    anyhow::ensure!(
        stdout.len() <= 256 * 1024,
        "GitHub workflow run response is too large"
    );
    let value: Value =
        serde_json::from_str(stdout).context("invalid GitHub workflow run response")?;
    let run_id = value
        .get("id")
        .and_then(Value::as_u64)
        .context("GitHub workflow run response is missing id")?;
    anyhow::ensure!(run_id == expected_run_id, "GitHub workflow run ID mismatch");
    let status = bounded_github_enum(value.get("status"), "status")?;
    let conclusion = match value.get("conclusion") {
        None | Some(Value::Null) => None,
        Some(value) => Some(bounded_github_enum(Some(value), "conclusion")?),
    };
    let event = bounded_github_enum(value.get("event"), "event")?;
    let head_sha = value
        .get("head_sha")
        .and_then(Value::as_str)
        .context("GitHub workflow run response is missing head_sha")?;
    validate_git_object_id(head_sha, "GitHub workflow head_sha")?;
    let html_url = bounded_github_html_url(value.get("html_url"))?;
    Ok(json!({
        "run_id": run_id.to_string(),
        "status": status,
        "conclusion": conclusion,
        "event": event,
        "head_sha": head_sha.to_ascii_lowercase(),
        "html_url": html_url,
    }))
}

fn bounded_github_enum<'a>(value: Option<&'a Value>, field: &str) -> Result<&'a str> {
    let value = value
        .and_then(Value::as_str)
        .with_context(|| format!("GitHub response is missing {field}"))?;
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value.chars().all(
                |character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            ),
        "GitHub response contains invalid {field}"
    );
    Ok(value)
}

fn bounded_github_html_url(value: Option<&Value>) -> Result<&str> {
    let value = value
        .and_then(Value::as_str)
        .context("GitHub response is missing html_url")?;
    anyhow::ensure!(
        value.len() <= 2048 && value.starts_with("https://github.com/"),
        "GitHub response contains invalid html_url"
    );
    Ok(value)
}

fn optional_git_remote(args: &Value) -> Result<Option<String>> {
    let Some(value) = args.get("remote") else {
        return Ok(None);
    };
    let remote = value.as_str().context("remote must be a string")?;
    validate_git_remote(remote)?;
    Ok(Some(remote.to_owned()))
}

async fn ensure_configured_git_remote(
    session: &config::Session,
    cwd: &Path,
    remote: &str,
) -> Result<()> {
    validate_git_remote(remote)?;
    let _ = resolve_git_remote_destinations(session, cwd, remote, GitRemoteOperation::Fetch)
        .await
        .with_context(|| format!("Git remote {remote:?} is not configured"))?;
    Ok(())
}

async fn git_config_values(
    session: &config::Session,
    cwd: &Path,
    key: &str,
) -> Result<Vec<String>> {
    let output = run_host_git_inspection(
        session,
        cwd,
        &[
            "git".to_owned(),
            "config".to_owned(),
            "--get-all".to_owned(),
            key.to_owned(),
        ],
    )
    .await?;
    anyhow::ensure!(
        !output.truncated && output.stdout.len() <= MAX_GIT_CONFIG_OUTPUT_BYTES,
        GIT_PULL_UPSTREAM_CONFIGURATION_ERROR
    );
    if output.status != 0 {
        // Git uses status 1 for a missing key. Any other failure is treated as
        // unavailable rather than returning config/parser diagnostics.
        anyhow::ensure!(output.status == 1, GIT_PULL_UPSTREAM_CONFIGURATION_ERROR);
        return Ok(Vec::new());
    }
    let mut values = output.stdout.split('\n').collect::<Vec<_>>();
    if values.last() == Some(&"") {
        values.pop();
    }
    anyhow::ensure!(
        !values.is_empty() && values.len() <= MAX_GIT_CONFIG_VALUES,
        GIT_PULL_UPSTREAM_CONFIGURATION_ERROR
    );
    let mut parsed = Vec::with_capacity(values.len());
    for value in values {
        anyhow::ensure!(
            !value.is_empty()
                && value.len() <= MAX_GIT_BASE_REF_BYTES
                && value == value.trim()
                && !value.chars().any(char::is_control),
            GIT_PULL_UPSTREAM_CONFIGURATION_ERROR
        );
        parsed.push(value.to_owned());
    }
    Ok(parsed)
}

async fn run_host_git_inspection(
    session: &config::Session,
    cwd: &Path,
    command: &[String],
) -> Result<sandbox::Output> {
    let repository_root = sandbox::git_worktree_root(cwd)?;
    config::ensure_permitted(session, &repository_root)
        .context("Git repository root must be inside a permitted session root")?;
    sandbox::run_unrestricted_with_env(
        command,
        &repository_root,
        None,
        &HashMap::new(),
        child_env::SENSITIVE_ENV_NAMES,
    )
    .await
}

fn validate_git_remote(remote: &str) -> Result<()> {
    anyhow::ensure!(!remote.is_empty(), "Git remote must not be empty");
    anyhow::ensure!(remote.len() <= 255, "Git remote is too long");
    anyhow::ensure!(
        !remote.starts_with('-')
            && !remote.starts_with('/')
            && !remote.ends_with('/')
            && !remote.contains("..")
            && !remote.contains("//"),
        "unsafe Git remote name: {remote:?}"
    );
    anyhow::ensure!(
        remote
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_./".contains(character)),
        "Git remote must be a configured name, not a URL or refspec: {remote:?}"
    );
    Ok(())
}

async fn run_approved_git_command(
    session: &config::Session,
    cwd: PathBuf,
    command: Vec<String>,
    operation: &str,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let output = run_approved_git_output(session, cwd, command, operation, activity).await?;
    text_result(render_output(output)?)
}

async fn run_approved_git_output(
    session: &config::Session,
    cwd: PathBuf,
    command: Vec<String>,
    operation: &str,
    activity: Option<&ActivityScope>,
) -> Result<sandbox::Output> {
    run_approved_git_output_inner(session, cwd, command, operation, activity, false).await
}

async fn run_approved_git_network_output(
    session: &config::Session,
    cwd: PathBuf,
    command: Vec<String>,
    operation: &str,
    activity: Option<&ActivityScope>,
    github_https_destination: bool,
) -> Result<sandbox::Output> {
    run_approved_git_output_inner(
        session,
        cwd,
        command,
        operation,
        activity,
        github_https_destination,
    )
    .await
}

async fn run_approved_git_output_inner(
    session: &config::Session,
    cwd: PathBuf,
    command: Vec<String>,
    operation: &str,
    activity: Option<&ActivityScope>,
    github_https_destination: bool,
) -> Result<sandbox::Output> {
    let repository_root = sandbox::git_worktree_root(&cwd)?;
    config::ensure_permitted(session, &repository_root)
        .context("Git repository root must be inside a permitted session root")?;
    if !approvals::ensure_local_approval_with_activity(
        session,
        approvals::ApprovalClass::GitNetwork,
        operation,
        format!("argv: {command:?}"),
        repository_root.clone(),
        BTreeMap::new(),
        activity,
    )
    .await?
    {
        if let Some(activity) = activity {
            let _ = activity
                .fail_with_summary(ActivitySummary::failure(ActivityErrorKind::ApprovalDenied));
        }
        anyhow::bail!("user denied {operation}")
    }
    let rendered_command = render_command(&command);
    approvals::activity(&session.id, format!("Running {rendered_command}"), None).await;
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    let output = sandbox::run_unrestricted_with_env(
        &command,
        &repository_root,
        None,
        &HashMap::new(),
        child_env::SENSITIVE_ENV_NAMES,
    )
    .await
    .map(|output| {
        if github_https_destination {
            sanitize_github_https_network_git_output(output)
        } else {
            output
        }
    });
    let reported = match &output {
        Ok(output) => render_output(output.clone()),
        Err(error) => Err(anyhow::anyhow!("{error:#}")),
    };
    report_command_finished(session.id.clone(), "git", &rendered_command, &reported).await;
    output
}

fn sanitize_github_https_network_git_output(mut output: sandbox::Output) -> sandbox::Output {
    if let Some(message) = classify_github_https_network_git_error(&output) {
        output.stdout.clear();
        output.stderr = message.to_owned();
        output.truncated = false;
    }
    output
}

/// Maps a failed GitHub HTTPS network Git process to a bounded, secret-free
/// policy message. A successful process is never classified, even if it emits
/// warning text on stderr.
pub(crate) fn classify_github_https_network_git_error(
    output: &sandbox::Output,
) -> Option<&'static str> {
    if output.status == 0 {
        return None;
    }
    let mut text = String::with_capacity(output.stdout.len() + output.stderr.len() + 1);
    text.push_str(&output.stdout);
    text.push('\n');
    text.push_str(&output.stderr);
    let text = text.to_ascii_lowercase();

    let permission_patterns = [
        "403",
        "forbidden",
        "permission denied",
        "write access denied",
        "write-access-denied",
        "write access to repository not granted",
        "does not have permission",
        "not allowed to push",
        "protected branch hook declined",
        "permission to ",
    ];
    if permission_patterns
        .iter()
        .any(|pattern| text.contains(pattern))
    {
        return Some(GITHUB_CREDENTIAL_PERMISSION_ERROR);
    }

    let unavailable_patterns = [
        "401",
        "authentication failed",
        "authentication required",
        "could not read username",
        "could not read password",
        "no such device or address",
        "terminal prompts disabled",
        "no credentials available",
        "credential unavailable",
        "invalid username or password",
        "bad credentials",
        "repository not found",
    ];
    if unavailable_patterns
        .iter()
        .any(|pattern| text.contains(pattern))
    {
        return Some(GITHUB_CREDENTIAL_UNAVAILABLE_ERROR);
    }

    Some(GITHUB_NETWORK_GIT_ERROR)
}

fn required_string_array(args: &Value, name: &str) -> Result<Vec<String>> {
    args.get(name)
        .and_then(Value::as_array)
        .context(format!("missing {name}"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .context(format!("{name} entries must be strings"))
        })
        .collect()
}

fn resolve_git_add_path(session: &config::Session, path: &str) -> Result<String> {
    Ok(validate_git_path(session, path)?.display().to_string())
}

/// Bounded, option-free Git path syntax shared by the structured `git_add`
/// tool and the local-agent Git broker.
pub(crate) fn validate_git_path_syntax(path: &str) -> Result<()> {
    validate_path_argument(path, "Git path")?;
    anyhow::ensure!(!path.is_empty(), "Git path must not be empty");
    anyhow::ensure!(
        !path.starts_with('-'),
        "Git path must not start with '-': {path:?}"
    );
    anyhow::ensure!(
        !path.starts_with(':') && !path.chars().any(|character| "*?[]".contains(character)),
        "Git pathspecs and glob patterns are not supported: {path:?}"
    );
    Ok(())
}

/// Commit message bounds shared by the structured `git_commit` tool and the
/// local-agent Git broker.
pub(crate) fn validate_git_commit_message(message: &str) -> Result<()> {
    anyhow::ensure!(!message.trim().is_empty(), "message must not be empty");
    anyhow::ensure!(
        message.len() <= MAX_GIT_COMMIT_MESSAGE_BYTES,
        "message must be at most {MAX_GIT_COMMIT_MESSAGE_BYTES} bytes"
    );
    Ok(())
}

fn validate_git_path(session: &config::Session, path: &str) -> Result<PathBuf> {
    validate_git_path_syntax(path)?;

    let path = PathBuf::from(path);
    match config::resolve_existing_path(session, &path) {
        Ok(_) => {}
        Err(_) => {
            config::resolve_write_path(session, &path)?;
        }
    };
    let candidate = if path.is_absolute() {
        path
    } else {
        session.cwd.join(path)
    };
    Ok(candidate)
}

pub(crate) async fn ensure_staged_paths_are_permitted(
    session: &config::Session,
    cwd: &Path,
) -> Result<()> {
    let command = [
        "git".to_owned(),
        "diff".to_owned(),
        "--cached".to_owned(),
        "--name-only".to_owned(),
        "-z".to_owned(),
        "--no-renames".to_owned(),
    ];
    let output = if session.yolo() {
        sandbox::run_unrestricted(&command, cwd, None).await?
    } else {
        sandbox::run(&command, cwd, &session.permitted_directories, None).await?
    };
    anyhow::ensure!(
        output.status == 0,
        "cannot inspect the Git index: {}",
        output.stderr.trim()
    );
    anyhow::ensure!(
        !output.truncated,
        "cannot inspect the Git index because its path list exceeded the output limit"
    );
    let repository_root = sandbox::git_worktree_root(cwd)?;
    for path in output.stdout.split('\0').filter(|path| !path.is_empty()) {
        let path = Path::new(path);
        anyhow::ensure!(
            !path.is_absolute(),
            "Git returned an absolute staged path: {}",
            path.display()
        );
        let path = repository_root.join(path);
        validate_git_path(session, &path.to_string_lossy()).with_context(|| {
            format!(
                "staged Git path is outside the session roots: {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

async fn run_git_and_report(
    session: &config::Session,
    cwd: PathBuf,
    command: Vec<String>,
    title: &str,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let rendered_command = render_command(&command);
    approvals::activity(&session.id, title, Some(rendered_command.clone())).await;
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    let output = if session.yolo() {
        sandbox::run_unrestricted(&command, &cwd, None).await
    } else {
        let git_roots = sandbox::git_metadata_roots(&cwd)?;
        sandbox::run_git(
            &command,
            &cwd,
            &session.permitted_directories,
            &git_roots,
            None,
        )
        .await
    };
    let result = output.and_then(render_output);
    report_command_finished(session.id.clone(), "git", &rendered_command, &result).await;
    text_result(result?)
}

async fn run_git_worktree_add_and_report(
    session: &config::Session,
    cwd: PathBuf,
    command: Vec<String>,
    title: &str,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let rendered_command = render_command(&command);
    approvals::activity(&session.id, title, Some(rendered_command.clone())).await;
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    let output = if session.yolo() {
        sandbox::run_unrestricted(&command, &cwd, None).await
    } else {
        let git_roots = sandbox::git_metadata_roots(&cwd)?;
        sandbox::run_git_worktree_add(
            &command,
            &cwd,
            &session.permitted_directories,
            &git_roots,
            None,
        )
        .await
    };
    let result = output.and_then(render_output);
    report_command_finished(session.id.clone(), "git", &rendered_command, &result).await;
    text_result(result?)
}

async fn execute(
    args: &Value,
    session: &config::Session,
    activity: Option<ActivityScope>,
) -> Result<Value> {
    let output_policy = parse_output_policy(args)?;
    let (rendered_command, _cwd, handle, completion) =
        spawn_sandboxed_command(args, session, activity).await?;

    finish_foreground_or_store_job(session, rendered_command, handle, completion, output_policy)
        .await
}

async fn finish_foreground_or_store_job(
    session: &config::Session,
    rendered_command: String,
    mut handle: JoinHandle<()>,
    completion: Arc<Mutex<JobCompletion>>,
    output_policy: OutputPolicy,
) -> Result<Value> {
    match tokio::time::timeout(FOREGROUND_TIMEOUT, &mut handle).await {
        Ok(joined) => {
            joined.context("command task failed")?;
            let result = completion
                .lock()
                .unwrap()
                .result
                .clone()
                .context("command task completed without a cached result")?;
            cached_job_result(result, output_policy)
        }
        Err(_) => {
            store_job(
                session,
                rendered_command,
                handle,
                completion,
                output_policy,
                "Backgrounded",
            )
            .await
        }
    }
}

async fn start_command(
    args: &Value,
    session: &config::Session,
    activity: Option<ActivityScope>,
) -> Result<Value> {
    let output_policy = parse_output_policy(args)?;
    let (rendered_command, _cwd, handle, completion) =
        spawn_sandboxed_command(args, session, activity).await?;
    store_job(
        session,
        rendered_command,
        handle,
        completion,
        output_policy,
        "Started",
    )
    .await
}

/// Resolves the optional managed-worktree workspace for one `local_agent_run`.
///
/// The caller supplies only an existing local branch and an optional task name;
/// Temote derives the managed path, reuses only a verified managed worktree of
/// the selected repository and branch, and otherwise creates one through the
/// approved `git_worktree_create` path. A caller-supplied `cwd` together with
/// `worktree` is rejected, so no request can inject a path around the managed
/// workspace authority.
async fn local_agent_managed_worktree_binding(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Option<ManagedWorktreeBinding>> {
    if args.get("worktree").is_none() {
        return Ok(None);
    }
    let src_root = configured_src_root()?;
    local_agent_managed_worktree_binding_with_src_root(args, session, &src_root, activity).await
}

async fn local_agent_managed_worktree_binding_with_src_root(
    args: &Value,
    session: &config::Session,
    src_root: &Path,
    activity: Option<&ActivityScope>,
) -> Result<Option<ManagedWorktreeBinding>> {
    let Some(request) = args.get("worktree") else {
        return Ok(None);
    };
    anyhow::ensure!(
        args.get("cwd").is_none(),
        "local_agent_run accepts cwd or worktree, not both; the managed worktree path is always derived by Temote"
    );
    let request = request.as_object().context("worktree must be an object")?;
    anyhow::ensure!(
        request
            .keys()
            .all(|key| matches!(key.as_str(), "branch" | "task")),
        "worktree accepts only branch and task"
    );
    let branch = request
        .get("branch")
        .and_then(Value::as_str)
        .context("worktree.branch is required")?;
    let task = match request.get("task") {
        Some(value) => Some(value.as_str().context("worktree.task must be a string")?),
        None => None,
    };

    let cwd = config::resolve_cwd(session, None)?;
    let repository = managed_repository_for_requested(None, session, &cwd, src_root)?;
    validate_git_branch_name(session, repository.primary_checkout(), branch).await?;
    ensure_local_branch_exists(session, repository.primary_checkout(), branch).await?;
    let task = match task {
        Some(task) => task.to_owned(),
        None => managed_worktree::derive_task_name(branch)?,
    };
    let target = repository.target(&task)?;

    if std::fs::symlink_metadata(&target).is_ok() {
        // Reuse is allowed only for the selected repository's own managed
        // worktree on the requested branch. Legacy, wrong-repository or
        // wrong-branch targets fail closed instead of being adopted.
        let selected_common_dir = sandbox::git_common_dir(repository.primary_checkout())?;
        managed_worktree::verify_reusable_managed_worktree(
            &repository,
            &target,
            branch,
            &selected_common_dir,
            repository.primary_checkout(),
        )
        .context("the existing managed worktree target cannot be reused")?;
        return Ok(Some(ManagedWorktreeBinding {
            repository,
            target,
            branch: branch.to_owned(),
        }));
    }

    let (binding, _) =
        create_managed_worktree(session, src_root, branch, Some(&task), None, activity).await?;
    Ok(Some(binding))
}

async fn local_agent_run(
    args: &Value,
    session: &config::Session,
    executable: Option<&Path>,
    activity: Option<ActivityScope>,
) -> Result<Value> {
    local_agent_run_with_src_root(args, session, executable, activity, None).await
}

async fn local_agent_run_with_src_root(
    args: &Value,
    session: &config::Session,
    executable: Option<&Path>,
    activity: Option<ActivityScope>,
    src_root: Option<&Path>,
) -> Result<Value> {
    local_agent_run_with_src_root_at_boundary(args, session, executable, activity, src_root, || {})
        .await
}

/// Internal variant of [`local_agent_run_with_src_root`] used by the
/// launch-boundary regression tests.
///
/// `boundary` runs after the final managed-worktree identity validation has
/// been attached to the prepared run and before the local agent task is
/// spawned. Production always passes a no-op; the callback observes the same
/// authority state, so it cannot bypass or weaken any validation.
async fn local_agent_run_with_src_root_at_boundary<F>(
    args: &Value,
    session: &config::Session,
    executable: Option<&Path>,
    activity: Option<ActivityScope>,
    src_root: Option<&Path>,
    boundary: F,
) -> Result<Value>
where
    F: FnOnce(),
{
    let binding = match src_root {
        Some(src_root) => {
            local_agent_managed_worktree_binding_with_src_root(
                args,
                session,
                src_root,
                activity.as_ref(),
            )
            .await?
        }
        None => local_agent_managed_worktree_binding(args, session, activity.as_ref()).await?,
    };
    let effective_args = match &binding {
        Some(binding) => {
            let mut value = args.clone();
            if let Some(object) = value.as_object_mut() {
                object.remove("worktree");
                object.insert(
                    "cwd".to_owned(),
                    json!(binding.workspace_root().to_string_lossy().into_owned()),
                );
            }
            value
        }
        None => args.clone(),
    };
    // The managed binding adds exactly the validated workspace root to this
    // run's sandbox session; the on-disk session keeps its own scope.
    let run_session = match &binding {
        Some(binding) => binding.run_session(session),
        None => session.clone(),
    };
    let mut prepared = match executable {
        Some(executable) => {
            local_agent::prepare_with_executable(&effective_args, &run_session, executable)?
        }
        None => local_agent::prepare(&effective_args, &run_session)?,
    };
    if approvals::local_approval(
        session.permission_mode,
        approvals::ApprovalClass::LocalAgent,
    ) != approvals::LocalApproval::Skip
    {
        let detail = prepared.approval_detail();
        approvals::ensure_approval_detail_fits(&detail)?;
        let approved = approvals::ensure_local_approval_with_activity(
            session,
            approvals::ApprovalClass::LocalAgent,
            "local_agent_run",
            detail,
            prepared.cwd.clone(),
            prepared.approval_metadata(),
            activity.as_ref(),
        )
        .await?;
        if !approved {
            if let Some(activity) = &activity {
                let _ = activity
                    .fail_with_summary(ActivitySummary::failure(ActivityErrorKind::ApprovalDenied));
            }
            anyhow::bail!("user denied local_agent_run");
        }
    }

    let current_session = config::load_session(&session.id).await?;
    anyhow::ensure!(
        current_session.started_at == session.started_at
            && current_session.process_id == session.process_id,
        "session instance changed while local agent approval was pending"
    );
    // The on-disk session must keep its exact scope; only this run's derived
    // session may include the validated managed workspace root.
    anyhow::ensure!(
        current_session.cwd == session.cwd
            && current_session.permitted_directories == session.permitted_directories,
        "session workspace changed while local agent approval was pending"
    );
    match executable {
        Some(executable) => prepared.revalidate_with_executable(&run_session, executable)?,
        None => prepared.revalidate(&run_session)?,
    }
    if let Some(binding) = &binding {
        anyhow::ensure!(
            prepared.cwd == binding.workspace_root(),
            "local agent workspace changed while approval was pending"
        );
        let src_root = match src_root {
            Some(src_root) => src_root.to_path_buf(),
            None => configured_src_root()?,
        };
        let expected_identity = binding.revalidate(&src_root)?;
        prepared.bind_managed_worktree_identity(expected_identity)?;
    }
    // The broker and the sandbox launch compare against the identity above,
    // so the final validation and the actual spawn target cannot diverge into
    // a different repository that happens to live at the same path.
    boundary();
    let (description, mut handle, completion) =
        spawn_local_agent(prepared, &current_session, activity).await?;
    match tokio::time::timeout(FOREGROUND_TIMEOUT, &mut handle).await {
        Ok(joined) => {
            joined.context("local agent task failed")?;
            let result = completion
                .lock()
                .unwrap()
                .result
                .clone()
                .context("local agent task completed without a cached result")?;
            cached_job_result(result, OutputPolicy::default())
        }
        Err(_) => {
            store_job(
                session,
                description,
                handle,
                completion,
                OutputPolicy::default(),
                "Backgrounded",
            )
            .await
        }
    }
}

async fn spawn_local_agent(
    prepared: local_agent::PreparedRun,
    session: &config::Session,
    activity: Option<ActivityScope>,
) -> Result<(String, JoinHandle<()>, Arc<Mutex<JobCompletion>>)> {
    spawn_local_agent_with_controls(
        prepared,
        session,
        activity,
        wait_for_session_stop(session.id.clone()),
        MAX_JOB_LIFETIME,
    )
    .await
}

async fn spawn_local_agent_with_controls<F>(
    prepared: local_agent::PreparedRun,
    session: &config::Session,
    activity: Option<ActivityScope>,
    session_stop: F,
    max_lifetime: Duration,
) -> Result<(String, JoinHandle<()>, Arc<Mutex<JobCompletion>>)>
where
    F: Future<Output = ()> + Send + 'static,
{
    let slot = reserve_job_slot_for_cwd(&session.id, &prepared.cwd).await?;
    let description = prepared.activity_label();
    approvals::activity(&session.id, format!("Running {description}"), None).await;
    if let Some(activity) = &activity {
        let _ = activity.running();
    }
    let session_id = session.id.clone();
    let evidence_scope = session.cwd.clone();
    let activity_label = description.clone();
    let completion = Arc::new(Mutex::new(JobCompletion {
        activity,
        ..JobCompletion::default()
    }));
    let task_completion = Arc::clone(&completion);
    let handle = tokio::spawn(async move {
        let (result, outcome) = tokio::select! {
            result = local_agent::run(prepared) => {
                let result = render_local_agent_result(result);
                let outcome = if result.is_ok() {
                    JobActivityOutcome::Completed
                } else {
                    JobActivityOutcome::Failed(JobActivityFailure::ChildFailed)
                };
                (result, outcome)
            }
            _ = session_stop => {
                (
                    Err(anyhow::anyhow!("session stopped; local agent job cancelled")),
                    JobActivityOutcome::Cancelled(ActivityCancellationReason::SessionStopped),
                )
            }
            _ = tokio::time::sleep(max_lifetime) => {
                (
                    Err(anyhow::anyhow!("local agent job exceeded the two-hour lifetime limit")),
                    JobActivityOutcome::Cancelled(ActivityCancellationReason::Timeout),
                )
            }
        };
        let cached = cache_job_result(&result, &session_id, &evidence_scope);
        finish_job_completion(&task_completion, cached, outcome);
        drop(slot);
        reap_jobs();
        report_local_agent_finished(session_id, activity_label, &result).await;
    });
    Ok((description, handle, completion))
}

async fn dev_tool_run(
    args: &Value,
    session: &config::Session,
    activity: Option<ActivityScope>,
) -> Result<Value> {
    dev_tool_run_with_executable(args, session, None, activity).await
}

async fn dev_tool_run_with_executable(
    args: &Value,
    session: &config::Session,
    executable: Option<&Path>,
    activity: Option<ActivityScope>,
) -> Result<Value> {
    let prepared = match executable {
        Some(executable) => dev_tool::prepare_with_executable(args, session, Some(executable))?,
        None => dev_tool::prepare(args, session)?,
    };
    let detail = prepared.approval_detail();
    approvals::ensure_approval_detail_fits(&detail)?;
    let approved = approvals::ensure_local_approval_with_activity(
        session,
        approvals::ApprovalClass::DeveloperTool,
        "dev_tool_run",
        detail,
        prepared.cwd().to_path_buf(),
        BTreeMap::new(),
        activity.as_ref(),
    )
    .await?;
    if !approved {
        if let Some(activity) = &activity {
            let _ = activity
                .fail_with_summary(ActivitySummary::failure(ActivityErrorKind::ApprovalDenied));
        }
        anyhow::bail!("user denied dev_tool_run");
    }

    let current_session = config::load_session(&session.id).await?;
    anyhow::ensure!(
        current_session.started_at == session.started_at
            && current_session.process_id == session.process_id,
        "session instance changed while developer-tool approval was pending"
    );
    prepared.revalidate(&current_session)?;
    let (description, mut handle, completion) =
        spawn_dev_tool(prepared, &current_session, activity).await?;
    match tokio::time::timeout(FOREGROUND_TIMEOUT, &mut handle).await {
        Ok(joined) => {
            joined.context("developer tool task failed")?;
            let result = completion
                .lock()
                .unwrap()
                .result
                .clone()
                .context("developer tool task completed without a cached result")?;
            cached_job_result(result, OutputPolicy::default())
        }
        Err(_) => {
            store_job(
                session,
                description,
                handle,
                completion,
                OutputPolicy::default(),
                "Backgrounded",
            )
            .await
        }
    }
}

async fn spawn_dev_tool(
    prepared: dev_tool::PreparedDevToolRun,
    session: &config::Session,
    activity: Option<ActivityScope>,
) -> Result<(String, JoinHandle<()>, Arc<Mutex<JobCompletion>>)> {
    spawn_dev_tool_with_controls(
        prepared,
        session,
        activity,
        wait_for_session_stop(session.id.clone()),
        MAX_JOB_LIFETIME,
    )
    .await
}

async fn spawn_dev_tool_with_controls<F>(
    prepared: dev_tool::PreparedDevToolRun,
    session: &config::Session,
    activity: Option<ActivityScope>,
    session_stop: F,
    max_lifetime: Duration,
) -> Result<(String, JoinHandle<()>, Arc<Mutex<JobCompletion>>)>
where
    F: Future<Output = ()> + Send + 'static,
{
    let slot = reserve_job_slot_for_cwd(&session.id, prepared.cwd()).await?;
    let description = prepared.activity_label();
    approvals::activity(&session.id, format!("Running {description}"), None).await;
    if let Some(activity) = &activity {
        let _ = activity.running();
    }
    let session_id = session.id.clone();
    let evidence_scope = session.cwd.clone();
    let activity_label = description.clone();
    let completion = Arc::new(Mutex::new(JobCompletion {
        activity,
        ..JobCompletion::default()
    }));
    let task_completion = Arc::clone(&completion);
    let handle = tokio::spawn(async move {
        let (result, outcome) = tokio::select! {
            result = dev_tool::run(prepared) => {
                let result = result.and_then(render_output);
                let outcome = if result.is_ok() {
                    JobActivityOutcome::Completed
                } else {
                    JobActivityOutcome::Failed(JobActivityFailure::ChildFailed)
                };
                (result, outcome)
            }
            _ = session_stop => {
                (
                    Err(anyhow::anyhow!("session stopped; developer tool job cancelled")),
                    JobActivityOutcome::Cancelled(ActivityCancellationReason::SessionStopped),
                )
            }
            _ = tokio::time::sleep(max_lifetime) => {
                (
                    Err(anyhow::anyhow!("developer tool job exceeded the two-hour lifetime limit")),
                    JobActivityOutcome::Cancelled(ActivityCancellationReason::Timeout),
                )
            }
        };
        let cached = cache_job_result(&result, &session_id, &evidence_scope);
        finish_job_completion(&task_completion, cached, outcome);
        drop(slot);
        reap_jobs();
        report_local_agent_finished(session_id, activity_label, &result).await;
    });
    Ok((description, handle, completion))
}

async fn spawn_sandboxed_command(
    args: &Value,
    session: &config::Session,
    activity: Option<ActivityScope>,
) -> Result<(String, PathBuf, JoinHandle<()>, Arc<Mutex<JobCompletion>>)> {
    spawn_sandboxed_command_with_controls(
        args,
        session,
        activity,
        wait_for_session_stop(session.id.clone()),
        MAX_JOB_LIFETIME,
    )
    .await
}

async fn spawn_sandboxed_command_with_controls<F>(
    args: &Value,
    session: &config::Session,
    activity: Option<ActivityScope>,
    session_stop: F,
    max_lifetime: Duration,
) -> Result<(String, PathBuf, JoinHandle<()>, Arc<Mutex<JobCompletion>>)>
where
    F: Future<Output = ()> + Send + 'static,
{
    let command = required_command(args)?;
    let cwd = cwd(args, session)?;
    let roots = session.permitted_directories.clone();
    let permission_mode = session.permission_mode;
    let slot = reserve_job_slot_for_cwd(&session.id, &cwd).await?;
    let rendered_command = render_command(&command);
    approvals::activity(&session.id, format!("Running {rendered_command}"), None).await;
    if let Some(activity) = &activity {
        let _ = activity.running();
    }
    let session_id = session.id.clone();
    let evidence_scope = session.cwd.clone();
    let task_command = rendered_command.clone();
    let task_cwd = cwd.clone();
    let completion = Arc::new(Mutex::new(JobCompletion {
        activity,
        ..JobCompletion::default()
    }));
    let task_completion = Arc::clone(&completion);
    let handle = tokio::spawn(async move {
        let (result, outcome) = tokio::select! {
            result = run_session_command(&command, &task_cwd, &roots, permission_mode) => {
                match result {
                    Ok(output) => {
                        let result = render_output(output);
                        let outcome = if result.is_ok() {
                            JobActivityOutcome::Completed
                        } else {
                            JobActivityOutcome::Failed(JobActivityFailure::ChildFailed)
                        };
                        (result, outcome)
                    }
                    Err(error) => (
                        Err(error),
                        JobActivityOutcome::Failed(JobActivityFailure::SandboxSetupFailed),
                    ),
                }
            }
            _ = session_stop => {
                (
                    Err(anyhow::anyhow!("session stopped; sandbox job cancelled")),
                    JobActivityOutcome::Cancelled(ActivityCancellationReason::SessionStopped),
                )
            }
            _ = tokio::time::sleep(max_lifetime) => {
                (
                    Err(anyhow::anyhow!("sandbox job exceeded the two-hour lifetime limit")),
                    JobActivityOutcome::Cancelled(ActivityCancellationReason::Timeout),
                )
            }
        };
        let cached = cache_job_result(&result, &session_id, &evidence_scope);
        finish_job_completion(&task_completion, cached, outcome);
        drop(slot);
        reap_jobs();
        report_command_finished(session_id, "execute", &task_command, &result).await;
    });
    Ok((rendered_command, cwd, handle, completion))
}

fn finish_job_completion(
    completion: &Arc<Mutex<JobCompletion>>,
    result: CachedJobResult,
    outcome: JobActivityOutcome,
) -> bool {
    let mut completion = completion.lock().unwrap();
    finish_job_completion_locked(&mut completion, result, outcome)
}

fn finish_job_completion_locked(
    completion: &mut JobCompletion,
    result: CachedJobResult,
    outcome: JobActivityOutcome,
) -> bool {
    if completion.result.is_some() || completion.activity_terminal {
        return false;
    }

    completion.result = Some(result);
    completion.completed_at = Some(Instant::now());
    completion.activity_terminal = true;
    finish_job_activity(completion.activity.as_ref(), outcome);
    true
}

fn cancel_pending_job_activity(
    completion: &Arc<Mutex<JobCompletion>>,
    reason: ActivityCancellationReason,
) -> bool {
    let mut completion = completion.lock().unwrap();
    cancel_pending_job_activity_locked(&mut completion, reason)
}

fn cancel_pending_job_activity_locked(
    completion: &mut JobCompletion,
    reason: ActivityCancellationReason,
) -> bool {
    if completion.result.is_some() || completion.activity_terminal {
        return false;
    }

    completion.activity_terminal = true;
    finish_job_activity(
        completion.activity.as_ref(),
        JobActivityOutcome::Cancelled(reason),
    );
    true
}

fn finish_job_activity(activity: Option<&ActivityScope>, outcome: JobActivityOutcome) {
    let Some(activity) = activity else {
        return;
    };
    let _ = match outcome {
        JobActivityOutcome::Completed => activity.complete(),
        JobActivityOutcome::Failed(failure) => {
            activity.fail_with_summary(ActivitySummary::failure(failure.error_kind()))
        }
        JobActivityOutcome::Cancelled(reason) => {
            activity.cancel_with_summary(ActivitySummary::cancellation(reason))
        }
    };
}

async fn run_session_command(
    command: &[String],
    cwd: &Path,
    roots: &[PathBuf],
    permission_mode: config::PermissionMode,
) -> Result<sandbox::Output> {
    match permission_mode.command_network_policy() {
        Some(network) => sandbox::run_with_network_policy(command, cwd, roots, network, None).await,
        None => sandbox::run_unrestricted(command, cwd, None).await,
    }
}

async fn store_job(
    session: &config::Session,
    rendered_command: String,
    handle: JoinHandle<()>,
    completion: Arc<Mutex<JobCompletion>>,
    output_policy: OutputPolicy,
    activity: &str,
) -> Result<Value> {
    let job_id = Uuid::new_v4();
    {
        let mut state = jobs().lock().unwrap();
        state.jobs.insert(
            job_id,
            Job {
                session_id: session.id.clone(),
                command: rendered_command.clone(),
                handle,
                completion,
                output_policy,
            },
        );
        reap_jobs_at(&mut state, Instant::now());
    }
    approvals::activity(
        &session.id,
        format!("{activity} {rendered_command}"),
        Some(format!("└ job {job_id}")),
    )
    .await;
    text_result(json!({"status":"running","job_id":job_id}).to_string())
}

fn cache_job_result(result: &Result<String>, session_id: &str, scope: &Path) -> CachedJobResult {
    match result {
        Ok(text) => CachedJobResult::Success {
            evidence: cache_command_evidence(session_id, scope, text),
            text: text.clone(),
        },
        Err(error) => {
            let text = format!("{error:#}");
            CachedJobResult::Error {
                evidence: cache_command_evidence(session_id, scope, &text),
                text,
            }
        }
    }
}

fn cache_command_evidence(
    session_id: &str,
    scope: &Path,
    text: &str,
) -> Option<evidence::EvidenceRef> {
    let value: Value = serde_json::from_str(text).ok()?;
    value.get("exit_code").and_then(Value::as_i64)?;
    evidence::store(session_id, scope, text.to_owned())
        .ok()
        .flatten()
}

fn cached_job_result(result: CachedJobResult, policy: OutputPolicy) -> Result<Value> {
    match result {
        CachedJobResult::Success { text, evidence } => {
            text_result(apply_output_policy(&text, policy, evidence.as_ref()))
        }
        CachedJobResult::Error { text, evidence } => {
            anyhow::bail!(apply_output_policy(&text, policy, evidence.as_ref()))
        }
    }
}

fn apply_output_policy(
    text: &str,
    policy: OutputPolicy,
    evidence_ref: Option<&evidence::EvidenceRef>,
) -> String {
    if policy == OutputPolicy::default() {
        return text.to_owned();
    }
    let Ok(mut value) = serde_json::from_str::<Value>(text) else {
        return text.to_owned();
    };
    let Some(object) = value.as_object_mut() else {
        return text.to_owned();
    };
    if object.get("exit_code").and_then(Value::as_i64).is_none() {
        return text.to_owned();
    }

    let mut omitted = false;
    let mut returned_truncated = false;
    if policy.status_only {
        omitted = object
            .get("stdout")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
            || object
                .get("stderr")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty());
        object.remove("stdout");
        object.remove("stderr");
        object.insert("output_omitted".to_owned(), Value::Bool(omitted));
    } else if let Some(limit) = policy.output_limit_bytes {
        let stdout = object
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let stderr = object
            .get("stderr")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (stdout, stdout_truncated, remaining) = truncate_utf8(stdout, limit);
        let (stderr, stderr_truncated, _) = truncate_utf8(stderr, remaining);
        returned_truncated = stdout_truncated || stderr_truncated;
        object.insert("stdout".to_owned(), Value::String(stdout));
        object.insert("stderr".to_owned(), Value::String(stderr));
        object.insert(
            "returned_truncated".to_owned(),
            Value::Bool(returned_truncated),
        );
        object.insert("output_limit_bytes".to_owned(), Value::Number(limit.into()));
    }
    if (omitted || returned_truncated)
        && let Some(reference) = evidence_ref
    {
        object.insert(
            "evidence".to_owned(),
            serde_json::to_value(reference).unwrap_or(Value::Null),
        );
    }
    value.to_string()
}

fn truncate_utf8(text: &str, limit: usize) -> (String, bool, usize) {
    if text.len() <= limit {
        return (text.to_owned(), false, limit - text.len());
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true, limit.saturating_sub(end))
}

fn parse_output_policy(args: &Value) -> Result<OutputPolicy> {
    Ok(parse_output_policy_override(args)?.unwrap_or_default())
}

fn parse_output_policy_override(args: &Value) -> Result<Option<OutputPolicy>> {
    let has_limit = args.get("output_limit_bytes").is_some();
    let has_status = args.get("status_only").is_some();
    if !has_limit && !has_status {
        return Ok(None);
    }
    let output_limit_bytes = args
        .get("output_limit_bytes")
        .map(|value| {
            let value = value
                .as_u64()
                .context("output_limit_bytes must be an integer")?;
            anyhow::ensure!(
                (MIN_RETURN_OUTPUT_BYTES as u64..=MAX_RETURN_OUTPUT_BYTES as u64).contains(&value),
                "output_limit_bytes must be {MIN_RETURN_OUTPUT_BYTES}..={MAX_RETURN_OUTPUT_BYTES}"
            );
            Ok(value as usize)
        })
        .transpose()?;
    let status_only = args
        .get("status_only")
        .map(|value| value.as_bool().context("status_only must be a boolean"))
        .transpose()?
        .unwrap_or(false);
    anyhow::ensure!(
        !(status_only && output_limit_bytes.is_some()),
        "status_only and output_limit_bytes cannot be combined"
    );
    Ok(Some(OutputPolicy {
        output_limit_bytes,
        status_only,
    }))
}

#[cfg(test)]
fn reserve_job_slot(session_id: &str) -> Result<JobSlot> {
    reserve_job_slot_with_admission(session_id, Path::new("."), None, None, None)
}

async fn reserve_job_slot_for_cwd(session_id: &str, cwd: &Path) -> Result<JobSlot> {
    let configured_src_root = managed_worktree::configured_src_root_from_env();
    let managed_worktree::WorktreeAdmission {
        cwd,
        worktree_root,
        repository_reservation,
        reservation,
    } = managed_worktree::acquire_worktree_admission(cwd, configured_src_root.as_deref()).await?;
    reserve_job_slot_with_admission(
        session_id,
        &cwd,
        worktree_root,
        repository_reservation,
        reservation,
    )
}

fn reserve_job_slot_with_admission(
    session_id: &str,
    cwd: &Path,
    worktree_root: Option<PathBuf>,
    repository_reservation: Option<managed_worktree::RepositoryReservation>,
    reservation: Option<managed_worktree::WorktreeReservation>,
) -> Result<JobSlot> {
    let mut state = jobs().lock().unwrap();
    let active = state
        .active_by_session
        .entry(session_id.to_owned())
        .or_default();
    anyhow::ensure!(
        *active < MAX_ACTIVE_JOBS_PER_SESSION,
        "session {session_id} already has {MAX_ACTIVE_JOBS_PER_SESSION} active sandbox jobs"
    );
    let admission_id = Uuid::new_v4();
    *active += 1;
    state.active_admissions.insert(
        admission_id,
        ActiveJobAdmission {
            cwd: cwd.to_path_buf(),
            worktree_root,
        },
    );
    // The active registry is now visible while the lifecycle reservation is
    // still held.  Keep the shared reservation for the entire job lifetime:
    // cleanup in another Temote process cannot observe this process-local
    // registry, but it will still fail closed on the cross-process lock.
    drop(repository_reservation);
    Ok(JobSlot {
        session_id: session_id.to_owned(),
        admission_id,
        _reservation: reservation,
    })
}

fn release_job_slot(session_id: &str, admission_id: Uuid) {
    let mut state = jobs().lock().unwrap();
    state.active_admissions.remove(&admission_id);
    if let Some(active) = state.active_by_session.get_mut(session_id) {
        *active = active.saturating_sub(1);
        if *active == 0 {
            state.active_by_session.remove(session_id);
        }
    }
}

/// Snapshot all active operation admissions, including foreground tasks that
/// have not yet been inserted into the completed/background `jobs` cache.
/// The caller supplies this snapshot to the same pure ownership predicate used
/// by deterministic remove/prune tests.
fn snapshot_active_job_ownerships() -> Vec<(String, PathBuf)> {
    let state = jobs().lock().unwrap();
    let mut owners = state
        .active_admissions
        .iter()
        .map(|(admission_id, admission)| {
            let owner_path = admission
                .worktree_root
                .clone()
                .unwrap_or_else(|| admission.cwd.clone());
            (admission_id.to_string(), owner_path)
        })
        .collect::<Vec<_>>();
    owners.sort_by(|left, right| left.0.cmp(&right.0));
    owners
}

#[cfg(test)]
fn remove_job(job_id: Uuid) -> Option<Job> {
    jobs().lock().unwrap().jobs.remove(&job_id)
}

fn reap_jobs() {
    let mut state = jobs().lock().unwrap();
    reap_jobs_at(&mut state, Instant::now());
}

fn reap_jobs_at(state: &mut JobState, now: Instant) {
    reap_jobs_with_limits(
        state,
        now,
        MAX_COMPLETED_JOBS_PER_SESSION,
        MAX_COMPLETED_JOBS_TOTAL,
    );
}

fn reap_jobs_with_limits(
    state: &mut JobState,
    now: Instant,
    per_session_limit: usize,
    total_limit: usize,
) {
    let completed = state
        .jobs
        .iter()
        .filter_map(|(job_id, job)| {
            let completed_at = job.completion.lock().unwrap().completed_at?;
            Some((*job_id, job.session_id.clone(), completed_at))
        })
        .collect::<Vec<_>>();

    let mut remove = completed
        .iter()
        .filter_map(|(job_id, _, completed_at)| {
            (now.saturating_duration_since(*completed_at) >= COMPLETED_JOB_TTL).then_some(*job_id)
        })
        .collect::<std::collections::HashSet<_>>();

    let mut by_session = HashMap::<String, Vec<(Uuid, Instant)>>::new();
    for (job_id, session_id, completed_at) in &completed {
        if !remove.contains(job_id) {
            by_session
                .entry(session_id.clone())
                .or_default()
                .push((*job_id, *completed_at));
        }
    }
    for entries in by_session.values_mut() {
        if entries.len() <= per_session_limit {
            continue;
        }
        entries.sort_by_key(|(job_id, completed_at)| (*completed_at, *job_id));
        for (job_id, _) in entries.iter().take(entries.len() - per_session_limit) {
            remove.insert(*job_id);
        }
    }

    let mut remaining = completed
        .iter()
        .filter(|(job_id, _, _)| !remove.contains(job_id))
        .map(|(job_id, _, completed_at)| (*job_id, *completed_at))
        .collect::<Vec<_>>();
    if remaining.len() > total_limit {
        remaining.sort_by_key(|(job_id, completed_at)| (*completed_at, *job_id));
        for (job_id, _) in remaining.iter().take(remaining.len() - total_limit) {
            remove.insert(*job_id);
        }
    }

    for job_id in remove {
        state.jobs.remove(&job_id);
    }
}

async fn wait_for_session_stop(session_id: String) {
    loop {
        if let Ok(false) = config::session_is_active(&session_id).await {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

enum JobPollSnapshot {
    Completed(CachedJobResult, OutputPolicy),
    Running(OutputPolicy),
    FinishedWithoutResult,
}

fn inspect_job(job_id: Uuid, session_id: &str) -> Result<JobPollSnapshot> {
    let state = jobs().lock().unwrap();
    let job = state.jobs.get(&job_id).context("unknown job_id")?;
    anyhow::ensure!(
        job.session_id == session_id,
        "job does not belong to this session"
    );
    if let Some(result) = job.completion.lock().unwrap().result.clone() {
        return Ok(JobPollSnapshot::Completed(result, job.output_policy));
    }
    if job.handle.is_finished() {
        Ok(JobPollSnapshot::FinishedWithoutResult)
    } else {
        Ok(JobPollSnapshot::Running(job.output_policy))
    }
}

pub(crate) fn snapshot_jobs_for_session(session_id: &str, limit: usize) -> JobListSnapshot {
    let state = jobs().lock().unwrap();
    let mut summaries = state
        .jobs
        .iter()
        .filter(|(_, job)| job.session_id == session_id)
        .map(|(job_id, job)| {
            let completion = job.completion.lock().unwrap();
            let status = match completion.result.as_ref() {
                Some(CachedJobResult::Success { .. }) => "completed",
                Some(CachedJobResult::Error { .. }) => "failed",
                None if job.handle.is_finished() => "unknown",
                None => "running",
            };
            JobSummary {
                job_id: job_id.to_string(),
                status: status.to_owned(),
            }
        })
        .collect::<Vec<_>>();
    summaries.sort_by(|left, right| {
        let left_rank = usize::from(left.status != "running");
        let right_rank = usize::from(right.status != "running");
        left_rank
            .cmp(&right_rank)
            .then_with(|| left.job_id.cmp(&right.job_id))
    });
    let truncated = summaries.len() > limit;
    summaries.truncate(limit);
    JobListSnapshot {
        jobs: summaries,
        truncated,
    }
}

fn take_job_for_session(job_id: Uuid, session_id: &str) -> Result<Job> {
    let mut state = jobs().lock().unwrap();
    let job = state.jobs.get(&job_id).context("unknown job_id")?;
    anyhow::ensure!(
        job.session_id == session_id,
        "job does not belong to this session"
    );
    state.jobs.remove(&job_id).context("unknown job_id")
}

fn save_checkpoint_after_approval(
    session: &config::Session,
    request: checkpoints::SaveRequest,
    store: &checkpoints::Store,
    approved: bool,
) -> Result<(checkpoints::CheckpointEnvelope, String)> {
    anyhow::ensure!(request.session_id == session.id, "session ID mismatch");
    if !approved {
        anyhow::bail!("user denied checkpoint save")
    }
    let activity_detail = checkpoints::activity_detail(&request.checkpoint);
    let saved = store.save_idempotent(
        session,
        request.operation_id,
        request.checkpoint_id,
        request.expected_revision,
        request.checkpoint,
    )?;
    Ok((saved, activity_detail))
}

fn job_list(args: &Value, session: &config::Session) -> Result<Value> {
    let object = args
        .as_object()
        .context("job_list arguments must be an object")?;
    anyhow::ensure!(
        object
            .keys()
            .all(|key| matches!(key.as_str(), "session_id" | "limit")),
        "job_list accepts only session_id and limit"
    );
    let limit = match object.get("limit") {
        Some(value) => {
            let limit = value
                .as_u64()
                .context("job_list limit must be an integer")?;
            anyhow::ensure!((1..=128).contains(&limit), "job_list limit must be 1..=128");
            limit as usize
        }
        None => 50,
    };
    let snapshot = snapshot_jobs_for_session(&session.id, limit);
    text_result(serde_json::to_string_pretty(&json!({
        "jobs": snapshot.jobs,
        "truncated": snapshot.truncated,
        "retention": "in_memory"
    }))?)
}

async fn poll_job(args: &Value, session: &config::Session) -> Result<Value> {
    let job_id = required_job_id(args)?;
    let override_policy = parse_output_policy_override(args)?;
    match inspect_job(job_id, &session.id)? {
        JobPollSnapshot::Completed(result, stored_policy) => {
            cached_job_result(result, override_policy.unwrap_or(stored_policy))
        }
        JobPollSnapshot::Running(stored_policy) => {
            let policy = override_policy.unwrap_or(stored_policy);
            let response = if policy == OutputPolicy::default() {
                json!({"status":"running","job_id":job_id})
            } else {
                json!({
                    "status":"running",
                    "job_id":job_id,
                    "status_only": policy.status_only,
                    "output_limit_bytes": policy.output_limit_bytes
                })
            };
            text_result(response.to_string())
        }
        JobPollSnapshot::FinishedWithoutResult => {
            anyhow::bail!("background command task finished without a cached result")
        }
    }
}

#[cfg(test)]
async fn stop_job(args: &Value, session: &config::Session) -> Result<Value> {
    stop_job_with_activity(args, session, None).await
}

async fn stop_job_with_activity(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let job_id = required_job_id(args)?;
    let job = take_job_for_session(job_id, &session.id)?;
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    cancel_pending_job_activity(&job.completion, ActivityCancellationReason::StopRequested);
    job.handle.abort();
    let _ = job.handle.await;
    approvals::activity(
        &session.id,
        format!("Stopped {}", job.command),
        Some(format!("└ job {job_id}")),
    )
    .await;
    text_result(json!({"status":"stopped","job_id":job_id}).to_string())
}

fn required_job_id(args: &Value) -> Result<Uuid> {
    let value = args
        .get("job_id")
        .and_then(Value::as_str)
        .context("missing job_id")?;
    Uuid::parse_str(value).context("invalid job_id")
}

fn required_command(args: &Value) -> Result<Vec<String>> {
    let command = args
        .get("command")
        .and_then(Value::as_array)
        .context("missing command")?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .context("command entries must be strings")
        })
        .collect::<Result<Vec<_>>>()?;
    validate_command_budget(&command)?;
    Ok(command)
}

fn validate_command_budget(command: &[String]) -> Result<()> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    anyhow::ensure!(
        !command[0].is_empty(),
        "command executable must not be empty"
    );
    anyhow::ensure!(
        command.len() <= MAX_COMMAND_ARGUMENTS,
        "command must contain at most {MAX_COMMAND_ARGUMENTS} arguments"
    );
    let mut total = 0usize;
    for argument in command {
        anyhow::ensure!(
            !argument.contains('\0'),
            "command arguments must not contain NUL bytes"
        );
        anyhow::ensure!(
            argument.len() <= MAX_COMMAND_ARGUMENT_BYTES,
            "command argument exceeds {MAX_COMMAND_ARGUMENT_BYTES} bytes"
        );
        total = total
            .checked_add(argument.len())
            .context("command argument size overflow")?;
        anyhow::ensure!(
            total <= MAX_COMMAND_TOTAL_BYTES,
            "command arguments exceed {MAX_COMMAND_TOTAL_BYTES} bytes in total"
        );
    }
    Ok(())
}

async fn without_sandbox(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let command = required_command(args)?;
    let cwd = cwd(args, session)?;
    request_activity_approval(
        session,
        ActivityApprovalRequest {
            class: approvals::ApprovalClass::HostUnrestricted,
            operation: "without_sandbox",
            detail: format!("argv: {command:?}"),
            cwd: cwd.clone(),
            metadata: BTreeMap::new(),
            denial: "user denied without_sandbox",
        },
        activity,
    )
    .await?;
    run_and_report(session.id.clone(), command, cwd, true, &[]).await
}

async fn run_and_report(
    session_id: String,
    command: Vec<String>,
    cwd: PathBuf,
    unrestricted: bool,
    roots: &[PathBuf],
) -> Result<Value> {
    let rendered_command = render_command(&command);
    approvals::activity(&session_id, format!("Running {rendered_command}"), None).await;
    let output = if unrestricted {
        sandbox::run_unrestricted(&command, &cwd, None).await
    } else {
        sandbox::run(&command, &cwd, roots, None).await
    };
    let result = output.and_then(render_output);
    report_command_finished(session_id, "execute", &rendered_command, &result).await;
    text_result(result?)
}

fn safe_child_call_summary(tool_name: &str, arguments: &Value) -> String {
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

fn safe_kintone_cli_summary(arguments: &[String], stdout_path: Option<&Path>) -> String {
    let command = match arguments.get(0..2) {
        Some([group, action])
            if matches!(
                (group.as_str(), action.as_str()),
                ("record", "export")
                    | ("record", "import")
                    | ("record", "delete")
                    | ("customize", "export")
                    | ("customize", "apply")
                    | ("plugin", "upload")
            ) =>
        {
            format!("{group} {action}")
        }
        _ => "(unvalidated)".to_owned(),
    };
    let mut option_names = arguments
        .iter()
        .skip(2)
        .filter(|argument| argument.starts_with('-'))
        .map(|argument| {
            argument
                .split_once('=')
                .map_or(argument.as_str(), |(name, _)| name)
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    option_names.sort();
    option_names.dedup();
    format!(
        "command: {command}\noption names: {}\nstdout file: {}",
        if option_names.is_empty() {
            "(none)".to_owned()
        } else {
            option_names.join(", ")
        },
        if stdout_path.is_some() {
            "configured"
        } else {
            "capture"
        }
    )
}

fn service_account_approval_detail(
    command: &[String],
    env_files: &[PathBuf],
    environment: &std::collections::BTreeMap<String, String>,
    allowed_locators: &[String],
) -> Result<String> {
    let locator_scope = if allowed_locators.is_empty() {
        "(none)".to_owned()
    } else {
        allowed_locators
            .iter()
            .map(|locator| format!("- {locator}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let detail = format!(
        "argv: {}\nenv files: {}\nsecret env names: {}\nnested resolver locators:\n{}",
        render_command(command),
        if env_files.is_empty() {
            "(none)".to_owned()
        } else {
            env_files
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        },
        if environment.is_empty() {
            "(none)".to_owned()
        } else {
            environment.keys().cloned().collect::<Vec<_>>().join(", ")
        },
        locator_scope
    );
    if !allowed_locators.is_empty() {
        approvals::ensure_approval_detail_fits(&detail)?;
    }
    Ok(detail)
}

fn render_command(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| shell_word(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn report_command_finished(
    session_id: String,
    operation_class: &str,
    command: &str,
    result: &Result<String>,
) {
    let detail = match result {
        Ok(text) => command_summary(text),
        Err(error) => Some(format!("└ Error: {error:#}")),
    };
    if result.is_err() {
        let kind = if operation_class == "git" {
            friction::FrictionKind::GitOperationFailed
        } else {
            friction::FrictionKind::ExecuteFailed
        };
        friction::record_observed_for_session_id(
            &session_id,
            kind,
            Some(operation_class),
            None,
            friction::EventOutcome::Failed,
        )
        .await;
    }
    approvals::activity(&session_id, format!("Ran {command}"), detail).await;
}

async fn report_local_agent_finished(
    session_id: String,
    activity_label: String,
    result: &Result<String>,
) {
    if result.is_err() {
        friction::record_observed_for_session_id(
            &session_id,
            friction::FrictionKind::ExecuteFailed,
            Some("local_agent_run"),
            None,
            friction::EventOutcome::Failed,
        )
        .await;
    }
    let status = if result.is_ok() {
        "completed"
    } else {
        "failed"
    };
    approvals::activity(
        &session_id,
        format!("{activity_label} status={status}"),
        None,
    )
    .await;
}

fn shell_word(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=+".contains(c))
    {
        value.to_owned()
    } else {
        format!("{:?}", value)
    }
}

fn command_summary(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    let stdout = value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    let stderr = value
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    let output = if stdout.is_empty() { stderr } else { stdout };
    if output.is_empty() {
        None
    } else {
        Some(
            output
                .lines()
                .map(|line| format!("└ {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

fn render_diff(old: &str, new: &str) -> (usize, usize, String) {
    crate::line_diff::render_diff(old, new)
}

fn render_output(output: sandbox::Output) -> Result<String> {
    let text = json!({
        "exit_code": output.status,
        "stdout": output.stdout,
        "stderr": output.stderr,
        "truncated": output.truncated
    })
    .to_string();
    if output.status == 0 {
        Ok(text)
    } else {
        anyhow::bail!(text)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalAgentFailureClass {
    RunnerSpawnFailed,
    SandboxSetupFailed,
    AgentChildProcessDenied,
    AgentNonzeroExit,
}

impl LocalAgentFailureClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::RunnerSpawnFailed => "runner_spawn_failed",
            Self::SandboxSetupFailed => "sandbox_setup_failed",
            Self::AgentChildProcessDenied => "agent_child_process_denied",
            Self::AgentNonzeroExit => "agent_nonzero_exit",
        }
    }
}

fn local_agent_output_failure_class(output: &sandbox::Output) -> Option<LocalAgentFailureClass> {
    if output.status == 0 {
        return None;
    }
    let child_spawn_denied = [
        "EPERM : failed to spawn process",
        "Failed to create unified exec process: Operation not permitted",
    ]
    .iter()
    .any(|sentinel| output.stderr.contains(sentinel) || output.stdout.contains(sentinel));
    Some(if child_spawn_denied {
        LocalAgentFailureClass::AgentChildProcessDenied
    } else {
        LocalAgentFailureClass::AgentNonzeroExit
    })
}

fn render_local_agent_result(result: Result<sandbox::Output>) -> Result<String> {
    match result {
        Ok(output) => {
            let failure_class = local_agent_output_failure_class(&output);
            let text = json!({
                "exit_code": output.status,
                "stdout": output.stdout,
                "stderr": output.stderr,
                "truncated": output.truncated,
                "failure_class": failure_class.map(LocalAgentFailureClass::as_str),
            })
            .to_string();
            if output.status == 0 {
                Ok(text)
            } else {
                anyhow::bail!(text)
            }
        }
        Err(error) => {
            let failure_class = if sandbox::is_local_agent_spawn_error(&error) {
                LocalAgentFailureClass::RunnerSpawnFailed
            } else {
                LocalAgentFailureClass::SandboxSetupFailed
            };
            anyhow::bail!(
                json!({
                    "failure_class": failure_class.as_str(),
                    "error": "local agent failed before a child result was available",
                })
                .to_string()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use temote_mcp::activity::contract::{ActivityState, ActivityUpdate};
    use temote_mcp::activity::scope::{ActivityEmitError, ActivityEmitter};

    #[derive(Clone, Default)]
    struct RecordingActivityEmitter {
        updates: Arc<Mutex<Vec<ActivityUpdate>>>,
    }

    impl RecordingActivityEmitter {
        fn updates(&self) -> Vec<ActivityUpdate> {
            self.updates.lock().unwrap().clone()
        }

        fn states(&self) -> Vec<ActivityState> {
            self.updates()
                .into_iter()
                .map(|update| update.state())
                .collect()
        }
    }

    impl ActivityEmitter for RecordingActivityEmitter {
        fn try_emit(&self, update: ActivityUpdate) -> Result<(), ActivityEmitError> {
            self.updates.lock().unwrap().push(update);
            Ok(())
        }
    }

    fn cached_success(text: impl Into<String>) -> CachedJobResult {
        CachedJobResult::Success {
            text: text.into(),
            evidence: None,
        }
    }

    fn cached_error(text: impl Into<String>) -> CachedJobResult {
        CachedJobResult::Error {
            text: text.into(),
            evidence: None,
        }
    }

    #[test]
    fn local_agent_failure_classifies_runner_child_and_generic_failures() {
        let opencode = sandbox::Output {
            status: 1,
            stdout: String::new(),
            stderr: "EPERM : failed to spawn process".to_owned(),
            truncated: false,
        };
        assert_eq!(
            local_agent_output_failure_class(&opencode),
            Some(LocalAgentFailureClass::AgentChildProcessDenied)
        );

        let codex = sandbox::Output {
            status: 1,
            stdout: "Failed to create unified exec process: Operation not permitted (os error 1)"
                .to_owned(),
            stderr: String::new(),
            truncated: false,
        };
        assert_eq!(
            local_agent_output_failure_class(&codex),
            Some(LocalAgentFailureClass::AgentChildProcessDenied)
        );

        let generic = sandbox::Output {
            status: 2,
            stdout: String::new(),
            stderr: "provider rejected request".to_owned(),
            truncated: false,
        };
        assert_eq!(
            local_agent_output_failure_class(&generic),
            Some(LocalAgentFailureClass::AgentNonzeroExit)
        );

        let rendered =
            render_local_agent_result(Err(anyhow::anyhow!("invalid sandbox root"))).unwrap_err();
        let value: Value = serde_json::from_str(&rendered.to_string()).unwrap();
        assert_eq!(value["failure_class"], "sandbox_setup_failed");
    }

    #[test]
    fn local_agent_failure_class_is_null_on_success() {
        let rendered = render_local_agent_result(Ok(sandbox::Output {
            status: 0,
            stdout: "ok".to_owned(),
            stderr: String::new(),
            truncated: false,
        }))
        .unwrap();
        let value: Value = serde_json::from_str(&rendered).unwrap();
        assert!(value["failure_class"].is_null());
    }

    fn activity_job_session(cwd: &Path) -> config::Session {
        config::Session {
            id: format!("activity-job-{}", Uuid::new_v4()),
            cwd: cwd.to_path_buf(),
            permitted_directories: vec![cwd.to_path_buf()],
            started_at: 1,
            process_id: std::process::id(),
            permission_mode: config::PermissionMode::Yolo,
        }
    }

    fn activity_job_scope(
        operation: ActivityOperation,
    ) -> (ActivityScope, RecordingActivityEmitter) {
        let emitter = RecordingActivityEmitter::default();
        let scope = ActivityScope::new(operation, emitter.clone());
        (scope, emitter)
    }

    fn activity_job_terminal_updates(emitter: &RecordingActivityEmitter) -> Vec<ActivityUpdate> {
        emitter
            .updates()
            .into_iter()
            .filter(|update| {
                matches!(
                    update.state(),
                    ActivityState::Completed | ActivityState::Failed | ActivityState::Cancelled
                )
            })
            .collect()
    }

    #[test]
    fn activity_coverage_classifies_every_advertised_session_tool_once() {
        let advertised_tools = tools(false, true);
        let advertised = advertised_tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let covered = ACTIVITY_TOOL_COVERAGE
            .iter()
            .map(|coverage| coverage.name)
            .chain(
                ACTIVITY_NON_DISPATCH_COVERAGE
                    .iter()
                    .map(|coverage| coverage.name),
            )
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            ACTIVITY_TOOL_COVERAGE.len() + ACTIVITY_NON_DISPATCH_COVERAGE.len(),
            covered.len()
        );
        assert_eq!(advertised, covered);
        assert!(
            ACTIVITY_TOOL_COVERAGE
                .iter()
                .all(|coverage| !coverage.fixture.is_empty())
        );
        assert!(
            ACTIVITY_NON_DISPATCH_COVERAGE
                .iter()
                .all(|coverage| !coverage.fixture.is_empty())
        );
        assert_eq!(
            ACTIVITY_NON_DISPATCH_COVERAGE
                .iter()
                .filter(|coverage| coverage.owner == ActivityNonDispatchOwner::Supervisor)
                .map(|coverage| coverage.name)
                .collect::<std::collections::BTreeSet<_>>(),
            ["session_restart", "session_start", "session_stop"]
                .into_iter()
                .collect()
        );
        assert_eq!(
            ACTIVITY_NON_DISPATCH_COVERAGE
                .iter()
                .filter(|coverage| coverage.owner == ActivityNonDispatchOwner::Excluded)
                .map(|coverage| coverage.name)
                .collect::<std::collections::BTreeSet<_>>(),
            ["session_info", "session_list"].into_iter().collect()
        );
        assert_eq!(
            ACTIVITY_TOOL_COVERAGE
                .iter()
                .filter(|coverage| coverage.owner == ActivityOwner::JobWorker)
                .map(|coverage| coverage.name)
                .collect::<std::collections::BTreeSet<_>>(),
            [
                "dev_tool_run",
                "execute",
                "local_agent_run",
                "start_command",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            ACTIVITY_TOOL_COVERAGE
                .iter()
                .filter(|coverage| coverage.success == ActivitySuccess::Accepted)
                .map(|coverage| coverage.name)
                .collect::<std::collections::BTreeSet<_>>(),
            ["codex_task_control", "codex_task_start"]
                .into_iter()
                .collect()
        );

        for coverage in ACTIVITY_TOOL_COVERAGE {
            let (scope, emitter) = activity_job_scope(coverage.operation);
            finish_covered_tool_activity(
                Some(coverage),
                Some(&scope),
                &text_result("fixture result".to_owned()),
            );
            let updates = emitter.updates();
            assert!(
                updates
                    .iter()
                    .all(|update| update.operation() == coverage.operation),
                "operation mismatch for {}",
                coverage.name
            );
            match coverage.owner {
                ActivityOwner::McpCall => {
                    assert_eq!(
                        updates
                            .iter()
                            .map(ActivityUpdate::state)
                            .collect::<Vec<_>>(),
                        vec![ActivityState::Started, ActivityState::Completed],
                        "terminal mismatch for {}",
                        coverage.name
                    );
                    let expected = match coverage.success {
                        ActivitySuccess::Completed => ActivitySummary::empty(),
                        ActivitySuccess::Accepted => {
                            ActivitySummary::result(ActivityResult::Accepted)
                        }
                    };
                    assert_eq!(updates.last().unwrap().summary(), &expected);
                }
                ActivityOwner::JobWorker => assert_eq!(
                    updates
                        .iter()
                        .map(ActivityUpdate::state)
                        .collect::<Vec<_>>(),
                    vec![ActivityState::Started],
                    "dispatcher finalized worker-owned {}",
                    coverage.name
                ),
            }
        }
    }

    #[test]
    fn activity_coverage_finalizes_call_worker_accepted_and_failure_paths() {
        let (completed_scope, completed_emitter) =
            activity_job_scope(ActivityOperation::EvidenceRead);
        finish_covered_tool_activity(
            activity_tool_coverage("evidence_read"),
            Some(&completed_scope),
            &text_result("ok".to_owned()),
        );
        assert_eq!(
            completed_emitter.states(),
            vec![ActivityState::Started, ActivityState::Completed]
        );

        let (accepted_scope, accepted_emitter) =
            activity_job_scope(ActivityOperation::CodexTaskStart);
        finish_covered_tool_activity(
            activity_tool_coverage("codex_task_start"),
            Some(&accepted_scope),
            &text_result("accepted".to_owned()),
        );
        let accepted = accepted_emitter.updates();
        assert_eq!(
            accepted
                .iter()
                .map(ActivityUpdate::state)
                .collect::<Vec<_>>(),
            vec![ActivityState::Started, ActivityState::Completed]
        );
        assert_eq!(
            accepted.last().unwrap().summary(),
            &ActivitySummary::result(ActivityResult::Accepted)
        );

        let (worker_scope, worker_emitter) = activity_job_scope(ActivityOperation::Execute);
        finish_covered_tool_activity(
            activity_tool_coverage("execute"),
            Some(&worker_scope),
            &text_result("backgrounded".to_owned()),
        );
        assert_eq!(worker_emitter.states(), vec![ActivityState::Started]);

        let (failure_scope, failure_emitter) =
            activity_job_scope(ActivityOperation::KintoneMcpStatus);
        let failed: Result<Value> = Err(anyhow::anyhow!("raw-secret-sentinel"));
        finish_covered_tool_activity(
            activity_tool_coverage("kintone_mcp_status"),
            Some(&failure_scope),
            &failed,
        );
        let failed = failure_emitter.updates();
        assert_eq!(
            failed.iter().map(ActivityUpdate::state).collect::<Vec<_>>(),
            vec![ActivityState::Started, ActivityState::Failed]
        );
        assert_eq!(
            failed.last().unwrap().summary(),
            &ActivitySummary::failure(ActivityErrorKind::OperationFailed)
        );
        assert!(
            failed
                .iter()
                .all(|update| !update.summary().safe_summary().contains("sentinel"))
        );
    }

    #[test]
    fn activity_coverage_explicit_approval_result_is_ordered_and_terminal_once() {
        let (allowed_scope, allowed_emitter) =
            activity_job_scope(ActivityOperation::CheckpointSave);
        allowed_scope.waiting_approval().unwrap();
        finish_activity_approval(true, Some(&allowed_scope), "denied").unwrap();
        finish_covered_tool_activity(
            activity_tool_coverage("checkpoint_save"),
            Some(&allowed_scope),
            &text_result("saved".to_owned()),
        );
        assert_eq!(
            allowed_emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::WaitingApproval,
                ActivityState::Running,
                ActivityState::Completed,
            ]
        );

        let (denied_scope, denied_emitter) = activity_job_scope(ActivityOperation::CheckpointSave);
        denied_scope.waiting_approval().unwrap();
        let denied = finish_activity_approval(false, Some(&denied_scope), "denied");
        assert!(denied.is_err());
        let outer_failure: Result<Value> = Err(anyhow::anyhow!("denied"));
        finish_covered_tool_activity(
            activity_tool_coverage("checkpoint_save"),
            Some(&denied_scope),
            &outer_failure,
        );
        let denied = denied_emitter.updates();
        assert_eq!(
            denied.iter().map(ActivityUpdate::state).collect::<Vec<_>>(),
            vec![
                ActivityState::Started,
                ActivityState::WaitingApproval,
                ActivityState::Failed,
            ]
        );
        assert_eq!(
            denied.last().unwrap().summary(),
            &ActivitySummary::failure(ActivityErrorKind::ApprovalDenied)
        );
    }

    #[tokio::test]
    async fn activity_job_foreground_completion_and_child_failure_are_terminalized() {
        let cwd = tempfile::tempdir().unwrap();
        let session = activity_job_session(cwd.path());

        let (success_scope, success_emitter) = activity_job_scope(ActivityOperation::Execute);
        let (rendered, _cwd, handle, completion) = spawn_sandboxed_command_with_controls(
            &json!({"command": ["sh", "-c", "printf success"]}),
            &session,
            Some(success_scope),
            std::future::pending(),
            MAX_JOB_LIFETIME,
        )
        .await
        .unwrap();
        let success = finish_foreground_or_store_job(
            &session,
            rendered,
            handle,
            completion,
            OutputPolicy::default(),
        )
        .await
        .unwrap();
        assert!(
            success["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("success")
        );
        assert_eq!(
            success_emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Completed
            ]
        );

        let (failure_scope, failure_emitter) = activity_job_scope(ActivityOperation::Execute);
        let (rendered, _cwd, handle, completion) = spawn_sandboxed_command_with_controls(
            &json!({"command": ["sh", "-c", "exit 7"]}),
            &session,
            Some(failure_scope),
            std::future::pending(),
            MAX_JOB_LIFETIME,
        )
        .await
        .unwrap();
        let failure = finish_foreground_or_store_job(
            &session,
            rendered,
            handle,
            completion,
            OutputPolicy::default(),
        )
        .await
        .unwrap_err();
        assert!(failure.to_string().contains("exit_code"));
        assert_eq!(
            failure_emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Failed
            ]
        );
        let terminal = activity_job_terminal_updates(&failure_emitter);
        assert_eq!(terminal.len(), 1);
        assert_eq!(
            terminal[0].summary().as_safe_summary(),
            "error=child_failed"
        );
    }

    #[tokio::test]
    async fn activity_job_sandbox_setup_failure_is_not_child_failed() {
        let cwd = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(cwd.path()).unwrap();
        let mut session = activity_job_session(&canonical);
        session.permission_mode = config::PermissionMode::Ask;
        session.permitted_directories =
            vec![canonical.clone(), canonical.join("missing-sandbox-root")];
        let (scope, emitter) = activity_job_scope(ActivityOperation::Execute);
        let (rendered, _cwd, handle, completion) = spawn_sandboxed_command_with_controls(
            &json!({"command": ["sh", "-c", "printf unreachable"]}),
            &session,
            Some(scope),
            std::future::pending(),
            MAX_JOB_LIFETIME,
        )
        .await
        .unwrap();
        let failure = finish_foreground_or_store_job(
            &session,
            rendered,
            handle,
            completion,
            OutputPolicy::default(),
        )
        .await
        .unwrap_err();
        assert!(
            failure.to_string().contains("writable root"),
            "unexpected sandbox setup failure: {failure:#}"
        );
        assert_eq!(
            emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Failed
            ]
        );
        let terminal = activity_job_terminal_updates(&emitter);
        assert_eq!(terminal.len(), 1);
        assert_eq!(
            terminal[0].summary().as_safe_summary(),
            "error=sandbox_setup_failed"
        );
    }

    #[tokio::test]
    async fn activity_job_return_does_not_complete_and_stop_cancels_original_scope() {
        let cwd = tempfile::tempdir().unwrap();
        let session = activity_job_session(cwd.path());
        let (command_scope, command_emitter) = activity_job_scope(ActivityOperation::StartCommand);
        let (rendered, _cwd, handle, completion) = spawn_sandboxed_command_with_controls(
            &json!({"command": ["sh", "-c", "sleep 30"]}),
            &session,
            Some(command_scope),
            std::future::pending(),
            MAX_JOB_LIFETIME,
        )
        .await
        .unwrap();
        let started = store_job(
            &session,
            rendered,
            handle,
            completion,
            OutputPolicy::default(),
            "Started",
        )
        .await
        .unwrap();
        assert_eq!(
            command_emitter.states(),
            vec![ActivityState::Started, ActivityState::Running]
        );
        let started: Value =
            serde_json::from_str(started["content"][0]["text"].as_str().unwrap()).unwrap();
        let job_id = started["job_id"].as_str().unwrap();

        let (stop_scope, stop_emitter) = activity_job_scope(ActivityOperation::StopJob);
        let stopped =
            stop_job_with_activity(&json!({"job_id": job_id}), &session, Some(&stop_scope)).await;
        finish_tool_activity(Some(&stop_scope), &stopped);
        assert!(stopped.is_ok());

        assert_eq!(
            command_emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Cancelled
            ]
        );
        let terminal = activity_job_terminal_updates(&command_emitter);
        assert_eq!(terminal.len(), 1);
        assert_eq!(
            terminal[0].summary().as_safe_summary(),
            "reason=stop_requested"
        );
        assert_eq!(
            stop_emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Completed
            ]
        );
        assert_ne!(
            command_scope_id(&command_emitter),
            command_scope_id(&stop_emitter)
        );
    }

    fn command_scope_id(emitter: &RecordingActivityEmitter) -> Uuid {
        emitter.updates()[0].operation_id()
    }

    #[test]
    fn activity_job_natural_first_and_stop_first_are_linearized_under_completion_lock() {
        for natural_first in [true, false] {
            let (scope, emitter) = activity_job_scope(ActivityOperation::StartCommand);
            scope.running().unwrap();
            let completion = Arc::new(Mutex::new(JobCompletion {
                activity: Some(scope),
                ..JobCompletion::default()
            }));
            let mut winner_guard = completion.lock().unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let contender_completion = Arc::clone(&completion);
            let contender_barrier = Arc::clone(&barrier);

            let contender = if natural_first {
                std::thread::spawn(move || {
                    contender_barrier.wait();
                    cancel_pending_job_activity(
                        &contender_completion,
                        ActivityCancellationReason::StopRequested,
                    )
                })
            } else {
                std::thread::spawn(move || {
                    contender_barrier.wait();
                    finish_job_completion(
                        &contender_completion,
                        cached_success("natural"),
                        JobActivityOutcome::Completed,
                    )
                })
            };
            barrier.wait();

            let winner = if natural_first {
                finish_job_completion_locked(
                    &mut winner_guard,
                    cached_success("natural"),
                    JobActivityOutcome::Completed,
                )
            } else {
                cancel_pending_job_activity_locked(
                    &mut winner_guard,
                    ActivityCancellationReason::StopRequested,
                )
            };
            assert!(winner);
            drop(winner_guard);
            assert!(!contender.join().unwrap());

            let terminal = activity_job_terminal_updates(&emitter);
            assert_eq!(terminal.len(), 1);
            if natural_first {
                assert_eq!(terminal[0].state(), ActivityState::Completed);
                assert_eq!(terminal[0].summary().as_safe_summary(), "");
            } else {
                assert_eq!(terminal[0].state(), ActivityState::Cancelled);
                assert_eq!(
                    terminal[0].summary().as_safe_summary(),
                    "reason=stop_requested"
                );
                assert!(completion.lock().unwrap().result.is_none());
            }
        }
    }

    #[tokio::test]
    async fn activity_job_session_stop_uses_fixed_cancellation_reason() {
        let cwd = tempfile::tempdir().unwrap();
        let session = activity_job_session(cwd.path());
        let (scope, emitter) = activity_job_scope(ActivityOperation::StartCommand);
        let (stop_sender, stop_receiver) = tokio::sync::oneshot::channel();
        let (_, _cwd, handle, completion) = spawn_sandboxed_command_with_controls(
            &json!({"command": ["sh", "-c", "sleep 30"]}),
            &session,
            Some(scope),
            async move {
                let _ = stop_receiver.await;
            },
            MAX_JOB_LIFETIME,
        )
        .await
        .unwrap();
        stop_sender.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();

        assert!(matches!(
            completion.lock().unwrap().result,
            Some(CachedJobResult::Error { .. })
        ));
        let terminal = activity_job_terminal_updates(&emitter);
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].state(), ActivityState::Cancelled);
        assert_eq!(
            terminal[0].summary().as_safe_summary(),
            "reason=session_stopped"
        );
    }

    #[tokio::test]
    async fn activity_job_lifetime_uses_fixed_cancellation_reason() {
        let cwd = tempfile::tempdir().unwrap();
        let session = activity_job_session(cwd.path());
        let (scope, emitter) = activity_job_scope(ActivityOperation::StartCommand);
        let (_, _cwd, handle, completion) = spawn_sandboxed_command_with_controls(
            &json!({"command": ["sh", "-c", "sleep 30"]}),
            &session,
            Some(scope),
            std::future::pending(),
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();

        assert!(matches!(
            completion.lock().unwrap().result,
            Some(CachedJobResult::Error { .. })
        ));
        let terminal = activity_job_terminal_updates(&emitter);
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].state(), ActivityState::Cancelled);
        assert_eq!(terminal[0].summary().as_safe_summary(), "reason=timeout");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn activity_delegated_job_executable(directory: &Path, name: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn activity_delegated_local_args(session_id: &str, task: &str) -> Value {
        json!({
            "session_id": session_id,
            "agent": "codex",
            "task": task,
            "access": "read_only"
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn activity_delegated_dev_args(session_id: &str) -> Value {
        json!({
            "session_id": session_id,
            "tool": "cargo",
            "operation": "check",
            "args": ["--workspace"]
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn activity_delegated_job_wait(handle: JoinHandle<()>) {
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("delegated worker did not finish")
            .expect("delegated worker task failed");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn activity_delegated_job_assert_terminal(
        emitter: &RecordingActivityEmitter,
        state: ActivityState,
        summary: &str,
    ) {
        let terminal = activity_job_terminal_updates(emitter);
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].state(), state);
        assert_eq!(terminal[0].summary().as_safe_summary(), summary);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn activity_delegated_job_success_is_terminal_and_omits_prompt_and_output() {
        let workspace = tempfile::tempdir().unwrap();
        let fake_dir = tempfile::tempdir().unwrap();
        let prompt_sentinel = "delegated-prompt-secret-sentinel";
        let agent_output_sentinel = "delegated-agent-output-secret-sentinel";
        let dev_output_sentinel = "delegated-dev-output-secret-sentinel";
        let fake_agent = activity_delegated_job_executable(
            fake_dir.path(),
            "codex",
            &format!("#!/bin/sh\nprintf '{agent_output_sentinel}\\n'\n"),
        );
        let fake_dev = activity_delegated_job_executable(
            fake_dir.path(),
            "cargo",
            &format!("#!/bin/sh\nprintf '{dev_output_sentinel}\\n'\n"),
        );
        let id = format!("activity-delegated-success-{}", Uuid::new_v4());
        let (sender, _receiver) = approvals::approval_channel();
        let runtime = approvals::spawn_runtime_with_logical_path_and_environment(
            workspace.path(),
            Some(&id),
            config::PermissionMode::Agent,
            sender,
            None,
            approvals::CapturedStartEnvironment::default(),
        )
        .await
        .unwrap();
        let session = config::load_session(&id).await.unwrap();

        let (agent_scope, agent_emitter) = activity_job_scope(ActivityOperation::LocalAgentRun);
        let agent_result = local_agent_run(
            &activity_delegated_local_args(&id, prompt_sentinel),
            &session,
            Some(&fake_agent),
            Some(agent_scope),
        )
        .await
        .unwrap();
        assert!(
            serde_json::to_string(&agent_result)
                .unwrap()
                .contains(agent_output_sentinel)
        );
        assert_eq!(
            agent_emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Completed
            ]
        );

        let (dev_scope, dev_emitter) = activity_job_scope(ActivityOperation::DevToolRun);
        let dev_result = dev_tool_run_with_executable(
            &activity_delegated_dev_args(&id),
            &session,
            Some(&fake_dev),
            Some(dev_scope),
        )
        .await
        .unwrap();
        assert!(
            serde_json::to_string(&dev_result)
                .unwrap()
                .contains(dev_output_sentinel)
        );
        assert_eq!(
            dev_emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Completed
            ]
        );

        let updates =
            serde_json::to_string(&(agent_emitter.updates(), dev_emitter.updates())).unwrap();
        for sentinel in [prompt_sentinel, agent_output_sentinel, dev_output_sentinel] {
            assert!(!updates.contains(sentinel), "activity leaked {sentinel}");
        }
        runtime.shutdown().await.unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn activity_delegated_job_denial_never_reaches_running() {
        let workspace = tempfile::tempdir().unwrap();
        let fake_dir = tempfile::tempdir().unwrap();
        let fake_agent =
            activity_delegated_job_executable(fake_dir.path(), "codex", "#!/bin/sh\nexit 99\n");
        let fake_dev =
            activity_delegated_job_executable(fake_dir.path(), "cargo", "#!/bin/sh\nexit 99\n");
        let id = format!("activity-delegated-deny-{}", Uuid::new_v4());
        let (sender, mut receiver) = approvals::approval_channel();
        let runtime = approvals::spawn_runtime_with_logical_path_and_environment(
            workspace.path(),
            Some(&id),
            config::PermissionMode::Ask,
            sender,
            None,
            approvals::CapturedStartEnvironment::default(),
        )
        .await
        .unwrap();
        let session = config::load_session(&id).await.unwrap();

        let (agent_scope, agent_emitter) = activity_job_scope(ActivityOperation::LocalAgentRun);
        let agent_session = session.clone();
        let agent_id = id.clone();
        let agent_task = tokio::spawn(async move {
            local_agent_run(
                &activity_delegated_local_args(&agent_id, "denied-prompt-sentinel"),
                &agent_session,
                Some(&fake_agent),
                Some(agent_scope),
            )
            .await
        });
        let prompt = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prompt.request.operation, "local_agent_run");
        prompt.respond(false);
        assert!(agent_task.await.unwrap().is_err());
        assert_eq!(
            agent_emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::WaitingApproval,
                ActivityState::Failed
            ]
        );
        activity_delegated_job_assert_terminal(
            &agent_emitter,
            ActivityState::Failed,
            "error=approval_denied",
        );

        let (dev_scope, dev_emitter) = activity_job_scope(ActivityOperation::DevToolRun);
        let dev_session = session.clone();
        let dev_id = id.clone();
        let dev_task = tokio::spawn(async move {
            dev_tool_run_with_executable(
                &activity_delegated_dev_args(&dev_id),
                &dev_session,
                Some(&fake_dev),
                Some(dev_scope),
            )
            .await
        });
        let prompt = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prompt.request.operation, "dev_tool_run");
        prompt.respond(false);
        assert!(dev_task.await.unwrap().is_err());
        assert_eq!(
            dev_emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::WaitingApproval,
                ActivityState::Failed
            ]
        );
        activity_delegated_job_assert_terminal(
            &dev_emitter,
            ActivityState::Failed,
            "error=approval_denied",
        );
        let denial_updates =
            serde_json::to_string(&(agent_emitter.updates(), dev_emitter.updates())).unwrap();
        assert!(!denial_updates.contains("denied-prompt-sentinel"));
        assert!(snapshot_jobs_for_session(&id, 50).jobs.is_empty());
        runtime.shutdown().await.unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn activity_delegated_job_timeout_uses_fixed_reason_for_both_workers() {
        let workspace = tempfile::tempdir().unwrap();
        let fake_dir = tempfile::tempdir().unwrap();
        let sleeper = "#!/bin/sh\nexec /bin/sleep 30\n";
        let fake_agent = activity_delegated_job_executable(fake_dir.path(), "codex", sleeper);
        let fake_dev = activity_delegated_job_executable(fake_dir.path(), "cargo", sleeper);
        let session = activity_job_session(workspace.path());

        let prepared_agent = local_agent::prepare_with_executable(
            &activity_delegated_local_args(&session.id, "timeout-prompt-sentinel"),
            &session,
            &fake_agent,
        )
        .unwrap();
        let (agent_scope, agent_emitter) = activity_job_scope(ActivityOperation::LocalAgentRun);
        let (_, agent_handle, agent_completion) = spawn_local_agent_with_controls(
            prepared_agent,
            &session,
            Some(agent_scope),
            std::future::pending(),
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        activity_delegated_job_wait(agent_handle).await;
        assert!(matches!(
            agent_completion.lock().unwrap().result,
            Some(CachedJobResult::Error { .. })
        ));
        activity_delegated_job_assert_terminal(
            &agent_emitter,
            ActivityState::Cancelled,
            "reason=timeout",
        );

        let prepared_dev = dev_tool::prepare_with_executable(
            &activity_delegated_dev_args(&session.id),
            &session,
            Some(&fake_dev),
        )
        .unwrap();
        let (dev_scope, dev_emitter) = activity_job_scope(ActivityOperation::DevToolRun);
        let (_, dev_handle, dev_completion) = spawn_dev_tool_with_controls(
            prepared_dev,
            &session,
            Some(dev_scope),
            std::future::pending(),
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        activity_delegated_job_wait(dev_handle).await;
        assert!(matches!(
            dev_completion.lock().unwrap().result,
            Some(CachedJobResult::Error { .. })
        ));
        activity_delegated_job_assert_terminal(
            &dev_emitter,
            ActivityState::Cancelled,
            "reason=timeout",
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn activity_delegated_job_stop_cancels_both_workers_before_abort() {
        let workspace = tempfile::tempdir().unwrap();
        let fake_dir = tempfile::tempdir().unwrap();
        let sleeper = "#!/bin/sh\nexec /bin/sleep 30\n";
        let fake_agent = activity_delegated_job_executable(fake_dir.path(), "codex", sleeper);
        let fake_dev = activity_delegated_job_executable(fake_dir.path(), "cargo", sleeper);
        let session = activity_job_session(workspace.path());

        let prepared_agent = local_agent::prepare_with_executable(
            &activity_delegated_local_args(&session.id, "stop-prompt-sentinel"),
            &session,
            &fake_agent,
        )
        .unwrap();
        let (agent_scope, agent_emitter) = activity_job_scope(ActivityOperation::LocalAgentRun);
        let (description, handle, completion) = spawn_local_agent_with_controls(
            prepared_agent,
            &session,
            Some(agent_scope),
            std::future::pending(),
            MAX_JOB_LIFETIME,
        )
        .await
        .unwrap();
        let started = store_job(
            &session,
            description,
            handle,
            completion,
            OutputPolicy::default(),
            "Backgrounded",
        )
        .await
        .unwrap();
        assert_eq!(
            agent_emitter.states(),
            vec![ActivityState::Started, ActivityState::Running]
        );
        let started: Value =
            serde_json::from_str(started["content"][0]["text"].as_str().unwrap()).unwrap();
        stop_job(
            &json!({"job_id": started["job_id"].as_str().unwrap()}),
            &session,
        )
        .await
        .unwrap();
        activity_delegated_job_assert_terminal(
            &agent_emitter,
            ActivityState::Cancelled,
            "reason=stop_requested",
        );

        let prepared_dev = dev_tool::prepare_with_executable(
            &activity_delegated_dev_args(&session.id),
            &session,
            Some(&fake_dev),
        )
        .unwrap();
        let (dev_scope, dev_emitter) = activity_job_scope(ActivityOperation::DevToolRun);
        let (description, handle, completion) = spawn_dev_tool_with_controls(
            prepared_dev,
            &session,
            Some(dev_scope),
            std::future::pending(),
            MAX_JOB_LIFETIME,
        )
        .await
        .unwrap();
        let started = store_job(
            &session,
            description,
            handle,
            completion,
            OutputPolicy::default(),
            "Backgrounded",
        )
        .await
        .unwrap();
        assert_eq!(
            dev_emitter.states(),
            vec![ActivityState::Started, ActivityState::Running]
        );
        let started: Value =
            serde_json::from_str(started["content"][0]["text"].as_str().unwrap()).unwrap();
        stop_job(
            &json!({"job_id": started["job_id"].as_str().unwrap()}),
            &session,
        )
        .await
        .unwrap();
        activity_delegated_job_assert_terminal(
            &dev_emitter,
            ActivityState::Cancelled,
            "reason=stop_requested",
        );
    }

    #[test]
    fn codex_approval_details_are_actionable_without_task_input() {
        let task_marker = "prompt-secret-marker";
        let (start_detail, start_metadata) = codex_task_start_approval(&json!({
            "operation_id": "0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa",
            "task": task_marker,
            "model": "gpt-5.6-luna",
            "effort": "max"
        }));
        assert!(start_detail.contains("operation: start task"));
        assert!(start_detail.contains("model: gpt-5.6-luna"));
        assert!(start_detail.contains("effort: max"));
        assert!(start_detail.contains("task input: omitted"));
        assert!(!start_detail.contains(task_marker));
        assert_eq!(start_metadata["provenance"], "codex_delegation");
        assert_eq!(start_metadata["tool"], "codex_task_start");
        assert_eq!(start_metadata["mutation"], "true");
        assert_eq!(start_metadata["task_input"], "omitted");
        assert!(
            !serde_json::to_string(&start_metadata)
                .unwrap()
                .contains(task_marker)
        );

        let (control_detail, control_metadata) = codex_task_control_approval(&json!({
            "task_id": "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb",
            "operation_id": "0199cccc-cccc-7ccc-8ccc-cccccccccccc",
            "action": "steer",
            "input": task_marker
        }));
        assert!(control_detail.contains("action: steer"));
        assert!(control_detail.contains("target: task 0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb"));
        assert!(control_detail.contains("control input: omitted"));
        assert!(!control_detail.contains(task_marker));
        assert_eq!(control_metadata["operation_type"], "task_control");
        assert_eq!(
            control_metadata["task_id"],
            "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb"
        );
        assert_eq!(control_metadata["control_input"], "omitted");

        let (_, status_metadata) = codex_status_approval();
        assert_eq!(status_metadata["read_only"], "true");
        assert_eq!(status_metadata["mutation"], "false");
    }

    #[test]
    fn service_account_approval_detail_lists_exact_nested_locator_scope() {
        let env = std::collections::BTreeMap::from([(
            "API_TOKEN".to_owned(),
            "op://vault/env-item/password".to_owned(),
        )]);
        let single = service_account_approval_detail(
            &["tool".to_owned()],
            &[],
            &env,
            &["op://vault/item-a/password".to_owned()],
        )
        .unwrap();
        assert!(single.contains("nested resolver locators:\n- op://vault/item-a/password"));
        assert!(!single.contains("op://vault/env-item/password"));

        let multiple = service_account_approval_detail(
            &["tool".to_owned()],
            &[],
            &env,
            &[
                "op://vault/item-a/password".to_owned(),
                "op://vault/item-b/client_secret".to_owned(),
            ],
        )
        .unwrap();
        assert!(multiple.contains("- op://vault/item-a/password"));
        assert!(multiple.contains("- op://vault/item-b/client_secret"));
        assert!(!multiple.contains("resolved-secret-sensitive-value"));
        assert!(!multiple.contains("service-account-token"));
    }

    #[test]
    fn service_account_approval_detail_distinguishes_equal_sized_locator_sets() {
        let left = service_account_approval_detail(
            &["tool".to_owned()],
            &[],
            &std::collections::BTreeMap::new(),
            &[
                "op://vault/item-a/password".to_owned(),
                "op://vault/item-b/password".to_owned(),
            ],
        )
        .unwrap();
        let right = service_account_approval_detail(
            &["tool".to_owned()],
            &[],
            &std::collections::BTreeMap::new(),
            &[
                "op://vault/item-a/password".to_owned(),
                "op://high-value-vault/admin/root-token".to_owned(),
            ],
        )
        .unwrap();
        assert_ne!(left, right);
        assert!(left.contains("op://vault/item-b/password"));
        assert!(right.contains("op://high-value-vault/admin/root-token"));
    }

    #[test]
    fn service_account_approval_detail_rejects_unrenderable_nested_scope() {
        let locators = (0..32)
            .map(|index| format!("op://vault/item-{index}/{}", "x".repeat(3000)))
            .collect::<Vec<_>>();
        let error = service_account_approval_detail(
            &["tool".to_owned()],
            &[],
            &std::collections::BTreeMap::new(),
            &locators,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot be displayed safely"));
    }

    #[tokio::test]
    async fn bounded_stdio_reader_discards_oversized_line_and_recovers() {
        let input = format!("{}\n{{\"ok\":true}}\n", "x".repeat(65));
        let mut reader = BufReader::new(input.as_bytes());

        assert_eq!(
            next_bounded_line(&mut reader, 64).await.unwrap(),
            Some(BoundedLine::TooLarge)
        );
        assert_eq!(
            next_bounded_line(&mut reader, 64).await.unwrap(),
            Some(BoundedLine::Line("{\"ok\":true}\n".to_owned()))
        );
        assert_eq!(next_bounded_line(&mut reader, 64).await.unwrap(), None);
    }

    #[tokio::test]
    async fn bounded_stdio_reader_discards_invalid_utf8_and_recovers() {
        let input = [0xff, b'\n', b'{', b'}', b'\n'];
        let mut reader = BufReader::new(input.as_slice());

        assert_eq!(
            next_bounded_line(&mut reader, 64).await.unwrap(),
            Some(BoundedLine::InvalidUtf8)
        );
        assert_eq!(
            next_bounded_line(&mut reader, 64).await.unwrap(),
            Some(BoundedLine::Line("{}\n".to_owned()))
        );
    }

    #[test]
    fn generated_mcp_response_encoding_matches_wire_limit() -> noprop::TestResult {
        test_support::run(0x4d43_5052_4553_5042, 512, |ctx| {
            let max_bytes = noprop::sample_usize_in(ctx, 96..=512);
            let payload_len = noprop::sample_usize_in(ctx, 0..=600);
            let payload = (0..payload_len)
                .map(|_| match noprop::sample_usize_in(ctx, 0..=3) {
                    0 => 'x',
                    1 => '"',
                    2 => '\\',
                    _ => '\n',
                })
                .collect::<String>();
            let message = json!({
                "jsonrpc": "2.0",
                "id": 7,
                "result": {"content": [{"type": "text", "text": payload}]}
            });
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
    fn oversized_mcp_response_degrades_to_bounded_json_rpc_error() {
        let message = json!({
            "jsonrpc": "2.0",
            "id": "request-7",
            "result": {"content": [{"type": "text", "text": "x".repeat(2048)}]}
        });
        let line = bounded_mcp_response_line(&message, 512).unwrap();
        assert!(line.len() <= 512);
        assert_eq!(line.last(), Some(&b'\n'));
        let response: Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        assert_eq!(response["id"], "request-7");
        assert_eq!(response["error"]["code"], -32000);
        assert!(
            response["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("MCP response exceeds"))
        );
    }

    #[test]
    fn stdio_response_budget_covers_max_image_and_request_line() {
        let base64_bytes = MAX_IMAGE_BYTES.div_ceil(3) * 4;
        let conservative_json_overhead = 64 * 1024;
        let required = MAX_JSON_LINE_BYTES
            .checked_add(base64_bytes)
            .and_then(|bytes| bytes.checked_add(conservative_json_overhead))
            .unwrap();
        assert!(
            required <= MAX_MCP_RESPONSE_BYTES,
            "required={required} budget={MAX_MCP_RESPONSE_BYTES}"
        );
    }

    #[test]
    fn generated_stdio_line_boundaries_match_wire_limit() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        test_support::run(0x5354_4449_4f4c_494e, 512, |ctx| {
            const LIMIT: usize = 64;
            let payload_len = noprop::sample_usize_in(ctx, 0..=LIMIT + 16);
            let mut input = vec![b'x'; payload_len];
            input.push(b'\n');
            let result = runtime.block_on(async {
                let mut reader = BufReader::new(input.as_slice());
                next_bounded_line(&mut reader, LIMIT).await.unwrap()
            });
            if payload_len + 1 > LIMIT {
                assert_eq!(result, Some(BoundedLine::TooLarge));
            } else {
                assert_eq!(
                    result,
                    Some(BoundedLine::Line(format!("{}\n", "x".repeat(payload_len))))
                );
            }
            Ok(())
        })
    }

    #[test]
    fn generated_directory_listing_budget_matches_reference_model() -> noprop::TestResult {
        test_support::run(0x4449_5242_5544_4745, 512, |ctx| {
            let max_entries = noprop::sample_usize_in(ctx, 0..=8);
            let max_bytes = noprop::sample_usize_in(ctx, 0..=96);
            let count = noprop::sample_usize_in(ctx, 0..=12);
            let entries = (0..count)
                .map(|_| test_support::safe_component(ctx))
                .collect::<Vec<_>>();
            let mut names = Vec::new();
            let mut rendered_bytes = 0usize;
            let mut reference_bytes = 0usize;

            for name in entries {
                let separator = usize::from(!names.is_empty());
                let next = reference_bytes
                    .checked_add(separator)
                    .and_then(|value| value.checked_add(name.len()));
                let expected =
                    names.len() < max_entries && next.is_some_and(|value| value <= max_bytes);
                let result = push_directory_listing_entry(
                    &mut names,
                    &mut rendered_bytes,
                    name.clone(),
                    max_entries,
                    max_bytes,
                );
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "budget mismatch: name={name:?} entries={} bytes={reference_bytes} max_entries={max_entries} max_bytes={max_bytes}",
                    names.len()
                );
                if !expected {
                    break;
                }
                reference_bytes = next.unwrap();
                assert_eq!(rendered_bytes, reference_bytes);
                assert_eq!(names.join("\n").len(), reference_bytes);
            }
            Ok(())
        })
    }

    #[test]
    fn generated_session_list_budget_matches_reference_model() -> noprop::TestResult {
        test_support::run(0x5345_5353_4c49_5354, 512, |ctx| {
            let max_entries = noprop::sample_usize_in(ctx, 0..=8);
            let max_bytes = noprop::sample_usize_in(ctx, 0..=1024);
            let count = noprop::sample_usize_in(ctx, 0..=12);
            let mut sessions = Vec::new();
            let mut rendered_bytes = 0usize;
            let mut reference_bytes = 0usize;

            for index in 0..count {
                let repeat = noprop::sample_usize_in(ctx, 0..=96);
                let session = json!({
                    "session_id": format!("generated-{index}"),
                    "cwd": "x".repeat(repeat),
                    "status": if noprop::sample_bool(ctx) { "active" } else { "unknown" },
                });
                let charged = serde_json::to_string_pretty(&session).unwrap().len() + 64;
                let next = reference_bytes.checked_add(charged);
                let expected =
                    sessions.len() < max_entries && next.is_some_and(|value| value <= max_bytes);
                let result = push_session_list_entry(
                    &mut sessions,
                    &mut rendered_bytes,
                    session,
                    max_entries,
                    max_bytes,
                );
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "entries={} bytes={reference_bytes} max_entries={max_entries} max_bytes={max_bytes}",
                    sessions.len()
                );
                if !expected {
                    break;
                }
                reference_bytes = next.unwrap();
                assert_eq!(rendered_bytes, reference_bytes);
            }
            Ok(())
        })
    }

    #[test]
    fn generated_bounded_text_reads_round_trip_utf8() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("text.txt");

        test_support::run(0x5445_5854_5245_4144, 256, |ctx| {
            let count = noprop::sample_usize_in(ctx, 0..=128);
            let text = (0..count)
                .map(|_| test_support::safe_component(ctx))
                .collect::<Vec<_>>()
                .join(" ");
            std::fs::write(&path, text.as_bytes()).unwrap();
            let actual = runtime.block_on(read_text_file(&path)).unwrap();
            assert_eq!(actual, text);
            Ok(())
        })
    }

    #[test]
    fn ranged_text_reads_are_utf8_safe_and_resumable() {
        let text = "αβγ\nbravo\ncharlie\n";
        let full_lines = ranged_text_result(
            &json!({"start_line": 2, "end_line": 3, "max_bytes": 64}),
            text,
        )
        .unwrap();
        assert_eq!(full_lines["content"], "bravo\ncharlie\n");
        assert_eq!(full_lines["truncated"], false);
        assert_eq!(full_lines["next_offset_bytes"], Value::Null);

        let first = ranged_text_result(&json!({"start_line": 1, "max_bytes": 4}), text).unwrap();
        assert_eq!(first["content"], "αβ");
        assert_eq!(first["returned_bytes"], 4);
        assert_eq!(first["truncated"], true);
        let next = first["next_offset_bytes"].as_u64().unwrap() as usize;
        let resumed =
            ranged_text_result(&json!({"offset_bytes": next, "max_bytes": 64}), text).unwrap();
        assert_eq!(resumed["content"], "γ\nbravo\ncharlie\n");
        assert_eq!(resumed["start_offset_bytes"], next);
        assert_eq!(resumed["utf8_boundary"], true);

        assert!(ranged_text_result(&json!({"offset_bytes": 1, "max_bytes": 4}), text).is_err());
        assert!(
            ranged_text_result(
                &json!({"start_line": 2, "offset_bytes": 0, "max_bytes": 4}),
                text
            )
            .is_err()
        );
    }

    #[test]
    fn command_output_policy_is_bounded_and_references_scoped_evidence() {
        let root = tempfile::tempdir().unwrap();
        let full = json!({
            "exit_code": 0,
            "stdout": "x".repeat(300),
            "stderr": "tail",
            "truncated": false
        })
        .to_string();
        let reference = evidence::store("session", root.path(), full.clone())
            .unwrap()
            .unwrap();

        assert_eq!(
            apply_output_policy(&full, OutputPolicy::default(), Some(&reference)),
            full
        );
        let limited: Value = serde_json::from_str(&apply_output_policy(
            &full,
            OutputPolicy {
                output_limit_bytes: Some(256),
                status_only: false,
            },
            Some(&reference),
        ))
        .unwrap();
        assert_eq!(limited["stdout"].as_str().unwrap().len(), 256);
        assert_eq!(limited["stderr"], "");
        assert_eq!(limited["returned_truncated"], true);
        assert_eq!(limited["evidence"]["evidence_id"], reference.evidence_id);

        let status: Value = serde_json::from_str(&apply_output_policy(
            &full,
            OutputPolicy {
                output_limit_bytes: None,
                status_only: true,
            },
            Some(&reference),
        ))
        .unwrap();
        assert!(status.get("stdout").is_none());
        assert!(status.get("stderr").is_none());
        assert_eq!(status["output_omitted"], true);
        assert_eq!(status["evidence"]["evidence_id"], reference.evidence_id);
    }

    #[test]
    fn command_output_policy_validation_rejects_ambiguous_requests() {
        assert!(
            parse_output_policy(&json!({"status_only": true}))
                .unwrap()
                .status_only
        );
        assert!(
            parse_output_policy(&json!({
                "status_only": true,
                "output_limit_bytes": 256
            }))
            .is_err()
        );
        assert!(parse_output_policy(&json!({"output_limit_bytes": 255})).is_err());
    }

    #[tokio::test]
    async fn bounded_text_read_rejects_oversized_and_invalid_utf8_files() {
        let root = tempfile::tempdir().unwrap();
        let oversized = root.path().join("oversized.txt");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(MAX_TEXT_FILE_BYTES as u64 + 1).unwrap();
        assert!(
            read_text_file(&oversized)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("file exceeds")
        );

        let invalid = root.path().join("invalid.txt");
        std::fs::write(&invalid, [0xff, 0xfe]).unwrap();
        assert!(
            read_text_file(&invalid)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("valid UTF-8")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_reads_reject_final_component_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let text_target = root.path().join("text-target.txt");
        let text_link = root.path().join("text-link.txt");
        std::fs::write(&text_target, b"secret").unwrap();
        symlink(&text_target, &text_link).unwrap();
        assert!(read_text_file(&text_link).await.is_err());

        let image_target = root.path().join("image-target.png");
        let image_link = root.path().join("image-link.png");
        std::fs::write(&image_target, b"\x89PNG\r\n\x1a\n").unwrap();
        symlink(&image_target, &image_link).unwrap();
        assert!(get_image(&image_link).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_tools_reject_special_file_targets_without_blocking() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("special.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

        assert!(read_text_file(&socket).await.is_err());
        assert!(
            ensure_regular_write_target(&socket)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("not a regular file")
        );
    }

    #[test]
    fn detects_supported_image_types() {
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(image_mime_type(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(image_mime_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(image_mime_type(b"RIFF\0\0\0\0WEBP"), Some("image/webp"));
        assert_eq!(image_mime_type(b"not an image"), None);
    }

    #[test]
    fn generated_supported_image_prefixes_survive_arbitrary_suffixes() -> noprop::TestResult {
        test_support::run(0x494d_4147_454d_494d, 512, |ctx| {
            let (prefix, expected): (&[u8], &str) = match noprop::sample_usize_in(ctx, 0..7) {
                0 => (b"\x89PNG\r\n\x1a\n", "image/png"),
                1 => (b"\xff\xd8\xff", "image/jpeg"),
                2 => (b"GIF89a", "image/gif"),
                3 => (b"RIFF\0\0\0\0WEBP", "image/webp"),
                4 => (b"BM", "image/bmp"),
                5 => (b"II*\0", "image/tiff"),
                _ => (b"\0\0\0\0ftypavif", "image/avif"),
            };
            let suffix_len = noprop::sample_usize_in(ctx, 0..=64);
            let mut bytes = prefix.to_vec();
            bytes.extend((0..suffix_len).map(|_| noprop::sample_u8(ctx)));
            assert_eq!(image_mime_type(&bytes), Some(expected));
            Ok(())
        })
    }

    #[tokio::test]
    async fn get_image_rejects_oversized_files_before_reading_them() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("oversized.png");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();

        let error = get_image(&path).await.unwrap_err();
        assert!(error.to_string().contains("image exceeds"));
    }

    #[test]
    fn renders_edit_counts_and_unified_diff() {
        let (added, removed, diff) = render_diff("one\ntwo\n", "one\nchanged\nthree\n");

        assert_eq!((added, removed), (2, 1));
        assert!(diff.contains("-two"));
        assert!(diff.contains("+changed"));
        assert!(diff.contains("+three"));
    }

    #[test]
    fn quotes_command_arguments_for_activity_display() {
        assert_eq!(shell_word("README.md"), "README.md");
        assert_eq!(shell_word("hello world"), "\"hello world\"");
    }

    #[test]
    fn child_mcp_approval_summary_hides_argument_values() {
        let summary = safe_child_call_summary(
            "kintone-add-record",
            &json!({
                "app": "42",
                "record": {"secret_field": {"value": "sensitive-value"}}
            }),
        );
        assert!(summary.contains("app"));
        assert!(summary.contains("record"));
        assert!(!summary.contains("42"));
        assert!(!summary.contains("secret_field"));
        assert!(!summary.contains("sensitive-value"));
    }

    #[test]
    fn cli_kintone_approval_summary_hides_argument_values_and_paths() {
        let summary = safe_kintone_cli_summary(
            &[
                "record".to_owned(),
                "export".to_owned(),
                "--app=42".to_owned(),
                "--attachments-dir".to_owned(),
                "/private/work/attachments".to_owned(),
            ],
            Some(Path::new("/private/work/export.csv")),
        );
        assert!(summary.contains("record export"));
        assert!(summary.contains("--app"));
        assert!(summary.contains("--attachments-dir"));
        assert!(!summary.contains("42"));
        assert!(!summary.contains("/private/work"));
        assert!(!summary.contains("export.csv"));
    }

    #[test]
    fn generated_rpc_request_shapes_match_reference_model() -> noprop::TestResult {
        test_support::run(0x5250_4353_4841_5045, 512, |ctx| {
            let valid_version = noprop::sample_bool(ctx);
            let method_len = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => 0,
                1 => 1,
                2 => MAX_RPC_METHOD_BYTES,
                3 => MAX_RPC_METHOD_BYTES + 1,
                _ => noprop::sample_usize_in(ctx, 0..=MAX_RPC_METHOD_BYTES + 32),
            };
            let method_is_string = noprop::sample_bool(ctx);
            let id_kind = noprop::sample_usize_in(ctx, 0..=5);
            let id_len = match noprop::sample_usize_in(ctx, 0..=3) {
                0 => 0,
                1 => MAX_RPC_ID_STRING_BYTES,
                2 => MAX_RPC_ID_STRING_BYTES + 1,
                _ => noprop::sample_usize_in(ctx, 0..=MAX_RPC_ID_STRING_BYTES + 32),
            };
            let id = match id_kind {
                0 => None,
                1 => Some(Value::Null),
                2 => Some(json!(noprop::sample_u64(ctx))),
                3 => Some(Value::String("i".repeat(id_len))),
                4 => Some(Value::Bool(noprop::sample_bool(ctx))),
                _ => Some(json!({"bad": true})),
            };
            let method = if method_is_string {
                Value::String("m".repeat(method_len))
            } else {
                Value::Bool(true)
            };
            let mut object = serde_json::Map::new();
            object.insert(
                "jsonrpc".to_owned(),
                Value::String(if valid_version { "2.0" } else { "1.0" }.to_owned()),
            );
            object.insert("method".to_owned(), method);
            if let Some(id) = id.clone() {
                object.insert("id".to_owned(), id);
            }
            let request = Value::Object(object);
            let expected_id = match id {
                None | Some(Value::Null) | Some(Value::Number(_)) => true,
                Some(Value::String(value)) => value.len() <= MAX_RPC_ID_STRING_BYTES,
                Some(Value::Bool(_) | Value::Array(_) | Value::Object(_)) => false,
            };
            let expected = valid_version
                && method_is_string
                && method_len > 0
                && method_len <= MAX_RPC_METHOD_BYTES
                && expected_id;
            assert_eq!(validate_rpc_request_shape(&request).is_ok(), expected);
            Ok(())
        })
    }

    #[test]
    fn generated_mcp_tool_names_match_byte_limit() -> noprop::TestResult {
        test_support::run(0x544f_4f4c_4e41_4d45, 512, |ctx| {
            let length = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => 0,
                1 => 1,
                2 => MAX_MCP_TOOL_NAME_BYTES,
                3 => MAX_MCP_TOOL_NAME_BYTES + 1,
                _ => noprop::sample_usize_in(ctx, 0..=MAX_MCP_TOOL_NAME_BYTES + 32),
            };
            let name = "t".repeat(length);
            assert_eq!(
                validate_mcp_tool_name(&name).is_ok(),
                length > 0 && length <= MAX_MCP_TOOL_NAME_BYTES
            );
            Ok(())
        })
    }

    #[tokio::test]
    async fn negotiates_supported_protocol_versions() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-03-26"}
        });
        let result = dispatch(&request).await.unwrap();
        assert_eq!(result["protocolVersion"], "2025-03-26");

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "future-version"}
        });
        let result = dispatch(&request).await.unwrap();
        assert_eq!(result["protocolVersion"], LATEST_LEGACY_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn handshake_exposes_non_secret_process_identity() {
        let initialize = dispatch(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        }))
        .await
        .unwrap();
        let identity = &initialize["_meta"][PROCESS_IDENTITY_META_KEY];
        let version = &identity["version"];
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        let boot_generation = &identity["boot_generation"];
        assert_eq!(boot_generation, crate::boot_identity::generation());
        assert!(identity["host_id"].is_string());
        assert!(!identity["host_id"].as_str().unwrap().is_empty());

        let ping = dispatch(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "ping",
            "params": {}
        }))
        .await
        .unwrap();
        let ping_identity = &ping["_meta"][PROCESS_IDENTITY_META_KEY];
        let initialize_identity = &initialize["_meta"][PROCESS_IDENTITY_META_KEY];
        assert_eq!(ping_identity, initialize_identity);

        let discover = dispatch(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "server/discover",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        }))
        .await
        .unwrap();
        let discovered = &discover["_meta"][PROCESS_IDENTITY_META_KEY]["boot_generation"];
        assert_eq!(discovered, crate::boot_identity::generation());

        let encoded = serde_json::to_string(&initialize["_meta"])
            .unwrap()
            .to_ascii_lowercase();
        for forbidden in ["token", "secret", "password", "authorization", "cookie"] {
            assert!(!encoded.contains(forbidden), "leaked {forbidden}");
        }
    }

    #[tokio::test]
    async fn modern_initialize_keeps_server_info_and_process_identity() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "modern-init",
            "method": "initialize",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        let result = dispatch(&request).await.unwrap();
        assert_eq!(result["resultType"], "complete");
        let server_name = &result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"];
        assert_eq!(server_name, "temote-mcp");
        let identity = &result["_meta"][PROCESS_IDENTITY_META_KEY];
        let boot_generation = &identity["boot_generation"];
        assert_eq!(boot_generation, crate::boot_identity::generation());
    }

    #[tokio::test]
    async fn modern_ping_keeps_server_info_and_process_identity() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "modern-ping",
            "method": "ping",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        let result = dispatch(&request).await.unwrap();
        assert_eq!(result["resultType"], "complete");
        assert_eq!(
            result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "temote-mcp"
        );
        let identity = &result["_meta"][PROCESS_IDENTITY_META_KEY];
        assert_eq!(identity["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            identity["boot_generation"],
            crate::boot_identity::generation()
        );
        assert!(identity["host_id"].is_string());
        assert!(result.get(PROCESS_IDENTITY_META_KEY).is_none());
    }

    #[test]
    fn generated_legacy_protocol_negotiation_matches_supported_set() -> noprop::TestResult {
        test_support::run(0x4d43_504c_4547_4143, 512, |ctx| {
            let requested = if noprop::sample_bool(ctx) {
                SUPPORTED_LEGACY_PROTOCOL_VERSIONS
                    [noprop::sample_usize_in(ctx, 0..SUPPORTED_LEGACY_PROTOCOL_VERSIONS.len())]
                .to_owned()
            } else {
                format!("future-{}", test_support::safe_component(ctx))
            };
            let request = json!({"params": {"protocolVersion": requested}});
            let expected = SUPPORTED_LEGACY_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .find(|version| *version == request["params"]["protocolVersion"].as_str().unwrap())
                .unwrap_or(LATEST_LEGACY_PROTOCOL_VERSION);
            assert_eq!(negotiate_protocol_version(&request), expected);
            Ok(())
        })
    }

    #[test]
    fn generated_modern_meta_matches_detection_and_validation_model() -> noprop::TestResult {
        const MODERN_KEYS: [&str; 4] = [
            "io.modelcontextprotocol/protocolVersion",
            "io.modelcontextprotocol/clientCapabilities",
            "io.modelcontextprotocol/clientInfo",
            "io.modelcontextprotocol/logLevel",
        ];
        test_support::run(0x4d43_504d_4f44_4552, test_support::DEFAULT_CASES, |ctx| {
            let mut meta = serde_json::Map::new();
            let include_marker = noprop::sample_bool(ctx);
            if include_marker {
                let key = MODERN_KEYS[noprop::sample_usize_in(ctx, 0..MODERN_KEYS.len())];
                meta.insert(key.to_owned(), Value::Null);
            } else if noprop::sample_bool(ctx) {
                meta.insert("unrelated".to_owned(), Value::Bool(true));
            }

            let valid_version = noprop::sample_bool(ctx);
            let valid_caps = noprop::sample_bool(ctx);
            if noprop::sample_bool(ctx) {
                meta.insert(
                    "io.modelcontextprotocol/protocolVersion".to_owned(),
                    if valid_version {
                        Value::String(MODERN_PROTOCOL_VERSION.to_owned())
                    } else {
                        Value::String("unsupported".to_owned())
                    },
                );
            }
            if noprop::sample_bool(ctx) {
                meta.insert(
                    "io.modelcontextprotocol/clientCapabilities".to_owned(),
                    if valid_caps {
                        json!({})
                    } else {
                        Value::String("bad".to_owned())
                    },
                );
            }
            let request = json!({"params": {"_meta": Value::Object(meta.clone())}});
            let expected_modern = MODERN_KEYS.iter().any(|key| meta.contains_key(*key));
            assert_eq!(modern_request(&request), expected_modern);

            let expected_valid = meta
                .get("io.modelcontextprotocol/protocolVersion")
                .and_then(Value::as_str)
                == Some(MODERN_PROTOCOL_VERSION)
                && meta
                    .get("io.modelcontextprotocol/clientCapabilities")
                    .is_some_and(Value::is_object);
            assert_eq!(
                validate_modern_request(&request).is_ok(),
                expected_valid,
                "meta={meta:?}"
            );
            Ok(())
        })
    }

    #[tokio::test]
    async fn modern_discovery_advertises_only_the_modern_protocol() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "discover-1",
            "method": "server/discover",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });

        let result = dispatch(&request).await.unwrap();
        assert_eq!(result["resultType"], "complete");
        assert_eq!(
            result["supportedVersions"],
            json!([MODERN_PROTOCOL_VERSION])
        );
        assert_eq!(result["ttlMs"], 0);
        assert_eq!(result["cacheScope"], "private");
        assert_eq!(
            result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "temote-mcp"
        );
    }

    #[tokio::test]
    async fn modern_tool_list_uses_the_2026_result_shape() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });

        let result = dispatch(&request).await.unwrap();
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["ttlMs"], 0);
        assert_eq!(result["cacheScope"], "private");
        assert!(result["tools"].is_array());
        assert_eq!(
            result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "temote-mcp"
        );
    }

    #[test]
    fn routed_gateway_contract_matches_checked_in_snapshot() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("gateway")
            .join("contract")
            .join("routed-tools.json");
        let mut rendered = serde_json::to_string_pretty(&routed_gateway_contract()).unwrap();
        rendered.push('\n');
        if std::env::var_os("TEMOTE_MCP_UPDATE_GATEWAY_CONTRACT").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &rendered).unwrap();
        }
        let checked_in = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "could not read gateway contract {}: {error}; regenerate with TEMOTE_MCP_UPDATE_GATEWAY_CONTRACT=1 cargo test routed_gateway_contract_matches_checked_in_snapshot",
                path.display()
            )
        });
        assert_eq!(checked_in, rendered, "gateway contract snapshot is stale");
    }

    #[test]
    fn public_contract_fingerprint_matches_checked_in_snapshot() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("gateway")
            .join("contract")
            .join("public-tools.fingerprint");
        let rendered = format!("{}\n", public_contract_fingerprint());
        if std::env::var_os("TEMOTE_MCP_UPDATE_GATEWAY_CONTRACT").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &rendered).unwrap();
        }
        let checked_in = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "could not read public contract fingerprint {}: {error}; regenerate with TEMOTE_MCP_UPDATE_GATEWAY_CONTRACT=1 cargo test public_contract_fingerprint_matches_checked_in_snapshot",
                path.display()
            )
        });
        assert_eq!(
            checked_in, rendered,
            "public contract fingerprint is stale; the connected runtime would not match the repository contract"
        );
    }

    #[test]
    fn diagnostics_surfaces_report_the_public_contract_fingerprint() {
        let fingerprint = public_contract_fingerprint();
        assert_eq!(fingerprint.len(), 64);
        assert!(
            fingerprint
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        );
        assert_eq!(
            discover_result()["_meta"]["dev.temote/contractFingerprint"],
            json!(fingerprint)
        );
    }

    #[test]
    fn public_tools_have_chatgpt_display_metadata() {
        let tools = tools(true, true).as_array().unwrap().to_owned();
        assert_eq!(tools.len(), 56);
        assert!(tools.iter().all(|tool| {
            tool["name"].is_string()
                && tool["title"].is_string()
                && tool["description"].is_string()
                && tool["inputSchema"].is_object()
                && tool["annotations"].is_object()
        }));
        assert!(tools.iter().all(|tool| {
            let description = tool["description"].as_str().unwrap();
            !description.contains("ChatGPT should confirm")
                && !description.contains("unless session_info reports yolo=true")
        }));
        assert!(tools.iter().any(|tool| tool["name"] == "session_start"));
        assert!(tools.iter().any(|tool| tool["name"] == "session_stop"));
        assert!(tools.iter().any(|tool| tool["name"] == "session_restart"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_add"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_commit"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_fetch"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_pull"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_push"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_push_tag"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_branch_create"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_switch"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_worktree_add"));
        assert!(
            tools
                .iter()
                .any(|tool| tool["name"] == "git_worktree_create")
        );
        assert!(tools.iter().any(|tool| tool["name"] == "git_worktree_list"));
        assert!(
            tools
                .iter()
                .any(|tool| tool["name"] == "github_workflow_dispatch")
        );
        assert!(
            tools
                .iter()
                .any(|tool| tool["name"] == "github_workflow_run_get")
        );
        for name in [
            "codex_status",
            "codex_task_start",
            "codex_task_get",
            "codex_task_control",
            "local_agent_run",
        ] {
            assert!(tools.iter().any(|tool| tool["name"] == name));
        }
        let local_agent = tools
            .iter()
            .find(|tool| tool["name"] == "local_agent_run")
            .unwrap();
        assert_eq!(
            local_agent["annotations"],
            json!({
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            })
        );
        assert_eq!(local_agent["inputSchema"]["additionalProperties"], false);
        assert_eq!(
            local_agent["inputSchema"]["properties"]["agent"]["enum"],
            json!(["codex", "opencode"])
        );
        assert_eq!(
            local_agent["inputSchema"]["properties"]["access"]["enum"],
            json!(["read_only", "workspace_write"])
        );
        assert_eq!(
            local_agent["inputSchema"]["properties"]["task"]["maxLength"],
            json!(local_agent::MAX_TASK_BYTES)
        );
        assert_eq!(
            local_agent["inputSchema"]["allOf"][0]["then"]["properties"]["task"]["maxLength"],
            json!(local_agent::MAX_OPENCODE_TASK_BYTES)
        );
        assert!(tools.iter().all(|tool| tool["name"] != "without_sandbox"));
        assert!(
            tools
                .iter()
                .any(|tool| tool["name"] == "onepassword_secret_resolve")
        );
    }

    #[tokio::test]
    async fn local_agent_run_requires_approval_and_denial_starts_no_job() {
        let root = tempfile::tempdir().unwrap();
        let fake_agent_dir = tempfile::tempdir().unwrap();
        let fake_executable = fake_agent_dir.path().join("codex");
        std::fs::write(&fake_executable, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&fake_executable).unwrap().permissions();
        #[cfg(unix)]
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
        std::fs::set_permissions(&fake_executable, permissions).unwrap();
        let id = format!("local-agent-approval-{}", Uuid::new_v4());
        let (sender, mut receiver) = approvals::approval_channel();
        let handle = approvals::spawn_runtime(root.path(), Some(&id), false, sender)
            .await
            .unwrap();

        let request = json!({
            "name": "local_agent_run",
            "arguments": {
                "session_id": id.clone(),
                "agent": "codex",
                "task": "approval-only test task",
                "access": "read_only"
            }
        });
        let task = tokio::spawn(async move {
            let _fake_agent_dir = fake_agent_dir;
            call_tool_with_test_local_agent_executable(&request, false, None, &fake_executable)
                .await
        });
        let prompt = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("local_agent_run did not request approval")
            .expect("approval channel closed before local_agent_run request");
        assert_eq!(prompt.request.operation, "local_agent_run");
        assert!(prompt.request.detail.contains("task_preview:"));
        assert!(prompt.request.detail.contains("approval-only test task"));
        prompt.respond(false);

        let error = task
            .await
            .unwrap()
            .expect_err("denied local_agent_run unexpectedly succeeded");
        assert!(error.to_string().contains("user denied local_agent_run"));
        assert!(snapshot_jobs_for_session(&id, 50).jobs.is_empty());
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_list_surfaces_ambiguous_probe_as_unknown() {
        use tokio::io::AsyncWriteExt as _;

        let root = tempfile::tempdir().unwrap();
        let cwd = config::canonical_directory(root.path()).unwrap();
        let id = format!("list-unknown-{}", Uuid::new_v4());
        let session = config::Session {
            id: id.clone(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 1,
            process_id: 1,
            permission_mode: config::PermissionMode::Ask,
        };
        config::save_session(&session).await.unwrap();

        let socket = config::socket_path(&id).unwrap();
        tokio::fs::create_dir_all(socket.parent().unwrap())
            .await
            .unwrap();
        let _ = tokio::fs::remove_file(&socket).await;
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"ambiguous\n").await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let listed = crate::session_control::inspect_session_read_only(&id)
            .await
            .expect("ambiguous session should be surfaced");
        server.await.unwrap();
        assert_eq!(listed.session_id, id);
        assert_eq!(listed.status, "unknown");

        tokio::fs::remove_file(config::session_path(&id).unwrap())
            .await
            .unwrap();
        tokio::fs::remove_file(socket).await.unwrap();
    }

    #[tokio::test]
    async fn session_info_renders_a_missing_workspace_as_degraded() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let cwd = config::canonical_directory(&workspace).unwrap();
        let id = format!("info-degraded-{}", Uuid::new_v4());
        let session = config::Session {
            id: id.clone(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
            started_at: 1,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
        };
        config::save_session(&session).await.unwrap();
        let mut lifecycle = config::SessionLifecycle::starting(session.started_at, None);
        lifecycle.status = config::LifecycleStatus::Stopped;
        lifecycle.stopped_at = Some(config::unix_time());
        config::save_session_lifecycle(&id, &lifecycle)
            .await
            .unwrap();
        std::fs::remove_dir_all(&cwd).unwrap();

        let result = call_tool(
            &json!({"name": "session_info", "arguments": {"session_id": id}}),
            false,
            None,
        )
        .await
        .expect("a missing workspace must not fail session_info");
        let rendered: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(rendered["session_id"], id);
        assert_eq!(rendered["status"], "degraded");
        assert_eq!(rendered["cwd"], serde_json::to_value(&cwd).unwrap());
        assert!(rendered["server_contract_fingerprint"].is_string());
        assert!(
            config::session_path(&id).unwrap().exists(),
            "session_info must not delete stale metadata"
        );

        let _ = tokio::fs::remove_file(config::session_path(&id).unwrap()).await;
        let _ = tokio::fs::remove_file(config::session_lifecycle_path(&id).unwrap()).await;
    }

    #[test]
    fn accepts_configured_git_remote_names_only() {
        for remote in ["origin", "upstream", "team/review", "release-1.0"] {
            validate_git_remote(remote).unwrap();
        }
        for remote in [
            "",
            "-origin",
            "../origin",
            "https://example.com/repo",
            "git@example.com:repo",
        ] {
            assert!(validate_git_remote(remote).is_err(), "accepted {remote:?}");
        }
    }

    #[test]
    fn git_remote_validation_matches_reference_grammar() -> noprop::TestResult {
        test_support::run(0x4749_5452_454d_4f54, test_support::DEFAULT_CASES, |ctx| {
            let remote = test_support::ascii_string(ctx, 280);
            let expected = !remote.is_empty()
                && remote.len() <= 255
                && !remote.starts_with('-')
                && !remote.starts_with('/')
                && !remote.ends_with('/')
                && !remote.contains("..")
                && !remote.contains("//")
                && remote.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "-_./".contains(character)
                });
            assert_eq!(
                validate_git_remote(&remote).is_ok(),
                expected,
                "Git remote grammar mismatch for {remote:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_path_arguments_match_byte_limit() -> noprop::TestResult {
        test_support::run(0x5041_5448_424f_554e, 512, |ctx| {
            let length = match noprop::sample_usize_in(ctx, 0..=5) {
                0 => 0,
                1 => 1,
                2 => MAX_PATH_ARGUMENT_BYTES - 1,
                3 => MAX_PATH_ARGUMENT_BYTES,
                4 => MAX_PATH_ARGUMENT_BYTES + 1,
                _ => noprop::sample_usize_in(ctx, 0..=MAX_PATH_ARGUMENT_BYTES + 256),
            };
            let value = "x".repeat(length);
            assert_eq!(
                validate_path_argument(&value, "path").is_ok(),
                length <= MAX_PATH_ARGUMENT_BYTES,
                "length={length}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_git_paths_reject_pathspecs_and_outside_roots() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let root = config::canonical_directory(&root).unwrap();
        let outside = config::canonical_directory(&outside).unwrap();
        let session = config::Session {
            id: "git-pbt".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root.clone()],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Ask,
        };

        test_support::run(0x4749_5450_4154_4801, 512, |ctx| {
            let leaf = test_support::safe_component(ctx);
            let inside = root.join(&leaf);
            std::fs::write(&inside, b"ok").unwrap();
            assert!(
                validate_git_path(&session, &leaf).is_ok(),
                "safe path rejected: {leaf:?}"
            );

            let dangerous = match noprop::sample_usize_in(ctx, 0..5) {
                0 => format!("-{leaf}"),
                1 => format!(":{leaf}"),
                2 => format!("{leaf}*"),
                3 => format!("{leaf}?"),
                _ => format!("{leaf}[0]"),
            };
            assert!(
                validate_git_path(&session, &dangerous).is_err(),
                "pathspec unexpectedly accepted: {dangerous:?}"
            );

            let outside_path = outside.join(&leaf);
            std::fs::write(&outside_path, b"secret").unwrap();
            assert!(
                validate_git_path(&session, &outside_path.to_string_lossy()).is_err(),
                "outside path unexpectedly accepted: {outside_path:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_command_arguments_match_schema_contract() -> noprop::TestResult {
        test_support::run(0x434f_4d4d_414e_4401, test_support::DEFAULT_CASES, |ctx| {
            let len = noprop::sample_usize_in(ctx, 0..=8);
            let mut items = (0..len)
                .map(|_| Value::String(test_support::safe_component(ctx)))
                .collect::<Vec<_>>();
            let corrupt = !items.is_empty() && noprop::sample_bool(ctx);
            if corrupt {
                let index = noprop::sample_usize_in(ctx, 0..items.len());
                items[index] = json!(noprop::sample_u64(ctx));
            }
            let args = json!({"command": items});
            let parsed = required_command(&args);
            let expected = len > 0 && !corrupt;
            assert_eq!(
                parsed.is_ok(),
                expected,
                "command parser mismatch: args={args:?}"
            );
            if let Ok(command) = parsed {
                assert_eq!(command.len(), len);
            }
            Ok(())
        })
    }

    #[test]
    fn generated_command_budget_matches_reference_model() -> noprop::TestResult {
        test_support::run(0x434f_4d4d_4255_4447, 512, |ctx| {
            let count = noprop::sample_usize_in(ctx, 0..=MAX_COMMAND_ARGUMENTS + 8);
            let width = noprop::sample_usize_in(ctx, 0..=1024);
            let mut command = (0..count)
                .map(|index| {
                    if index == 0 {
                        "x".repeat(width.max(1))
                    } else {
                        "x".repeat(width)
                    }
                })
                .collect::<Vec<_>>();
            let mutation = noprop::sample_usize_in(ctx, 0..=4);
            if !command.is_empty() {
                match mutation {
                    1 => command[0].clear(),
                    2 => {
                        let index = noprop::sample_usize_in(ctx, 0..command.len());
                        command[index].push('\0');
                    }
                    3 => {
                        let index = noprop::sample_usize_in(ctx, 0..command.len());
                        command[index] = "x".repeat(MAX_COMMAND_ARGUMENT_BYTES + 1);
                    }
                    _ => {}
                }
            }
            let total = command
                .iter()
                .try_fold(0usize, |sum, argument| sum.checked_add(argument.len()));
            let expected = !command.is_empty()
                && !command[0].is_empty()
                && command.len() <= MAX_COMMAND_ARGUMENTS
                && command.iter().all(|argument| {
                    !argument.contains('\0') && argument.len() <= MAX_COMMAND_ARGUMENT_BYTES
                })
                && total.is_some_and(|bytes| bytes <= MAX_COMMAND_TOTAL_BYTES);
            let result = validate_command_budget(&command);
            assert_eq!(
                result.is_ok(),
                expected,
                "count={} width={width} mutation={mutation} total={total:?}",
                command.len()
            );
            Ok(())
        })
    }

    #[test]
    fn generated_string_arrays_require_array_of_strings() -> noprop::TestResult {
        test_support::run(0x5354_5241_5252_4159, test_support::DEFAULT_CASES, |ctx| {
            let len = noprop::sample_usize_in(ctx, 0..=8);
            let mut items = (0..len)
                .map(|_| Value::String(test_support::safe_component(ctx)))
                .collect::<Vec<_>>();
            let corrupt = !items.is_empty() && noprop::sample_bool(ctx);
            if corrupt {
                let index = noprop::sample_usize_in(ctx, 0..items.len());
                items[index] = Value::Bool(noprop::sample_bool(ctx));
            }
            let args = json!({"paths": items});
            let parsed = required_string_array(&args, "paths");
            assert_eq!(
                parsed.is_ok(),
                !corrupt,
                "string-array parser mismatch: args={args:?}"
            );
            if let Ok(paths) = parsed {
                assert_eq!(paths.len(), len);
            }
            Ok(())
        })
    }

    #[test]
    fn generated_job_ids_accept_uuid_strings_only() -> noprop::TestResult {
        test_support::run(0x4a4f_4249_4450_4254, test_support::DEFAULT_CASES, |ctx| {
            let upper = noprop::sample_u64(ctx) as u128;
            let lower = noprop::sample_u64(ctx) as u128;
            let uuid = Uuid::from_u128((upper << 64) | lower);
            let valid = noprop::sample_bool(ctx);
            let value = if valid {
                uuid.to_string()
            } else {
                format!("not-a-uuid-{}", test_support::safe_component(ctx))
            };
            let args = json!({"job_id": value});
            assert_eq!(
                required_job_id(&args).is_ok(),
                valid,
                "job id parser mismatch: args={args:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_cached_jobs_are_isolated_by_session() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        test_support::run(0x4a4f_424f_574e_4552, 256, |ctx| {
            let nonce = noprop::sample_u64(ctx);
            let owner_id = format!("job-owner-{nonce:x}");
            let other_id = format!("job-other-{nonce:x}");
            let cwd = std::env::current_dir().unwrap();
            let owner = config::Session {
                id: owner_id.clone(),
                cwd: cwd.clone(),
                permitted_directories: Vec::new(),
                started_at: 0,
                process_id: 0,
                permission_mode: config::PermissionMode::Yolo,
            };
            let other = config::Session {
                id: other_id,
                cwd,
                permitted_directories: Vec::new(),
                started_at: 0,
                process_id: 0,
                permission_mode: config::PermissionMode::Yolo,
            };
            let job_id = Uuid::new_v4();
            let completion = Arc::new(Mutex::new(JobCompletion {
                result: Some(cached_success("owned")),
                completed_at: Some(Instant::now()),
                ..JobCompletion::default()
            }));
            let handle = runtime.spawn(async {});
            jobs().lock().unwrap().jobs.insert(
                job_id,
                Job {
                    session_id: owner_id,
                    command: "test".to_owned(),
                    handle,
                    completion,
                    output_policy: OutputPolicy::default(),
                },
            );
            let args = json!({"job_id": job_id.to_string()});
            runtime.block_on(async {
                assert!(poll_job(&args, &other).await.is_err());
                assert_eq!(
                    poll_job(&args, &owner).await.unwrap()["content"][0]["text"],
                    "owned"
                );
            });
            remove_job(job_id);
            Ok(())
        })
    }

    #[test]
    fn generated_stop_job_cannot_cross_session_boundary() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        test_support::run(0x4a4f_4253_544f_5058, 128, |ctx| {
            let nonce = noprop::sample_u64(ctx);
            let owner_id = format!("stop-owner-{nonce:x}");
            let other_id = format!("stop-other-{nonce:x}");
            let cwd = std::env::current_dir().unwrap();
            let owner = config::Session {
                id: owner_id.clone(),
                cwd: cwd.clone(),
                permitted_directories: Vec::new(),
                started_at: 0,
                process_id: 0,
                permission_mode: config::PermissionMode::Yolo,
            };
            let other = config::Session {
                id: other_id,
                cwd,
                permitted_directories: Vec::new(),
                started_at: 0,
                process_id: 0,
                permission_mode: config::PermissionMode::Yolo,
            };
            let job_id = Uuid::new_v4();
            let completion = Arc::new(Mutex::new(JobCompletion::default()));
            let handle = runtime.spawn(async {
                std::future::pending::<()>().await;
            });
            jobs().lock().unwrap().jobs.insert(
                job_id,
                Job {
                    session_id: owner_id,
                    command: "test".to_owned(),
                    handle,
                    completion,
                    output_policy: OutputPolicy::default(),
                },
            );
            let args = json!({"job_id": job_id.to_string()});
            runtime.block_on(async {
                assert!(stop_job(&args, &other).await.is_err());
                assert!(jobs().lock().unwrap().jobs.contains_key(&job_id));
                assert!(stop_job(&args, &owner).await.is_ok());
                assert!(!jobs().lock().unwrap().jobs.contains_key(&job_id));
            });
            Ok(())
        })
    }

    #[test]
    fn generated_concurrent_poll_stop_is_linearizable() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        test_support::run(0x4a4f_4252_4143_4501, 64, |ctx| {
            let nonce = noprop::sample_u64(ctx);
            let session = config::Session {
                id: format!("poll-stop-{nonce:x}"),
                cwd: std::env::current_dir().unwrap(),
                permitted_directories: Vec::new(),
                started_at: 0,
                process_id: 0,
                permission_mode: config::PermissionMode::Yolo,
            };
            let job_id = Uuid::new_v4();
            let completion = Arc::new(Mutex::new(JobCompletion::default()));
            let handle = runtime.spawn(async { std::future::pending::<()>().await });
            jobs().lock().unwrap().jobs.insert(
                job_id,
                Job {
                    session_id: session.id.clone(),
                    command: "test".to_owned(),
                    handle,
                    completion,
                    output_policy: OutputPolicy::default(),
                },
            );

            runtime.block_on(async {
                let barrier = Arc::new(tokio::sync::Barrier::new(3));
                let poll_barrier = Arc::clone(&barrier);
                let stop_barrier = Arc::clone(&barrier);
                let poll_session = session.clone();
                let stop_session = session.clone();
                let poll_args = json!({"job_id": job_id.to_string()});
                let stop_args = poll_args.clone();

                let poll = tokio::spawn(async move {
                    poll_barrier.wait().await;
                    poll_job(&poll_args, &poll_session).await
                });
                let stop = tokio::spawn(async move {
                    stop_barrier.wait().await;
                    stop_job(&stop_args, &stop_session).await
                });
                barrier.wait().await;

                let poll_result = poll.await.unwrap();
                let stop_result = stop.await.unwrap();
                assert!(
                    stop_result.is_ok(),
                    "owner stop unexpectedly failed: {stop_result:?}"
                );
                match poll_result {
                    Ok(value) => {
                        let text = value["content"][0]["text"].as_str().unwrap_or_default();
                        assert!(
                            text.contains("\"status\":\"running\""),
                            "poll returned unexpected value: {value:?}"
                        );
                    }
                    Err(error) => {
                        assert!(
                            error.to_string().contains("unknown job_id"),
                            "poll returned non-linearizable error: {error:#}"
                        );
                    }
                }
                assert!(!jobs().lock().unwrap().jobs.contains_key(&job_id));
            });
            Ok(())
        })
    }

    #[test]
    fn generated_concurrent_stops_remove_job_exactly_once() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        test_support::run(0x4a4f_4253_544f_5002, 64, |ctx| {
            let nonce = noprop::sample_u64(ctx);
            let session = config::Session {
                id: format!("double-stop-{nonce:x}"),
                cwd: std::env::current_dir().unwrap(),
                permitted_directories: Vec::new(),
                started_at: 0,
                process_id: 0,
                permission_mode: config::PermissionMode::Yolo,
            };
            let job_id = Uuid::new_v4();
            let completion = Arc::new(Mutex::new(JobCompletion::default()));
            let handle = runtime.spawn(async { std::future::pending::<()>().await });
            jobs().lock().unwrap().jobs.insert(
                job_id,
                Job {
                    session_id: session.id.clone(),
                    command: "test".to_owned(),
                    handle,
                    completion,
                    output_policy: OutputPolicy::default(),
                },
            );

            runtime.block_on(async {
                let barrier = Arc::new(tokio::sync::Barrier::new(3));
                let args = json!({"job_id": job_id.to_string()});
                let mut tasks = Vec::new();
                for _ in 0..2 {
                    let barrier = Arc::clone(&barrier);
                    let session = session.clone();
                    let args = args.clone();
                    tasks.push(tokio::spawn(async move {
                        barrier.wait().await;
                        stop_job(&args, &session).await
                    }));
                }
                barrier.wait().await;
                let first = tasks.remove(0).await.unwrap();
                let second = tasks.remove(0).await.unwrap();
                assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
                let error = if first.is_err() {
                    first.err()
                } else {
                    second.err()
                }
                .unwrap();
                assert!(error.to_string().contains("unknown job_id"));
                assert!(!jobs().lock().unwrap().jobs.contains_key(&job_id));
            });
            Ok(())
        })
    }

    #[test]
    fn checkpoint_save_denial_keeps_disk_unchanged() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let store_path = state.path().join("checkpoints");
        let store = checkpoints::Store::new(store_path.clone());
        let session = config::Session {
            id: format!("checkpoint-denied-{}", Uuid::new_v4()),
            cwd: config::canonical_directory(cwd.path()).unwrap(),
            permitted_directories: vec![config::canonical_directory(cwd.path()).unwrap()],
            started_at: 1,
            process_id: 2,
            permission_mode: config::PermissionMode::Ask,
        };
        let marker = "denied-secret-sentinel";
        let request = checkpoints::parse_save_request(&json!({
            "session_id": session.id,
            "operation_id": Uuid::new_v4(),
            "expected_revision": 0,
            "checkpoint": {
                "title": marker,
                "base_commit": null,
                "steps": [{
                    "id": "step",
                    "description": marker,
                    "reported_status": "pending"
                }],
                "checks": [],
                "next_step_id": "step"
            }
        }))
        .unwrap();
        let safe_detail = checkpoints::approval_detail(&request.checkpoint);
        assert!(!safe_detail.contains(marker));
        let error = save_checkpoint_after_approval(&session, request, &store, false).unwrap_err();
        assert!(error.to_string().contains("user denied checkpoint save"));
        assert!(
            !store_path.exists(),
            "denied save created checkpoint storage"
        );
    }

    #[tokio::test]
    async fn job_list_is_session_scoped() {
        let owner = format!("job-list-owner-{}", Uuid::new_v4());
        let other = format!("job-list-other-{}", Uuid::new_v4());
        let owner_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();
        let marker_command = "command-sentinel-must-not-leak";
        let marker_output = "output-sentinel-must-not-leak";
        let owner_completion = Arc::new(Mutex::new(JobCompletion {
            result: Some(cached_success(marker_output)),
            completed_at: Some(Instant::now()),
            ..JobCompletion::default()
        }));
        let other_completion = Arc::new(Mutex::new(JobCompletion::default()));
        jobs().lock().unwrap().jobs.insert(
            owner_id,
            Job {
                session_id: owner.clone(),
                command: marker_command.to_owned(),
                handle: tokio::spawn(async {}),
                completion: owner_completion,
                output_policy: OutputPolicy::default(),
            },
        );
        jobs().lock().unwrap().jobs.insert(
            other_id,
            Job {
                session_id: other,
                command: "other-secret-command".to_owned(),
                handle: tokio::spawn(async { std::future::pending::<()>().await }),
                completion: other_completion,
                output_policy: OutputPolicy::default(),
            },
        );

        let snapshot = snapshot_jobs_for_session(&owner, 50);
        assert_eq!(snapshot.jobs.len(), 1);
        assert_eq!(snapshot.jobs[0].job_id, owner_id.to_string());
        assert_eq!(snapshot.jobs[0].status, "completed");
        let rendered = serde_json::to_string(&snapshot).unwrap();
        assert!(!rendered.contains(marker_command));
        assert!(!rendered.contains(marker_output));
        assert!(!rendered.contains("other-secret-command"));

        if let Some(job) = remove_job(owner_id) {
            job.handle.abort();
        }
        if let Some(job) = remove_job(other_id) {
            job.handle.abort();
        }
    }

    #[tokio::test]
    async fn job_list_redacts_command_and_output() {
        let session_id = format!("job-list-redact-{}", Uuid::new_v4());
        let success_id = Uuid::new_v4();
        let failure_id = Uuid::new_v4();
        let command_sentinel = "command-secret-sentinel";
        let success_sentinel = "success-secret-sentinel";
        let failure_sentinel = "failure-secret-sentinel";
        for (job_id, result, command) in [
            (
                success_id,
                cached_success(success_sentinel),
                command_sentinel,
            ),
            (
                failure_id,
                cached_error(failure_sentinel),
                "failure-command-secret-sentinel",
            ),
        ] {
            let completion = Arc::new(Mutex::new(JobCompletion {
                result: Some(result),
                completed_at: Some(Instant::now()),
                ..JobCompletion::default()
            }));
            jobs().lock().unwrap().jobs.insert(
                job_id,
                Job {
                    session_id: session_id.clone(),
                    command: command.to_owned(),
                    handle: tokio::spawn(async {}),
                    completion,
                    output_policy: OutputPolicy::default(),
                },
            );
        }
        let rendered = serde_json::to_string(&snapshot_jobs_for_session(&session_id, 50)).unwrap();
        for sentinel in [
            command_sentinel,
            success_sentinel,
            failure_sentinel,
            "failure-command-secret-sentinel",
        ] {
            assert!(!rendered.contains(sentinel), "leaked {sentinel}");
        }
        assert!(rendered.contains("completed"));
        assert!(rendered.contains("failed"));
        for job_id in [success_id, failure_id] {
            if let Some(job) = remove_job(job_id) {
                job.handle.abort();
            }
        }
    }

    #[tokio::test]
    async fn job_list_does_not_consume_completion() {
        let session_id = format!("job-list-repeat-{}", Uuid::new_v4());
        let job_id = Uuid::new_v4();
        let completion = Arc::new(Mutex::new(JobCompletion {
            result: Some(cached_success("still-cached")),
            completed_at: Some(Instant::now()),
            ..JobCompletion::default()
        }));
        jobs().lock().unwrap().jobs.insert(
            job_id,
            Job {
                session_id: session_id.clone(),
                command: "hidden".to_owned(),
                handle: tokio::spawn(async {}),
                completion,
                output_policy: OutputPolicy::default(),
            },
        );

        let first = snapshot_jobs_for_session(&session_id, 50);
        let second = snapshot_jobs_for_session(&session_id, 50);
        assert_eq!(first, second);
        assert!(matches!(
            inspect_job(job_id, &session_id).unwrap(),
            JobPollSnapshot::Completed(CachedJobResult::Success { .. }, _)
        ));
        if let Some(job) = remove_job(job_id) {
            job.handle.abort();
        }
    }

    #[tokio::test]
    async fn job_list_orders_running_first_and_reports_truncation() {
        let session_id = format!("job-list-order-{}", Uuid::new_v4());
        let ids = [
            Uuid::from_u128(4),
            Uuid::from_u128(2),
            Uuid::from_u128(3),
            Uuid::from_u128(1),
        ];
        for (index, job_id) in ids.iter().copied().enumerate() {
            let running = index < 2;
            let completion = Arc::new(Mutex::new(if running {
                JobCompletion::default()
            } else {
                JobCompletion {
                    result: Some(cached_success("hidden")),
                    completed_at: Some(Instant::now()),
                    ..JobCompletion::default()
                }
            }));
            let handle = if running {
                tokio::spawn(async { std::future::pending::<()>().await })
            } else {
                tokio::spawn(async {})
            };
            jobs().lock().unwrap().jobs.insert(
                job_id,
                Job {
                    session_id: session_id.clone(),
                    command: "hidden".to_owned(),
                    handle,
                    completion,
                    output_policy: OutputPolicy::default(),
                },
            );
        }
        let snapshot = snapshot_jobs_for_session(&session_id, 2);
        assert!(snapshot.truncated);
        assert_eq!(
            snapshot
                .jobs
                .iter()
                .map(|job| job.job_id.clone())
                .collect::<Vec<_>>(),
            vec![
                Uuid::from_u128(2).to_string(),
                Uuid::from_u128(4).to_string()
            ]
        );
        assert!(snapshot.jobs.iter().all(|job| job.status == "running"));
        for job_id in ids {
            if let Some(job) = remove_job(job_id) {
                job.handle.abort();
            }
        }
    }

    #[tokio::test]
    async fn job_list_reports_unknown_for_finished_handle_without_result() {
        let session_id = format!("job-list-unknown-{}", Uuid::new_v4());
        let job_id = Uuid::new_v4();
        let handle = tokio::spawn(async {});
        tokio::task::yield_now().await;
        jobs().lock().unwrap().jobs.insert(
            job_id,
            Job {
                session_id: session_id.clone(),
                command: "hidden".to_owned(),
                handle,
                completion: Arc::new(Mutex::new(JobCompletion::default())),
                output_policy: OutputPolicy::default(),
            },
        );
        let snapshot = snapshot_jobs_for_session(&session_id, 50);
        assert_eq!(snapshot.jobs[0].status, "unknown");
        if let Some(job) = remove_job(job_id) {
            job.handle.abort();
        }
    }

    #[test]
    fn job_list_orders_running_first_and_validates_limit_and_fields() {
        let cwd = std::env::current_dir().unwrap();
        let session = config::Session {
            id: format!("job-list-args-{}", Uuid::new_v4()),
            cwd,
            permitted_directories: Vec::new(),
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Yolo,
        };
        assert!(job_list(&json!({"session_id":session.id,"limit":1}), &session).is_ok());
        assert!(job_list(&json!({"session_id":session.id,"limit":128}), &session).is_ok());
        assert!(job_list(&json!({"session_id":session.id,"limit":0}), &session).is_err());
        assert!(job_list(&json!({"session_id":session.id,"limit":129}), &session).is_err());
        assert!(
            job_list(
                &json!({"session_id":session.id,"limit":50,"unknown":true}),
                &session
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn job_list_after_new_chat_discovers_existing_running_job() {
        let session_id = format!("job-list-new-chat-{}", Uuid::new_v4());
        let job_id = Uuid::new_v4();
        let handle = tokio::spawn(async { std::future::pending::<()>().await });
        jobs().lock().unwrap().jobs.insert(
            job_id,
            Job {
                session_id: session_id.clone(),
                command: "hidden".to_owned(),
                handle,
                completion: Arc::new(Mutex::new(JobCompletion::default())),
                output_policy: OutputPolicy::default(),
            },
        );

        let first_client = snapshot_jobs_for_session(&session_id, 50);
        let second_client = snapshot_jobs_for_session(&session_id, 50);
        assert_eq!(first_client.jobs.len(), 1);
        assert_eq!(first_client, second_client);
        assert_eq!(first_client.jobs[0].job_id, job_id.to_string());
        assert_eq!(first_client.jobs[0].status, "running");

        if let Some(job) = remove_job(job_id) {
            job.handle.abort();
        }
    }

    #[test]
    fn job_list_empty_is_not_execution_history() {
        let session_id = format!("job-list-empty-{}", Uuid::new_v4());
        let snapshot = snapshot_jobs_for_session(&session_id, 50);
        assert!(snapshot.jobs.is_empty());
        assert!(!snapshot.truncated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn yolo_command_bypasses_sandbox_file_roots() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let marker = outside.path().join("yolo-marker");
        let command = vec!["/usr/bin/touch".to_owned(), marker.display().to_string()];

        let output = run_session_command(
            &command,
            workspace.path(),
            &[workspace.path().to_path_buf()],
            config::PermissionMode::Yolo,
        )
        .await
        .unwrap();

        assert_eq!(output.status, 0, "{}", output.stderr);
        assert!(marker.is_file());
    }

    #[tokio::test]
    async fn get_image_returns_mcp_image_content() {
        let path = std::env::temp_dir().join(format!("temote-mcp-{}.png", uuid::Uuid::new_v4()));
        let bytes = b"\x89PNG\r\n\x1a\nexample";
        tokio::fs::write(&path, bytes).await.unwrap();

        let result = get_image(&path).await.unwrap();
        tokio::fs::remove_file(path).await.unwrap();

        assert_eq!(result["content"][0]["type"], "image");
        assert_eq!(result["content"][0]["mimeType"], "image/png");
        assert_eq!(result["content"][0]["data"], STANDARD.encode(bytes));
    }

    #[tokio::test]
    async fn get_image_resolves_relative_paths_from_session_cwd() {
        let directory = std::env::temp_dir().join(format!("temote-mcp-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir(&directory).await.unwrap();
        let path = directory.join("image.gif");
        tokio::fs::write(&path, b"GIF89a").await.unwrap();

        let result = get_image(&directory.join("image.gif")).await.unwrap();
        tokio::fs::remove_dir_all(directory).await.unwrap();

        assert_eq!(result["content"][0]["mimeType"], "image/gif");
    }

    #[tokio::test]
    async fn completed_job_result_can_be_polled_repeatedly() {
        let session_id = format!("test-job-cache-{}", Uuid::new_v4());
        let session = config::Session {
            id: session_id.clone(),
            cwd: std::env::current_dir().unwrap(),
            permitted_directories: Vec::new(),
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Yolo,
        };
        let job_id = Uuid::new_v4();
        let completion = Arc::new(Mutex::new(JobCompletion {
            result: Some(cached_success("cached-result")),
            completed_at: Some(Instant::now()),
            ..JobCompletion::default()
        }));
        let handle = tokio::spawn(async {});
        jobs().lock().unwrap().jobs.insert(
            job_id,
            Job {
                session_id,
                command: "test".to_owned(),
                handle,
                completion,
                output_policy: OutputPolicy::default(),
            },
        );
        let args = json!({"job_id": job_id.to_string()});

        let first = poll_job(&args, &session).await.unwrap();
        let second = poll_job(&args, &session).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(first["content"][0]["text"], "cached-result");
        remove_job(job_id);
    }

    #[tokio::test]
    async fn default_running_job_poll_preserves_legacy_response_shape() {
        let session_id = format!("test-job-running-default-{}", Uuid::new_v4());
        let session = config::Session {
            id: session_id.clone(),
            cwd: std::env::current_dir().unwrap(),
            permitted_directories: Vec::new(),
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Yolo,
        };
        let job_id = Uuid::new_v4();
        let handle = tokio::spawn(async { std::future::pending::<()>().await });
        jobs().lock().unwrap().jobs.insert(
            job_id,
            Job {
                session_id,
                command: "test".to_owned(),
                handle,
                completion: Arc::new(Mutex::new(JobCompletion::default())),
                output_policy: OutputPolicy::default(),
            },
        );

        let result = poll_job(&json!({"job_id": job_id.to_string()}), &session)
            .await
            .unwrap();
        assert_eq!(
            result["content"][0]["text"],
            json!({"status":"running","job_id":job_id}).to_string()
        );
        remove_job(job_id);
    }

    #[tokio::test]
    async fn explicit_running_job_poll_policy_is_opt_in() {
        let session_id = format!("test-job-running-extended-{}", Uuid::new_v4());
        let session = config::Session {
            id: session_id.clone(),
            cwd: std::env::current_dir().unwrap(),
            permitted_directories: Vec::new(),
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Yolo,
        };
        let job_id = Uuid::new_v4();
        let handle = tokio::spawn(async { std::future::pending::<()>().await });
        jobs().lock().unwrap().jobs.insert(
            job_id,
            Job {
                session_id,
                command: "test".to_owned(),
                handle,
                completion: Arc::new(Mutex::new(JobCompletion::default())),
                output_policy: OutputPolicy::default(),
            },
        );

        let result = poll_job(
            &json!({"job_id": job_id.to_string(), "status_only": true}),
            &session,
        )
        .await
        .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["status"], "running");
        assert_eq!(value["status_only"], true);
        assert!(value.get("output_limit_bytes").is_some());
        remove_job(job_id);
    }

    #[test]
    fn completed_jobs_release_their_active_slot_independently_of_cache_retention() {
        let session_id = format!("test-job-slot-{}", Uuid::new_v4());
        let slot = reserve_job_slot(&session_id).unwrap();
        assert_eq!(
            jobs()
                .lock()
                .unwrap()
                .active_by_session
                .get(&session_id)
                .copied(),
            Some(1)
        );

        drop(slot);

        assert!(
            !jobs()
                .lock()
                .unwrap()
                .active_by_session
                .contains_key(&session_id)
        );
    }

    #[test]
    fn generated_job_slot_sequences_respect_per_session_capacity() -> noprop::TestResult {
        test_support::run(0x4a4f_4253_4c4f_5401, 256, |ctx| {
            let session_id = format!("pbt-job-slot-{}", noprop::sample_u64(ctx));
            let attempts = noprop::sample_usize_in(ctx, 0..=MAX_ACTIVE_JOBS_PER_SESSION + 4);
            let mut slots = Vec::new();

            for attempt in 0..attempts {
                match reserve_job_slot(&session_id) {
                    Ok(slot) => {
                        assert!(
                            attempt < MAX_ACTIVE_JOBS_PER_SESSION,
                            "slot above capacity was accepted: attempt={attempt}"
                        );
                        slots.push(slot);
                    }
                    Err(_) => {
                        assert!(
                            attempt >= MAX_ACTIVE_JOBS_PER_SESSION,
                            "slot below capacity was rejected: attempt={attempt}"
                        );
                    }
                }
            }

            let expected_active = attempts.min(MAX_ACTIVE_JOBS_PER_SESSION);
            let actual_active = jobs()
                .lock()
                .unwrap()
                .active_by_session
                .get(&session_id)
                .copied()
                .unwrap_or_default();
            assert_eq!(actual_active, expected_active);

            let releases = noprop::sample_usize_in(ctx, 0..=slots.len());
            for _ in 0..releases {
                slots.pop();
            }
            let remaining = expected_active - releases;
            let actual_remaining = jobs()
                .lock()
                .unwrap()
                .active_by_session
                .get(&session_id)
                .copied()
                .unwrap_or_default();
            assert_eq!(actual_remaining, remaining);

            drop(slots);
            assert!(
                !jobs()
                    .lock()
                    .unwrap()
                    .active_by_session
                    .contains_key(&session_id),
                "dropping all slots did not clear the session counter"
            );
            Ok(())
        })
    }

    #[test]
    fn foreground_job_admission_is_visible_to_worktree_ownership_snapshot() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("managed-task");
        std::fs::create_dir(&target).unwrap();
        let target = std::fs::canonicalize(target).unwrap();
        let session = config::Session {
            id: format!("foreground-owner-{}", Uuid::new_v4()),
            cwd: fixture.path().to_path_buf(),
            permitted_directories: Vec::new(),
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Yolo,
        };
        let reservation = managed_worktree::acquire_worktree_reservation(&target).unwrap();
        let slot = reserve_job_slot_with_admission(
            &session.id,
            &target,
            Some(target.clone()),
            None,
            Some(reservation),
        )
        .unwrap();
        let jobs = snapshot_active_job_ownerships();
        let ownership = managed_worktree_owners_from(&session, &target, &[], &jobs);
        assert_eq!(ownership.owning_jobs.len(), 1);
        assert!(!ownership.owning_jobs[0].is_empty());
        drop(slot);
        assert!(snapshot_active_job_ownerships().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn job_admission_waits_for_cleanup_reservation_but_not_unrelated_worktree() {
        let (_root, canonical_root, _checkout, _session) = managed_worktree_removal_fixture();
        let target = canonical_root.join("worktrees/repo/feature-remove-me");
        let unrelated = canonical_root.join("worktrees/repo/feature-sibling");
        let target_root = sandbox::git_worktree_root(&target).unwrap();
        let unrelated_root = sandbox::git_worktree_root(&unrelated).unwrap();
        assert_ne!(target_root, unrelated_root);

        let cleanup_reservation =
            managed_worktree::acquire_worktree_reservation(&target_root).unwrap();
        let session_id = format!("admission-target-{}", Uuid::new_v4());
        let mut blocked = Box::pin(reserve_job_slot_for_cwd(&session_id, &target));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut blocked)
                .await
                .is_err(),
            "same-worktree job admission must wait for cleanup"
        );

        let unrelated_id = format!("admission-unrelated-{}", Uuid::new_v4());
        let unrelated_slot = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reserve_job_slot_for_cwd(&unrelated_id, &unrelated),
        )
        .await
        .expect("unrelated worktree admission must not wait for cleanup")
        .unwrap();
        drop(unrelated_slot);

        drop(cleanup_reservation);
        let slot = tokio::time::timeout(std::time::Duration::from_secs(2), &mut blocked)
            .await
            .expect("same-worktree admission must complete after cleanup releases")
            .unwrap();
        let owners = snapshot_active_job_ownerships();
        assert!(owners.iter().any(|(_, owner)| owner == &target_root));
        drop(slot);
        assert!(
            !snapshot_active_job_ownerships()
                .iter()
                .any(|(_, owner)| owner == &target_root)
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn shared_job_admissions_exclude_cross_process_cleanup_until_all_drop() {
        let (_root, canonical_root, _checkout, _session) = managed_worktree_removal_fixture();
        let target = canonical_root.join("worktrees/repo/feature-remove-me");
        let unrelated = canonical_root.join("worktrees/repo/feature-sibling");
        let target_root = sandbox::git_worktree_root(&target).unwrap();
        let unrelated_root = sandbox::git_worktree_root(&unrelated).unwrap();
        assert_ne!(target_root, unrelated_root);

        let first_id = format!("shared-job-first-{}", Uuid::new_v4());
        let first = reserve_job_slot_for_cwd(&first_id, &target).await.unwrap();
        let second_id = format!("shared-job-second-{}", Uuid::new_v4());
        let second = reserve_job_slot_for_cwd(&second_id, &target).await.unwrap();
        assert!(
            managed_worktree::try_acquire_worktree_reservation_async(&target_root)
                .await
                .is_err(),
            "exclusive cleanup must fail while a shared job reservation is held"
        );

        drop(first);
        assert!(
            managed_worktree::try_acquire_worktree_reservation_async(&target_root)
                .await
                .is_err(),
            "the remaining shared job reservation must exclude cleanup"
        );
        drop(second);

        let cleanup = managed_worktree::try_acquire_worktree_reservation_async(&target_root)
            .await
            .unwrap();
        let blocked_id = format!("shared-job-blocked-{}", Uuid::new_v4());
        let mut blocked = Box::pin(reserve_job_slot_for_cwd(&blocked_id, &target));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut blocked)
                .await
                .is_err(),
            "new same-worktree admission must wait while cleanup owns the target"
        );

        let unrelated_id = format!("shared-job-unrelated-{}", Uuid::new_v4());
        let unrelated_slot = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reserve_job_slot_for_cwd(&unrelated_id, &unrelated),
        )
        .await
        .expect("unrelated admission must not wait for target cleanup")
        .unwrap();
        drop(unrelated_slot);

        drop(cleanup);
        let admitted = tokio::time::timeout(std::time::Duration::from_secs(2), &mut blocked)
            .await
            .expect("same-worktree admission must complete after cleanup release")
            .unwrap();
        drop(admitted);
    }

    #[test]
    fn generated_completed_job_cache_stays_bounded_and_preserves_active_jobs() -> noprop::TestResult
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        test_support::run(0x4a4f_4243_4143_4845, 128, |ctx| {
            let per_session_limit = noprop::sample_usize_in(ctx, 1..=8);
            let total_limit = noprop::sample_usize_in(ctx, per_session_limit..=24);
            let session_count = noprop::sample_usize_in(ctx, 1..=5);
            let operation_count = noprop::sample_usize_in(ctx, 1..=48);
            let now = Instant::now();
            let campaign = noprop::sample_u64(ctx);
            let mut state = JobState {
                jobs: HashMap::new(),
                active_by_session: HashMap::new(),
                active_admissions: HashMap::new(),
            };
            let mut active_ids = std::collections::HashSet::new();

            for step in 0..operation_count {
                let session_id = format!(
                    "cache-pbt-{campaign}-{}",
                    noprop::sample_usize_in(ctx, 0..session_count)
                );
                let job_id = Uuid::new_v4();
                let active = noprop::sample_usize_in(ctx, 0..4) == 0;
                let completion = Arc::new(Mutex::new(if active {
                    JobCompletion::default()
                } else {
                    JobCompletion {
                        result: Some(cached_success(format!("done-{step}"))),
                        completed_at: Some(now + Duration::from_nanos(step as u64 + 1)),
                        ..JobCompletion::default()
                    }
                }));
                let handle = if active {
                    active_ids.insert(job_id);
                    runtime.spawn(async { std::future::pending::<()>().await })
                } else {
                    runtime.spawn(async {})
                };
                state.jobs.insert(
                    job_id,
                    Job {
                        session_id,
                        command: "test".to_owned(),
                        handle,
                        completion,
                        output_policy: OutputPolicy::default(),
                    },
                );
                reap_jobs_with_limits(&mut state, now, per_session_limit, total_limit);

                assert!(
                    active_ids
                        .iter()
                        .all(|job_id| state.jobs.contains_key(job_id)),
                    "active job was evicted"
                );
                let completed = state
                    .jobs
                    .values()
                    .filter(|job| job.completion.lock().unwrap().completed_at.is_some())
                    .collect::<Vec<_>>();
                assert!(completed.len() <= total_limit);
                let mut per_session = HashMap::<&str, usize>::new();
                for job in completed {
                    *per_session.entry(job.session_id.as_str()).or_default() += 1;
                }
                assert!(
                    per_session
                        .values()
                        .all(|count| *count <= per_session_limit),
                    "per-session completed cache exceeded limit"
                );
            }

            for job in state.jobs.values() {
                job.handle.abort();
            }
            Ok(())
        })
    }

    #[tokio::test]
    async fn completed_job_cache_is_reaped_only_after_ttl() {
        let session_id = format!("test-job-ttl-{}", Uuid::new_v4());
        let job_id = Uuid::new_v4();
        let completion = Arc::new(Mutex::new(JobCompletion {
            result: Some(cached_success("expired")),
            completed_at: Some(Instant::now() - COMPLETED_JOB_TTL - Duration::from_secs(1)),
            ..JobCompletion::default()
        }));
        let handle = tokio::spawn(async {});
        jobs().lock().unwrap().jobs.insert(
            job_id,
            Job {
                session_id,
                command: "test".to_owned(),
                handle,
                completion,
                output_policy: OutputPolicy::default(),
            },
        );

        reap_jobs();

        assert!(!jobs().lock().unwrap().jobs.contains_key(&job_id));
    }

    fn run_git_fixture(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", cwd.display());
    }

    fn run_git_command_fixture(cwd: &Path, command: &[String]) -> std::process::Output {
        assert_eq!(command.first().map(String::as_str), Some("git"));
        std::process::Command::new("git")
            .args(&command[1..])
            .current_dir(cwd)
            .output()
            .unwrap()
    }

    fn git_fixture_stdout(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn git_push_tag_validation_and_command_are_exact_and_lease_bound() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        validate_git_object_id(sha1, "source_sha").unwrap();
        validate_git_object_id(sha256, "source_sha").unwrap();
        for invalid in [
            "",
            "abc",
            "g123456789012345678901234567890123456789",
            &"a".repeat(41),
        ] {
            assert!(validate_git_object_id(invalid, "source_sha").is_err());
        }
        for valid in ["latest", "release/2026.09", "v1.2.3"] {
            validate_git_tag_name(valid).unwrap();
        }
        for invalid in ["", "-latest", "refs/tags/latest", "bad\ntag"] {
            assert!(validate_git_tag_name(invalid).is_err());
        }

        let create = build_git_push_tag_command("origin", "latest", sha1, None);
        assert_eq!(
            create,
            [
                "git",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "push.recurseSubmodules=off",
                "push",
                "--force-with-lease=refs/tags/latest:",
                "origin",
                "0123456789abcdef0123456789abcdef01234567:refs/tags/latest",
            ]
        );
        assert!(!create.iter().any(|argument| argument == "--force"));

        let update = build_git_push_tag_command("upstream", "latest", sha256, Some(sha1));
        assert_eq!(
            update[6],
            format!("--force-with-lease=refs/tags/latest:{sha1}")
        );
        assert_eq!(update[7], "upstream");
        assert_eq!(update[8], format!("{sha256}:refs/tags/latest"));
    }

    #[test]
    fn generated_git_push_tag_commands_never_escape_tag_namespace() -> noprop::TestResult {
        test_support::run(0x5441_4750_5553_484c, 1024, |ctx| {
            let tag = format!("release-{:016x}", noprop::sample_u64(ctx));
            let source = format!("{:040x}", noprop::sample_u64(ctx));
            let expected = format!("{:040x}", noprop::sample_u64(ctx));
            validate_git_tag_name(&tag).unwrap();
            validate_git_object_id(&source, "source_sha").unwrap();
            validate_git_object_id(&expected, "expected_remote_sha").unwrap();
            let command = build_git_push_tag_command("origin", &tag, &source, Some(&expected));
            let tag_ref = format!("refs/tags/{tag}");
            assert_eq!(
                command[6],
                format!("--force-with-lease={tag_ref}:{expected}")
            );
            assert_eq!(command[7], "origin");
            assert_eq!(command[8], format!("{source}:{tag_ref}"));
            assert!(!command.iter().any(|argument| argument == "--force"));
            Ok(())
        })
    }

    #[test]
    fn structured_git_branch_and_worktree_commands_are_force_free_and_repo_owned() {
        let repository = tempfile::tempdir().unwrap();
        let base_sha = "0123456789abcdef0123456789abcdef01234567";
        for base in [
            "HEAD",
            "main",
            "origin/main",
            "refs/remotes/origin/main",
            base_sha,
        ] {
            validate_git_base_ref(base).unwrap();
        }
        for base in ["", "-main", "HEAD~1", "main^{commit}", "main@{1}", "a..b"] {
            assert!(validate_git_base_ref(base).is_err(), "{base}");
        }
        for name in ["feature", "task-123", "review_1", "r1.2"] {
            validate_git_worktree_name(name).unwrap();
        }
        for name in ["", ".", "..", "-bad", "../bad", "nested/bad", "bad\nname"] {
            assert!(validate_git_worktree_name(name).is_err(), "{name:?}");
        }

        let destination = git_worktree_destination(repository.path(), "feature").unwrap();
        assert_eq!(destination, repository.path().join(".wt/feature"));

        let branch = build_git_branch_create_command("feature", base_sha);
        let switch = build_git_switch_command("feature");
        let create = build_git_worktree_add_create_command(&destination, "feature", base_sha);
        let existing = build_git_worktree_add_existing_command(&destination, "feature");
        for command in [&branch, &switch, &create, &existing] {
            assert_eq!(command.first().map(String::as_str), Some("git"));
            for forbidden in ["--force", "-f", "-B", "reset", "stash"] {
                assert!(
                    !command.iter().any(|argument| argument == forbidden),
                    "forbidden argument {forbidden} in {command:?}"
                );
            }
        }
        assert_eq!(branch[3], "branch");
        assert_eq!(branch[4], "--no-track");
        assert_eq!(switch[3], "switch");
        assert_eq!(switch[4], "--no-guess");
        assert_eq!(create[3], "worktree");
        assert_eq!(create[4], "add");
        assert_eq!(create[5], "-b");
        assert_eq!(existing[5], destination.to_string_lossy());
        assert_eq!(existing[6], "feature");
    }

    #[test]
    fn generated_git_worktree_destinations_stay_under_repo_owned_root() -> noprop::TestResult {
        test_support::run(0x4757_4f52_4b54_5245, 1024, |ctx| {
            let repository =
                PathBuf::from(format!("/tmp/repository-{:016x}", noprop::sample_u64(ctx)));
            let name = format!("task-{:016x}", noprop::sample_u64(ctx));
            let branch = format!("feature/{:016x}", noprop::sample_u64(ctx));
            validate_git_worktree_name(&name).unwrap();
            let destination = git_worktree_destination(&repository, &name).unwrap();
            assert!(destination.starts_with(repository.join(".wt")));
            let command = build_git_worktree_add_existing_command(&destination, &branch);
            assert_eq!(command[5], destination.to_string_lossy());
            assert_eq!(command[6], branch);
            assert!(!command.iter().any(|argument| argument == "--force"));
            Ok(())
        })
    }

    #[test]
    fn structured_git_commands_preserve_dirty_worktree_and_create_linked_worktree() {
        let repository = tempfile::tempdir().unwrap();
        run_git_fixture(repository.path(), &["init", "--quiet"]);
        run_git_fixture(repository.path(), &["config", "user.name", "Temote Test"]);
        run_git_fixture(
            repository.path(),
            &["config", "user.email", "temote-test@example.invalid"],
        );
        std::fs::write(repository.path().join("tracked.txt"), "base\n").unwrap();
        run_git_fixture(repository.path(), &["add", "tracked.txt"]);
        run_git_fixture(repository.path(), &["commit", "--quiet", "-m", "initial"]);
        run_git_fixture(repository.path(), &["branch", "-M", "main"]);
        let base_sha = git_fixture_stdout(repository.path(), &["rev-parse", "HEAD"]);

        std::fs::write(repository.path().join("untracked.txt"), "keep me\n").unwrap();
        let create_branch = build_git_branch_create_command("feature", &base_sha);
        let output = run_git_command_fixture(repository.path(), &create_branch);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            git_fixture_stdout(repository.path(), &["branch", "--show-current"]),
            "main"
        );
        assert_eq!(
            std::fs::read_to_string(repository.path().join("untracked.txt")).unwrap(),
            "keep me\n"
        );

        let switch = build_git_switch_command("feature");
        let output = run_git_command_fixture(repository.path(), &switch);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            git_fixture_stdout(repository.path(), &["branch", "--show-current"]),
            "feature"
        );
        assert_eq!(
            std::fs::read_to_string(repository.path().join("untracked.txt")).unwrap(),
            "keep me\n"
        );

        run_git_fixture(repository.path(), &["switch", "--quiet", "main"]);
        let worktree_root = repository.path().join(".wt");
        std::fs::create_dir(&worktree_root).unwrap();
        let destination = worktree_root.join("review");
        let add = build_git_worktree_add_create_command(&destination, "review", &base_sha);
        let output = run_git_command_fixture(repository.path(), &add);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            git_fixture_stdout(&destination, &["branch", "--show-current"]),
            "review"
        );
        assert_eq!(
            git_fixture_stdout(&destination, &["rev-parse", "HEAD"]),
            base_sha
        );
        assert_eq!(
            std::fs::read_to_string(repository.path().join("untracked.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn structured_git_switch_refuses_conflicting_dirty_change_without_discarding_it() {
        let repository = tempfile::tempdir().unwrap();
        run_git_fixture(repository.path(), &["init", "--quiet"]);
        run_git_fixture(repository.path(), &["config", "user.name", "Temote Test"]);
        run_git_fixture(
            repository.path(),
            &["config", "user.email", "temote-test@example.invalid"],
        );
        std::fs::write(repository.path().join("tracked.txt"), "base\n").unwrap();
        run_git_fixture(repository.path(), &["add", "tracked.txt"]);
        run_git_fixture(repository.path(), &["commit", "--quiet", "-m", "initial"]);
        run_git_fixture(repository.path(), &["branch", "-M", "main"]);
        run_git_fixture(repository.path(), &["switch", "--quiet", "-c", "feature"]);
        std::fs::write(repository.path().join("tracked.txt"), "feature\n").unwrap();
        run_git_fixture(repository.path(), &["add", "tracked.txt"]);
        run_git_fixture(repository.path(), &["commit", "--quiet", "-m", "feature"]);
        run_git_fixture(repository.path(), &["switch", "--quiet", "main"]);

        std::fs::write(repository.path().join("tracked.txt"), "dirty-main\n").unwrap();
        let command = build_git_switch_command("feature");
        let output = run_git_command_fixture(repository.path(), &command);
        assert!(!output.status.success());
        assert_eq!(
            git_fixture_stdout(repository.path(), &["branch", "--show-current"]),
            "main"
        );
        assert_eq!(
            std::fs::read_to_string(repository.path().join("tracked.txt")).unwrap(),
            "dirty-main\n"
        );
    }

    #[test]
    fn structured_git_worktree_can_attach_an_existing_local_branch() {
        let repository = tempfile::tempdir().unwrap();
        run_git_fixture(repository.path(), &["init", "--quiet"]);
        run_git_fixture(repository.path(), &["config", "user.name", "Temote Test"]);
        run_git_fixture(
            repository.path(),
            &["config", "user.email", "temote-test@example.invalid"],
        );
        std::fs::write(repository.path().join("tracked.txt"), "base\n").unwrap();
        run_git_fixture(repository.path(), &["add", "tracked.txt"]);
        run_git_fixture(repository.path(), &["commit", "--quiet", "-m", "initial"]);
        run_git_fixture(repository.path(), &["branch", "-M", "main"]);
        run_git_fixture(repository.path(), &["branch", "review"]);

        let worktree_root = repository.path().join(".wt");
        std::fs::create_dir(&worktree_root).unwrap();
        let destination = worktree_root.join("review");
        let command = build_git_worktree_add_existing_command(&destination, "review");
        let output = run_git_command_fixture(repository.path(), &command);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            git_fixture_stdout(&destination, &["branch", "--show-current"]),
            "review"
        );
        assert_eq!(
            git_fixture_stdout(&destination, &["rev-parse", "HEAD"]),
            git_fixture_stdout(repository.path(), &["rev-parse", "main"])
        );
    }

    fn init_git_repository(path: &Path) {
        run_git_fixture(path, &["init", "--quiet"]);
        run_git_fixture(path, &["config", "user.name", "Temote Test"]);
        run_git_fixture(
            path,
            &["config", "user.email", "temote-test@example.invalid"],
        );
        std::fs::write(path.join("tracked.txt"), "base\n").unwrap();
        run_git_fixture(path, &["add", "tracked.txt"]);
        run_git_fixture(path, &["commit", "--quiet", "-m", "initial"]);
        run_git_fixture(path, &["branch", "-M", "main"]);
    }

    fn managed_worktree_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, config::Session) {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let checkout = canonical_root.join("repo");
        std::fs::create_dir(&checkout).unwrap();
        init_git_repository(&checkout);
        std::fs::create_dir(checkout.join(".wt")).unwrap();
        run_git_fixture(
            &checkout,
            &[
                "worktree",
                "add",
                "--quiet",
                ".wt/legacy",
                "-b",
                "legacy-branch",
            ],
        );
        let sibling = canonical_root.join("repo-legacy-linked");
        run_git_fixture(
            &checkout,
            &[
                "worktree",
                "add",
                "--quiet",
                sibling.to_str().unwrap(),
                "-b",
                "sibling-branch",
            ],
        );
        std::fs::write(checkout.join("untracked.txt"), "keep me\n").unwrap();
        let checkout = std::fs::canonicalize(&checkout).unwrap();
        let session = config::Session {
            id: format!("managed-worktree-{}", Uuid::new_v4()),
            cwd: checkout.clone(),
            permitted_directories: vec![checkout.clone()],
            started_at: 1,
            process_id: 1,
            permission_mode: config::PermissionMode::Agent,
        };
        (root, canonical_root, checkout, session)
    }

    fn worktree_snapshot(path: &Path) -> (String, String, String) {
        (
            git_fixture_stdout(path, &["branch", "--show-current"]),
            git_fixture_stdout(path, &["rev-parse", "HEAD"]),
            git_fixture_stdout(path, &["status", "--porcelain"]),
        )
    }

    fn result_text(result: &Value) -> Value {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn managed_worktree_list_value(value: &Value, path: &Path) -> Option<String> {
        let canonical = std::fs::canonicalize(path).unwrap();
        value["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| {
                std::fs::canonicalize(entry["path"].as_str().unwrap()).unwrap() == canonical
            })
            .map(|entry| entry["classification"].as_str().unwrap().to_owned())
    }

    #[tokio::test]
    async fn managed_worktree_create_lands_under_managed_root_and_classifies_legacy() {
        let (_root, canonical_root, checkout, session) = managed_worktree_fixture();
        run_git_fixture(&checkout, &["branch", "feature/foo/bar"]);
        let legacy = checkout.join(".wt/legacy");
        let sibling = canonical_root.join("repo-legacy-linked");
        let before = [
            worktree_snapshot(&checkout),
            worktree_snapshot(&legacy),
            worktree_snapshot(&sibling),
        ];

        let result = git_worktree_create_in_src_root(
            &json!({"session_id": session.id, "branch": "feature/foo/bar"}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap();
        let value = result_text(&result);
        assert_eq!(value["status"], "created");
        assert_eq!(value["repository"], "repo");
        assert_eq!(value["task"], "feature-foo-bar");
        assert_eq!(value["identity_verified"], true);
        assert_eq!(value["mutation_committed"], true);
        assert!(value.get("verification_error").is_none());
        let target = canonical_root.join("worktrees/repo/feature-foo-bar");
        assert_eq!(value["path"], target.to_string_lossy().as_ref());
        assert!(target.join(".git").exists());
        assert!(!canonical_root.join("worktrees/repo/feature").exists());
        assert_eq!(
            git_fixture_stdout(&target, &["branch", "--show-current"]),
            "feature/foo/bar"
        );

        assert!(
            git_fixture_stdout(&checkout, &["status", "--porcelain"]).contains("?? untracked.txt")
        );
        assert_eq!(worktree_snapshot(&checkout), before[0]);
        assert_eq!(worktree_snapshot(&legacy), before[1]);
        assert_eq!(worktree_snapshot(&sibling), before[2]);

        let listed = git_worktree_list_with_src_root(
            &json!({"session_id": session.id}),
            &session,
            Some(&canonical_root),
            None,
        )
        .await
        .unwrap();
        let listed = result_text(&listed);
        assert_eq!(listed["repository"], "repo");
        assert_eq!(
            listed["managed_root"],
            canonical_root
                .join("worktrees/repo")
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(
            managed_worktree_list_value(&listed, &checkout).as_deref(),
            Some("primary")
        );
        assert_eq!(
            managed_worktree_list_value(&listed, &target).as_deref(),
            Some("managed")
        );
        assert_eq!(
            managed_worktree_list_value(&listed, &legacy).as_deref(),
            Some("legacy")
        );
        assert_eq!(
            managed_worktree_list_value(&listed, &sibling).as_deref(),
            Some("legacy")
        );
        assert_eq!(listed["truncated"], false);
    }

    #[tokio::test]
    async fn managed_worktree_create_denied_approval_has_zero_side_effects() {
        let (_root, canonical_root, checkout, session) = managed_worktree_fixture();
        // The Ask-mode session has no approval console, so the existing
        // approval framework fails closed without a real host prompt.
        let ask_session = config::Session {
            permission_mode: config::PermissionMode::Ask,
            ..session
        };
        let branches_before =
            git_fixture_stdout(&checkout, &["branch", "--format=%(refname:short)"]);
        let worktrees_before = git_fixture_stdout(&checkout, &["worktree", "list", "--porcelain"]);

        let error = git_worktree_create_in_src_root(
            &json!({"session_id": ask_session.id, "branch": "main", "task": "denied-task"}),
            &ask_session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("not running"),
            "unexpected error: {error:#}"
        );
        assert!(!canonical_root.join("worktrees").exists());
        assert!(!canonical_root.join("worktrees/repo").exists());
        assert!(!canonical_root.join("worktrees/repo/denied-task").exists());
        assert_eq!(
            git_fixture_stdout(&checkout, &["branch", "--format=%(refname:short)"]),
            branches_before
        );
        assert_eq!(
            git_fixture_stdout(&checkout, &["worktree", "list", "--porcelain"]),
            worktrees_before
        );
    }

    #[tokio::test]
    async fn managed_worktree_create_attaches_only_an_existing_local_branch() {
        let (_root, canonical_root, checkout, session) = managed_worktree_fixture();
        let branches_before =
            git_fixture_stdout(&checkout, &["branch", "--format=%(refname:short)"]);
        let worktrees_before = git_fixture_stdout(&checkout, &["worktree", "list", "--porcelain"]);

        let error = git_worktree_create_in_src_root(
            &json!({"session_id": session.id, "branch": "missing-branch", "task": "missing-task"}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{error:#}");
        assert_eq!(
            git_fixture_stdout(&checkout, &["branch", "--format=%(refname:short)"]),
            branches_before
        );
        assert_eq!(
            git_fixture_stdout(&checkout, &["worktree", "list", "--porcelain"]),
            worktrees_before
        );
        assert!(!canonical_root.join("worktrees").exists());
        assert!(!canonical_root.join("worktrees/repo/missing-task").exists());
    }

    #[tokio::test]
    async fn managed_worktree_create_rejects_removed_cwd_and_base_arguments() {
        let (_root, canonical_root, _checkout, session) = managed_worktree_fixture();

        let cwd_error = git_worktree_create_in_src_root(
            &json!({"session_id": session.id, "branch": "main", "cwd": "/tmp"}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(cwd_error.to_string().contains("cwd"), "{cwd_error:#}");
        let base_error = git_worktree_create_in_src_root(
            &json!({"session_id": session.id, "branch": "main", "base": "HEAD"}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(base_error.to_string().contains("base"), "{base_error:#}");
        assert!(!canonical_root.join("worktrees").exists());
    }

    async fn assert_layout_rejected(checkout: PathBuf, src_root: &Path, session: &config::Session) {
        let checkout = std::fs::canonicalize(&checkout).unwrap();
        let layout_session = config::Session {
            cwd: checkout.clone(),
            permitted_directories: vec![checkout.clone()],
            ..session.clone()
        };
        let worktrees_before = git_fixture_stdout(&checkout, &["worktree", "list", "--porcelain"]);
        let error = git_worktree_create_in_src_root(
            &json!({"session_id": layout_session.id, "branch": "main"}),
            &layout_session,
            src_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("exactly one directory"),
            "{checkout:?}: {error:#}"
        );
        assert_eq!(
            git_fixture_stdout(&checkout, &["worktree", "list", "--porcelain"]),
            worktrees_before
        );
    }

    #[tokio::test]
    async fn managed_worktree_create_requires_the_exact_src_root_layout() {
        let (_root, canonical_root, _checkout, session) = managed_worktree_fixture();

        // Nested checkout below the src root.
        let nested = canonical_root.join("nested/repo");
        std::fs::create_dir_all(&nested).unwrap();
        init_git_repository(&nested);
        assert_layout_rejected(nested, &canonical_root, &session).await;

        // A repository below another named root is never redirected into the
        // `src` namespace.
        let other_checkout = canonical_root.join("work/repo");
        std::fs::create_dir_all(&other_checkout).unwrap();
        init_git_repository(&other_checkout);
        assert_layout_rejected(other_checkout, &canonical_root, &session).await;

        // A repository outside any configured root fails closed as well.
        let outside = tempfile::tempdir().unwrap();
        init_git_repository(outside.path());
        assert_layout_rejected(outside.path().to_path_buf(), &canonical_root, &session).await;

        assert!(!canonical_root.join("worktrees").exists());
    }

    #[tokio::test]
    async fn managed_worktree_create_rejects_a_checkout_inside_the_reserved_namespace() {
        let (_root, canonical_root, _checkout, session) = managed_worktree_fixture();
        let checkout = canonical_root.join("worktrees/repo");
        std::fs::create_dir_all(&checkout).unwrap();
        init_git_repository(&checkout);
        assert_layout_rejected(checkout.clone(), &canonical_root, &session).await;
        assert!(!checkout.join("task").exists());
    }

    #[tokio::test]
    async fn managed_worktree_create_rejects_collisions_injection_and_wrong_repository() {
        let (root, canonical_root, _checkout, session) = managed_worktree_fixture();

        for task in ["/tmp/x", "../x", ".", "a/../../b", "-option", "bad\nname"] {
            let error = git_worktree_create_in_src_root(
                &json!({"session_id": session.id, "branch": "main", "task": task}),
                &session,
                &canonical_root,
                None,
            )
            .await
            .unwrap_err();
            assert!(!error.to_string().is_empty());
        }
        assert!(!root.path().join("x").exists());
        assert!(!canonical_root.join("worktrees/repo/option").exists());

        let mismatch = git_worktree_create_in_src_root(
            &json!({"session_id": session.id, "branch": "main", "repository": "other-repo"}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            mismatch.to_string().contains("does not match"),
            "{mismatch:#}"
        );

        let occupied = canonical_root.join("worktrees/repo/task-occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("unrelated.txt"), "keep\n").unwrap();
        let wrong_task = canonical_root.join("worktrees/repo/task-wrong");
        std::fs::create_dir_all(wrong_task.parent().unwrap()).unwrap();
        let other = canonical_root.join("other-repo");
        std::fs::create_dir(&other).unwrap();
        init_git_repository(&other);
        run_git_fixture(
            &other,
            &[
                "worktree",
                "add",
                "--quiet",
                wrong_task.to_str().unwrap(),
                "-b",
                "other-branch",
            ],
        );
        let wrong_before = worktree_snapshot(&wrong_task);

        for task in ["task-occupied", "task-wrong"] {
            let error = git_worktree_create_in_src_root(
                &json!({"session_id": session.id, "branch": "main", "task": task}),
                &session,
                &canonical_root,
                None,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("already exists"), "{error:#}");
        }
        assert_eq!(
            std::fs::read_to_string(occupied.join("unrelated.txt")).unwrap(),
            "keep\n"
        );
        assert_eq!(worktree_snapshot(&wrong_task), wrong_before);
        assert!(git_fixture_stdout(&wrong_task, &["branch", "--show-current"]) == "other-branch");

        #[cfg(unix)]
        {
            let link = canonical_root.join("worktrees/repo/task-link");
            std::os::unix::fs::symlink(canonical_root.join("outside/escape"), &link).unwrap();
            let error = git_worktree_create_in_src_root(
                &json!({"session_id": session.id, "branch": "main", "task": "task-link"}),
                &session,
                &canonical_root,
                None,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("already exists"), "{error:#}");
            assert!(!canonical_root.join("outside/escape").exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_worktree_create_rejects_symlinked_managed_root() {
        let (root, canonical_root, _checkout, session) = managed_worktree_fixture();
        let outside = canonical_root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        assert!(!canonical_root.join("worktrees").exists());
        std::os::unix::fs::symlink(&outside, canonical_root.join("worktrees")).unwrap();

        let error = git_worktree_create_in_src_root(
            &json!({"session_id": session.id, "branch": "main", "task": "task-one"}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("normal directory"), "{error:#}");
        assert!(!outside.join("repo").exists());
        assert!(!outside.join("repo/task-one").exists());
        drop(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_worktree_list_never_classifies_below_a_symlinked_managed_root() {
        let (_root, canonical_root, checkout, session) = managed_worktree_fixture();
        run_git_fixture(&checkout, &["branch", "task-branch"]);
        let created = git_worktree_create_in_src_root(
            &json!({"session_id": session.id, "branch": "task-branch", "task": "task-symlink"}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap();
        let created = result_text(&created);
        assert_eq!(created["status"], "created");
        let target = canonical_root.join("worktrees/repo/task-symlink");

        // Without the configured src root nothing may be classified managed.
        let unconfigured = git_worktree_list_with_src_root(
            &json!({"session_id": session.id}),
            &session,
            None,
            None,
        )
        .await
        .unwrap();
        let unconfigured = result_text(&unconfigured);
        assert_eq!(unconfigured["managed_root"], Value::Null);
        assert_eq!(
            managed_worktree_list_value(&unconfigured, &target).as_deref(),
            Some("legacy")
        );
        assert_eq!(
            managed_worktree_list_value(&unconfigured, &checkout).as_deref(),
            Some("primary")
        );

        // A swapped managed root (here a symlink) must disable managed
        // classification instead of adopting paths that resolve below it.
        let outside = canonical_root.join("outside-repo");
        std::fs::rename(canonical_root.join("worktrees/repo"), &outside).unwrap();
        std::os::unix::fs::symlink(&outside, canonical_root.join("worktrees/repo")).unwrap();

        let listed = git_worktree_list_with_src_root(
            &json!({"session_id": session.id}),
            &session,
            Some(&canonical_root),
            None,
        )
        .await
        .unwrap();
        let listed = result_text(&listed);
        assert_eq!(listed["managed_root"], Value::Null);
        assert_eq!(
            managed_worktree_list_value(&listed, &target).as_deref(),
            Some("legacy")
        );
    }

    #[test]
    fn managed_worktree_tools_do_not_expose_cwd_or_base() {
        let tools = tools(true, true).as_array().unwrap().to_owned();
        for name in [
            "git_worktree_create",
            "git_worktree_list",
            "git_worktree_remove",
            "git_worktree_prune",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("{name} missing"));
            let properties = tool["inputSchema"]["properties"].as_object().unwrap();
            for removed in ["cwd", "base"] {
                assert!(
                    !properties.contains_key(removed),
                    "{name} must not expose {removed}"
                );
            }
        }
        let create = tools
            .iter()
            .find(|tool| tool["name"] == "git_worktree_create")
            .unwrap();
        assert_eq!(
            create["inputSchema"]["required"],
            json!(["session_id", "branch"])
        );
        assert!(create["inputSchema"]["properties"]["task"].is_object());
        let list = tools
            .iter()
            .find(|tool| tool["name"] == "git_worktree_list")
            .unwrap();
        assert_eq!(list["inputSchema"]["required"], json!(["session_id"]));
    }

    #[test]
    fn local_agent_worktree_input_is_bounded_and_path_free() {
        let tools = tools(true, true).as_array().unwrap().to_owned();
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == "local_agent_run")
            .unwrap();
        let worktree = &tool["inputSchema"]["properties"]["worktree"];
        assert_eq!(worktree["type"], "object");
        assert_eq!(worktree["required"], json!(["branch"]));
        assert_eq!(worktree["additionalProperties"], false);
        assert_eq!(
            worktree["properties"]["branch"]["maxLength"],
            json!(MAX_GIT_BRANCH_NAME_BYTES)
        );
        assert_eq!(
            worktree["properties"]["task"]["maxLength"],
            json!(managed_worktree::MAX_MANAGED_TASK_BYTES)
        );
        assert!(
            !worktree["properties"]
                .as_object()
                .unwrap()
                .contains_key("cwd")
        );
        assert!(
            !worktree["properties"]
                .as_object()
                .unwrap()
                .contains_key("path")
        );
    }

    #[tokio::test]
    async fn local_agent_worktree_binding_reuses_and_creates_verified_managed_worktrees() {
        let (_root, canonical_root, checkout, session) = managed_worktree_fixture();
        run_git_fixture(&checkout, &["branch", "feature/foo/bar"]);
        let target = canonical_root.join("worktrees/repo/feature-foo-bar");

        // Absent target: Temote creates the managed worktree through the
        // approved broker path without any caller-supplied path.
        let created = local_agent_managed_worktree_binding_with_src_root(
            &json!({"session_id": session.id, "worktree": {"branch": "feature/foo/bar"}}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap()
        .expect("managed worktree binding");
        assert_eq!(created.workspace_root(), target);
        assert_eq!(created.branch, "feature/foo/bar");
        assert_eq!(created.repository_name(), "repo");
        assert_eq!(
            git_fixture_stdout(&target, &["branch", "--show-current"]),
            "feature/foo/bar"
        );

        // The run session gains exactly the validated managed workspace root;
        // the on-disk session keeps its own scope.
        let run_session = created.run_session(&session);
        assert_eq!(
            run_session.permitted_directories.len(),
            session.permitted_directories.len() + 1
        );
        assert!(run_session.permitted_directories.contains(&target));
        assert_eq!(session.permitted_directories, vec![checkout.clone()]);

        // Existing target: reuse only the verified managed worktree.
        let reused = local_agent_managed_worktree_binding_with_src_root(
            &json!({"session_id": session.id, "worktree": {"branch": "feature/foo/bar"}}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap()
        .expect("managed worktree binding");
        assert_eq!(reused.workspace_root(), target);

        // Primary working tree, dirty sentinel and legacy worktrees stay
        // untouched.
        assert!(
            git_fixture_stdout(&checkout, &["status", "--porcelain"]).contains("?? untracked.txt")
        );
        assert_eq!(
            git_fixture_stdout(&checkout.join(".wt/legacy"), &["branch", "--show-current"]),
            "legacy-branch"
        );
        assert_eq!(
            git_fixture_stdout(
                &canonical_root.join("repo-legacy-linked"),
                &["branch", "--show-current"]
            ),
            "sibling-branch"
        );

        // Immediate pre-launch revalidation succeeds for the verified target
        // and fails closed once the workspace identity changes.
        created.revalidate(&canonical_root).unwrap();
        std::fs::rename(&target, canonical_root.join("worktrees/repo/moved")).unwrap();
        assert!(created.revalidate(&canonical_root).is_err());
    }

    #[tokio::test]
    async fn local_agent_without_worktree_intent_needs_no_managed_root_configuration() {
        let (_root, _canonical_root, checkout, session) = managed_worktree_fixture();
        let binding = local_agent_managed_worktree_binding(
            &json!({
                "session_id": session.id,
                "cwd": checkout.to_string_lossy()
            }),
            &session,
            None,
        )
        .await
        .unwrap();
        assert!(binding.is_none());
    }

    fn managed_worktree_removal_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, config::Session)
    {
        let (root, canonical_root, checkout, session) = managed_worktree_fixture();
        for (branch, task) in [
            ("feature/remove-me", "feature-remove-me"),
            ("feature/sibling", "feature-sibling"),
        ] {
            run_git_fixture(&checkout, &["branch", branch]);
            let target = canonical_root.join("worktrees/repo").join(task);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            run_git_fixture(
                &checkout,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    target.to_str().unwrap(),
                    branch,
                ],
            );
        }
        (root, canonical_root, checkout, session)
    }

    fn session_view_for_test(
        id: &str,
        cwd: PathBuf,
        status: &str,
        workspace_root: Option<PathBuf>,
    ) -> session_control::SessionView {
        session_control::SessionView {
            host_id: "test-host".to_owned(),
            id: id.to_owned(),
            session_id: id.to_owned(),
            status: status.to_owned(),
            pid: None,
            process_id: 0,
            cwd,
            permitted_directories: Vec::new(),
            started_at: 0,
            stopped_at: None,
            exit_reason: None,
            last_error: None,
            permission_mode: config::PermissionMode::Agent,
            yolo: false,
            logical_path: None,
            workspace: workspace_root.map(|root| managed_worktree::SessionWorkspace {
                workspace_type: managed_worktree::SessionWorkspaceType::ManagedWorktree,
                repository: Some("repo".to_owned()),
                repository_root: root.clone(),
                workspace_root: root,
                branch: None,
                task: None,
            }),
            restart_policy: "never".to_owned(),
            restart_count: 0,
            last_restart_at: None,
            next_restart_at: None,
            restart_limit_reason: None,
        }
    }

    fn managed_worktree_prune_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, config::Session) {
        let (root, canonical_root, checkout, session) = managed_worktree_fixture();
        let managed_root = canonical_root.join("worktrees/repo");
        std::fs::create_dir_all(&managed_root).unwrap();
        for (branch, task) in [
            ("stale-branch", "stale-managed"),
            ("gone-branch", "gone-managed"),
            ("live-branch", "live-managed"),
            ("dirty-branch", "dirty-managed"),
        ] {
            run_git_fixture(&checkout, &["branch", branch]);
            run_git_fixture(
                &checkout,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    managed_root.join(task).to_str().unwrap(),
                    branch,
                ],
            );
        }
        // Git-classified stale metadata whose directory still exists with
        // unrelated files.
        let stale = managed_root.join("stale-managed");
        std::fs::remove_file(stale.join(".git")).unwrap();
        std::fs::write(stale.join("unrelated.txt"), "keep me\n").unwrap();
        // Git-classified stale metadata whose directory is gone.
        std::fs::remove_dir_all(managed_root.join("gone-managed")).unwrap();
        // Dirty worktree.
        std::fs::write(managed_root.join("dirty-managed/unrelated.txt"), "dirty\n").unwrap();
        // A filesystem-only directory that was never registered.
        std::fs::create_dir(managed_root.join("orphan-dir")).unwrap();
        std::fs::write(managed_root.join("orphan-dir/keep.txt"), "orphan\n").unwrap();
        (root, canonical_root, checkout, session)
    }

    #[tokio::test]
    async fn managed_worktree_prune_removes_only_stale_metadata_and_preserves_filesystems() {
        let (_root, canonical_root, checkout, session) = managed_worktree_prune_fixture();
        let managed_root = canonical_root.join("worktrees/repo");
        let stale = managed_root.join("stale-managed");
        let live = managed_root.join("live-managed");
        let dirty = managed_root.join("dirty-managed");
        let legacy = checkout.join(".wt/legacy");
        let legacy_sibling = canonical_root.join("repo-legacy-linked");
        let live_before = worktree_snapshot(&live);
        let dirty_before = worktree_snapshot(&dirty);
        let legacy_before = worktree_snapshot(&legacy);
        let legacy_sibling_before = worktree_snapshot(&legacy_sibling);
        let primary_before = worktree_snapshot(&checkout);

        let result = git_worktree_prune_in_src_root_with_snapshots(
            &json!({"session_id": session.id}),
            &session,
            &canonical_root,
            None,
            &[],
            &[],
        )
        .await
        .unwrap();
        let value = result_text(&result);
        assert_eq!(value["status"], "pruned");
        assert_eq!(value["repository"], "repo");
        assert_eq!(value["before"]["prunable_count"], 2);
        assert_eq!(value["after"]["prunable_count"], 0);
        assert_eq!(value["removed_metadata_count"], 2);
        assert_eq!(
            value["primary_checkout"],
            checkout.to_string_lossy().as_ref()
        );
        assert!(
            value["before"]["prunable"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path == stale.to_string_lossy().as_ref())
        );
        assert_eq!(value["filesystem_directories_preserved"], true);
        assert!(value.get("verification_error").is_none());
        for field in ["stdout", "stderr", "truncated"] {
            assert!(
                value.get(field).is_none(),
                "unexpected public field: {field}"
            );
        }

        // Metadata-only: the stale directory and its unrelated files survive.
        assert_eq!(
            std::fs::read_to_string(stale.join("unrelated.txt")).unwrap(),
            "keep me\n"
        );
        assert!(managed_root.join("orphan-dir/keep.txt").exists());
        assert!(!checkout.join(".git/worktrees/stale-managed").exists());
        assert!(!checkout.join(".git/worktrees/gone-managed").exists());
        assert!(checkout.join(".git/worktrees/live-managed").exists());
        assert!(checkout.join(".git/worktrees/dirty-managed").exists());

        // Live, dirty, legacy and out-of-root worktrees are unchanged.
        assert_eq!(worktree_snapshot(&live), live_before);
        assert_eq!(worktree_snapshot(&dirty), dirty_before);
        assert_eq!(worktree_snapshot(&legacy), legacy_before);
        assert_eq!(worktree_snapshot(&legacy_sibling), legacy_sibling_before);
        assert_eq!(worktree_snapshot(&checkout), primary_before);
        for branch in ["stale-branch", "gone-branch", "live-branch", "dirty-branch"] {
            assert!(
                !git_fixture_stdout(&checkout, &["branch", "--list", branch]).is_empty(),
                "branch {branch} must be preserved"
            );
        }
    }

    #[tokio::test]
    async fn managed_worktree_prune_is_idempotent() {
        let (_root, canonical_root, checkout, session) = managed_worktree_prune_fixture();
        let args = json!({"session_id": session.id});
        let first = result_text(
            &git_worktree_prune_in_src_root_with_snapshots(
                &args,
                &session,
                &canonical_root,
                None,
                &[],
                &[],
            )
            .await
            .unwrap(),
        );
        assert_eq!(first["removed_metadata_count"], 2);
        let registered_after_first =
            git_fixture_stdout(&checkout, &["worktree", "list", "--porcelain"]);

        let second = result_text(
            &git_worktree_prune_in_src_root_with_snapshots(
                &args,
                &session,
                &canonical_root,
                None,
                &[],
                &[],
            )
            .await
            .unwrap(),
        );
        assert_eq!(second["status"], "pruned");
        assert_eq!(second["before"]["prunable_count"], 0);
        assert_eq!(second["after"]["prunable_count"], 0);
        assert_eq!(second["removed_metadata_count"], 0);
        for field in ["stdout", "stderr", "truncated"] {
            assert!(
                second.get(field).is_none(),
                "unexpected public field: {field}"
            );
        }
        assert_eq!(
            git_fixture_stdout(&checkout, &["worktree", "list", "--porcelain"]),
            registered_after_first
        );
    }

    #[tokio::test]
    async fn managed_worktree_prune_refuses_owned_stale_metadata_and_path_input() {
        let (_root, canonical_root, checkout, session) = managed_worktree_prune_fixture();
        let stale = canonical_root.join("worktrees/repo/stale-managed");
        let gone = canonical_root.join("worktrees/repo/gone-managed");
        let prunable = vec![stale.clone(), gone.clone()];

        // A live session that owns the stale path rejects the whole prune, so
        // nothing is pruned around it.
        let views = [session_view_for_test(
            "owner",
            PathBuf::from("/elsewhere"),
            "active",
            Some(stale.clone()),
        )];
        let error =
            ensure_prunable_entries_unowned_from(&session, &prunable, &views, &[]).unwrap_err();
        assert!(error.to_string().contains("refusing prune"), "{error:#}");

        // A running job in the stale path rejects it too.
        let jobs = [("job".to_owned(), gone.join("sub"))];
        let error =
            ensure_prunable_entries_unowned_from(&session, &prunable, &[], &jobs).unwrap_err();
        assert!(error.to_string().contains("refusing prune"), "{error:#}");

        // Terminal sessions never own stale metadata.
        let views = [
            session_view_for_test("stopped", stale.clone(), "stopped", Some(stale.clone())),
            session_view_for_test("crashed", stale.clone(), "crashed", Some(stale.clone())),
            session_view_for_test("degraded", stale.clone(), "degraded", Some(stale.clone())),
        ];
        ensure_prunable_entries_unowned_from(&session, &prunable, &views, &[]).unwrap();

        // The structured tool is path-free.
        for args in [
            json!({"session_id": session.id, "task": "stale-managed"}),
            json!({"session_id": session.id, "path": stale.to_string_lossy()}),
            json!({"session_id": session.id, "cwd": "/tmp"}),
            json!({"session_id": session.id, "base": "HEAD"}),
            json!({"session_id": session.id, "repository": "other"}),
        ] {
            let error = git_worktree_prune_in_src_root_with_snapshots(
                &args,
                &session,
                &canonical_root,
                None,
                &[],
                &[],
            )
            .await
            .unwrap_err();
            assert!(
                !error.to_string().contains("prune did not complete"),
                "{args}: {error:#}"
            );
        }
        // A session outside the configured src root fails closed.
        let outside = config::Session {
            cwd: std::env::temp_dir(),
            permitted_directories: vec![std::env::temp_dir()],
            ..session
        };
        assert!(
            git_worktree_prune_in_src_root_with_snapshots(
                &json!({"session_id": outside.id}),
                &outside,
                &canonical_root,
                None,
                &[],
                &[],
            )
            .await
            .is_err()
        );

        // Nothing was pruned by any rejected request.
        assert!(checkout.join(".git/worktrees/stale-managed").exists());
        assert!(stale.join("unrelated.txt").exists());
    }

    #[tokio::test]
    async fn managed_worktree_prune_rejects_existing_owner_before_mutation() {
        let (_root, canonical_root, checkout, session) = managed_worktree_prune_fixture();
        let managed_root = canonical_root.join("worktrees/repo");
        let stale = managed_root.join("stale-managed");
        let stale_metadata = checkout.join(".git/worktrees/stale-managed");
        let gone_metadata = checkout.join(".git/worktrees/gone-managed");
        let views = [session_view_for_test(
            "owner",
            PathBuf::from("/elsewhere"),
            "active",
            Some(stale.clone()),
        )];

        let error = git_worktree_prune_in_src_root_with_snapshots(
            &json!({"session_id": session.id}),
            &session,
            &canonical_root,
            None,
            &views,
            &[],
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("refusing prune"), "{error:#}");
        assert!(stale.exists());
        assert!(stale.join("unrelated.txt").exists());
        assert!(stale_metadata.exists());
        assert!(gone_metadata.exists());
    }

    #[test]
    fn managed_worktree_prune_verification_rejects_live_or_filesystem_changes() {
        let stale = PathBuf::from("/src/worktrees/repo/stale");
        let live = PathBuf::from("/src/worktrees/repo/live");
        let entry = |path: &Path, prunable: bool| managed_worktree::RegisteredWorktree {
            path: path.to_path_buf(),
            head: None,
            branch: None,
            bare: false,
            detached: false,
            prunable,
        };
        let fixture = tempfile::tempdir().unwrap();
        let existing = std::fs::canonicalize(fixture.path()).unwrap();
        let before = ManagedWorktreePruneObservation {
            registered: vec![entry(&stale, true), entry(&existing, false)],
            prunable: vec![stale.clone()],
            existing_paths: vec![existing.clone()],
        };

        // A prune that drops a live registration is rejected.
        let after = ManagedWorktreePruneObservation {
            registered: vec![entry(&stale, true)],
            prunable: vec![stale.clone()],
            existing_paths: vec![existing.clone()],
        };
        assert!(verify_managed_worktree_prune(&before, &after).is_err());

        // A prune that deletes a filesystem directory is rejected.
        let after = ManagedWorktreePruneObservation {
            registered: vec![entry(&existing, false)],
            prunable: Vec::new(),
            existing_paths: vec![existing.clone()],
        };
        std::fs::remove_dir_all(&existing).unwrap();
        assert!(verify_managed_worktree_prune(&before, &after).is_err());

        // A prune that leaves an unexpected stale entry is rejected.
        let after = ManagedWorktreePruneObservation {
            registered: vec![
                entry(&stale, true),
                entry(&live, true),
                entry(&existing, false),
            ],
            prunable: vec![stale.clone(), live.clone()],
            existing_paths: Vec::new(),
        };
        assert!(verify_managed_worktree_prune(&before, &after).is_err());
    }

    #[test]
    fn managed_worktree_prune_result_bounds_failure_output() {
        let root = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(root.path()).unwrap();
        let checkout = src_root.join("repo");
        std::fs::create_dir(&checkout).unwrap();
        let repository =
            managed_worktree::ManagedRepository::resolve(&checkout, &src_root).unwrap();
        let stale = src_root.join("worktrees/repo/stale");
        let before = ManagedWorktreePruneObservation {
            registered: Vec::new(),
            prunable: vec![stale.clone()],
            existing_paths: vec![stale.clone()],
        };
        let after = ManagedWorktreePruneObservation::default();
        let raw_stdout = "raw stdout must not be returned";
        let raw_stderr = "raw stderr /tmp/secret must not be returned";
        let output = sandbox::Output {
            status: 17,
            stdout: raw_stdout.to_owned(),
            stderr: raw_stderr.to_owned(),
            truncated: true,
        };
        let rendered = managed_worktree_prune_result(
            "failed",
            &repository,
            &before,
            &after,
            &output,
            Some("raw verification error /tmp/secret"),
        );
        let value: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["status"], "failed");
        assert_eq!(value["repository"], "repo");
        assert_eq!(value["before"]["prunable_count"], 1);
        assert_eq!(
            value["before"]["prunable"][0],
            stale.to_string_lossy().as_ref()
        );
        assert_eq!(value["removed_metadata_count"], 1);
        assert_eq!(value["filesystem_directories_preserved"], false);
        assert_eq!(value["mutation_committed"], false);
        assert_eq!(value["exit_code"], 17);
        assert_eq!(
            value["verification_error"],
            MANAGED_WORKTREE_PRUNE_MUTATION_ERROR
        );
        for field in ["stdout", "stderr", "truncated"] {
            assert!(
                value.get(field).is_none(),
                "unexpected public field: {field}"
            );
        }
        assert!(!rendered.contains(raw_stdout));
        assert!(!rendered.contains(raw_stderr));
        assert!(!rendered.contains("raw verification error"));

        let verification_output = sandbox::Output {
            status: 0,
            stdout: raw_stdout.to_owned(),
            stderr: raw_stderr.to_owned(),
            truncated: true,
        };
        let verification = managed_worktree_prune_result(
            "verification_failed",
            &repository,
            &before,
            &after,
            &verification_output,
            Some("raw verification error /tmp/secret"),
        );
        let verification: Value = serde_json::from_str(&verification).unwrap();
        assert_eq!(
            verification["verification_error"],
            MANAGED_WORKTREE_PRUNE_VERIFICATION_ERROR
        );
        assert!(!verification.to_string().contains("raw verification error"));
    }

    #[tokio::test]
    async fn github_https_credential_mapping_is_repository_local_and_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(root.path()).unwrap();
        let make_repo = |name: &str| {
            let path = src_root.join(name);
            std::fs::create_dir(&path).unwrap();
            init_git_repository(&path);
            run_git_fixture(
                &path,
                &[
                    "remote",
                    "add",
                    "origin",
                    &format!("https://github.com/example/{name}.git"),
                ],
            );
            path
        };
        let managed = make_repo("managed");
        // The managed contract is a helper reset followed by the managed helper.
        // `git config --add` preserves the empty reset entry that the repo-local
        // gh-git binding writes into its included config file.
        run_git_fixture(
            &managed,
            &["config", "--local", "--add", "credential.helper", ""],
        );
        run_git_fixture(
            &managed,
            &[
                "config",
                "--local",
                "--add",
                "credential.helper",
                "!gh git credential --managed",
            ],
        );
        run_git_fixture(
            &managed,
            &["config", "--local", "credential.useHttpPath", "true"],
        );
        let unmanaged = make_repo("unmanaged");
        run_git_fixture(
            &unmanaged,
            &["config", "--local", "credential.helper", "store"],
        );
        run_git_fixture(
            &unmanaged,
            &["config", "--local", "credential.useHttpPath", "true"],
        );
        let missing = make_repo("missing");
        let local_remote = src_root.join("local-remote.git");
        run_git_fixture(
            &src_root,
            &["init", "--quiet", "--bare", local_remote.to_str().unwrap()],
        );
        let plain = src_root.join("plain");
        std::fs::create_dir(&plain).unwrap();
        init_git_repository(&plain);
        run_git_fixture(
            &plain,
            &["remote", "add", "origin", local_remote.to_str().unwrap()],
        );

        let session = config::Session {
            id: format!("github-credential-{}", Uuid::new_v4()),
            cwd: managed.clone(),
            permitted_directories: vec![
                src_root.clone(),
                managed.clone(),
                unmanaged.clone(),
                missing.clone(),
                plain.clone(),
            ],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
        };

        ensure_github_https_remote_credential_mapping(&session, &managed, "origin")
            .await
            .unwrap();
        for (path, label) in [
            (&unmanaged, "unmanaged helper"),
            (&missing, "missing mapping"),
        ] {
            let error = ensure_github_https_remote_credential_mapping(&session, path, "origin")
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("credential mapping is unavailable"),
                "{label}: {error:#}"
            );
        }
        // A non-GitHub remote carries no GitHub credential and is left to the
        // configured-remote contract.
        ensure_github_https_remote_credential_mapping(&session, &plain, "origin")
            .await
            .unwrap();
        // An unknown remote name fails closed before any credential work.
        assert!(
            ensure_github_https_remote_credential_mapping(&session, &managed, "upstream")
                .await
                .is_err()
        );

        // The managed credential command is a read-only `gh git credential`
        // lookup; no `gh auth` state is ever touched.
        assert_eq!(
            github_managed_credential_command(),
            vec![
                "gh".to_owned(),
                "git".to_owned(),
                "credential".to_owned(),
                "--managed".to_owned(),
                "get".to_owned()
            ]
        );
        assert!(
            !github_managed_credential_command()
                .iter()
                .any(|token| token == "auth")
        );
    }

    #[tokio::test]
    async fn github_https_credential_mapping_stays_repository_local_across_concurrent_repositories()
    {
        let root = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(root.path()).unwrap();
        let make_github_repo = |name: &str| {
            let path = src_root.join(name);
            std::fs::create_dir(&path).unwrap();
            init_git_repository(&path);
            run_git_fixture(
                &path,
                &[
                    "remote",
                    "add",
                    "origin",
                    &format!("https://github.com/example/{name}.git"),
                ],
            );
            path
        };
        // Repository "bound" stores the managed mapping in an included file below
        // its own .git directory, the same shape the repo-local gh-git binding
        // writes. Repository "unmanaged" has a non-managed helper.
        let bound = make_github_repo("bound");
        let include_path = bound.join(".git/gh-git.conf");
        std::fs::write(
            &include_path,
            "[credential]\n\thelper =\n\thelper = !gh git credential --managed\n\tuseHttpPath = true\n",
        )
        .unwrap();
        run_git_fixture(
            &bound,
            &["config", "--local", "include.path", "gh-git.conf"],
        );
        let unmanaged = make_github_repo("unmanaged");
        run_git_fixture(
            &unmanaged,
            &["config", "--local", "credential.helper", "store"],
        );
        run_git_fixture(
            &unmanaged,
            &["config", "--local", "credential.useHttpPath", "true"],
        );

        let session_for = |name: &str, cwd: &Path| config::Session {
            id: format!("{name}-{}", Uuid::new_v4()),
            cwd: cwd.to_owned(),
            permitted_directories: vec![src_root.clone(), bound.clone(), unmanaged.clone()],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
        };
        let bound_session = session_for("bound", &bound);
        let unmanaged_session = session_for("unmanaged", &unmanaged);

        // Interleaved concurrent selections stay bound to the working
        // repository: session identity never supplies credential state.
        let (bound_ok, unmanaged_err) = tokio::join!(
            ensure_github_https_remote_credential_mapping(&bound_session, &bound, "origin"),
            ensure_github_https_remote_credential_mapping(&unmanaged_session, &unmanaged, "origin"),
        );
        bound_ok.unwrap();
        let error = unmanaged_err.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("credential mapping is unavailable"),
            "{error:#}"
        );

        // Cross-selection keeps following the repository, not the session.
        let (cross_unmanaged, cross_bound) = tokio::join!(
            ensure_github_https_remote_credential_mapping(&bound_session, &unmanaged, "origin"),
            ensure_github_https_remote_credential_mapping(&unmanaged_session, &bound, "origin"),
        );
        let error = cross_unmanaged.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("credential mapping is unavailable"),
            "{error:#}"
        );
        cross_bound.unwrap();
    }

    #[tokio::test]
    async fn github_https_network_git_requires_the_repository_local_mapping() {
        let root = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(root.path()).unwrap();
        let repository = src_root.join("repo");
        std::fs::create_dir(&repository).unwrap();
        init_git_repository(&repository);
        run_git_fixture(
            &repository,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/example/repo.git",
            ],
        );
        // An upstream makes the effective push remote resolvable; without the
        // managed mapping neither fetch nor push may start a Git network process.
        run_git_fixture(
            &repository,
            &["config", "--local", "branch.main.remote", "origin"],
        );
        run_git_fixture(
            &repository,
            &["config", "--local", "branch.main.merge", "refs/heads/main"],
        );
        let session = config::Session {
            id: format!("github-network-{}", Uuid::new_v4()),
            cwd: repository.clone(),
            permitted_directories: vec![repository.clone()],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
        };

        // Without the managed mapping the validated network commands fail
        // closed before any Git network process starts.
        let error = git_fetch_output(&session, repository.clone(), None, None)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("credential mapping is unavailable"),
            "{error:#}"
        );
        let error = git_push_output(&session, repository.clone(), None, false, None)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("credential mapping is unavailable"),
            "{error:#}"
        );

        // With the exact managed mapping the credential gate passes; the fixed
        // remote name is still the only network argument and never a URL. The
        // network commands themselves are not executed here (live GitHub
        // credential acceptance is Phase 4).
        run_git_fixture(
            &repository,
            &["config", "--local", "--add", "credential.helper", ""],
        );
        run_git_fixture(
            &repository,
            &[
                "config",
                "--local",
                "--add",
                "credential.helper",
                "!gh git credential --managed",
            ],
        );
        run_git_fixture(
            &repository,
            &["config", "--local", "credential.useHttpPath", "true"],
        );
        ensure_github_https_remote_credential_mapping(&session, &repository, "origin")
            .await
            .unwrap();
        assert_eq!(
            build_git_fetch_command("origin").last().map(String::as_str),
            Some("origin")
        );
        assert_eq!(
            build_git_push_command(None, false),
            vec![
                "git".to_owned(),
                "-c".to_owned(),
                "core.hooksPath=/dev/null".to_owned(),
                "-c".to_owned(),
                "push.recurseSubmodules=off".to_owned(),
                "push".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn default_push_remote_resolution_stays_bounded_and_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(root.path()).unwrap();
        let repository = src_root.join("repo");
        std::fs::create_dir(&repository).unwrap();
        init_git_repository(&repository);
        let session = config::Session {
            id: format!("push-remote-{}", Uuid::new_v4()),
            cwd: repository.clone(),
            permitted_directories: vec![repository.clone()],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
        };

        // No remote at all: nothing to gate.
        assert_eq!(
            git_current_push_remote(&session, &repository)
                .await
                .unwrap(),
            None
        );

        // Git's documented default destination is origin when it is configured.
        let bare = src_root.join("bare.git");
        run_git_fixture(
            &src_root,
            &["init", "--quiet", "--bare", bare.to_str().unwrap()],
        );
        run_git_fixture(
            &repository,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        assert_eq!(
            git_current_push_remote(&session, &repository)
                .await
                .unwrap(),
            Some("origin".to_owned())
        );

        // A local destination never contacts a remote.
        run_git_fixture(
            &repository,
            &["config", "--local", "branch.main.remote", "."],
        );
        assert_eq!(
            git_current_push_remote(&session, &repository)
                .await
                .unwrap(),
            None
        );

        // A configured branch remote wins over the origin default.
        run_git_fixture(
            &repository,
            &["config", "--local", "branch.main.pushRemote", "mirror"],
        );
        assert_eq!(
            git_current_push_remote(&session, &repository)
                .await
                .unwrap(),
            Some("mirror".to_owned())
        );

        // Malformed names fail closed instead of reaching any Git command.
        run_git_fixture(
            &repository,
            &[
                "config",
                "--local",
                "branch.main.pushRemote",
                "origin;touch /tmp/pwned",
            ],
        );
        assert!(
            git_current_push_remote(&session, &repository)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn github_https_pull_gate_uses_configured_branch_before_tracking_ref() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        init_git_repository(&repository);
        run_git_fixture(
            &repository,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/example/first-pull.git",
            ],
        );
        run_git_fixture(
            &repository,
            &["config", "--local", "branch.main.remote", "origin"],
        );
        run_git_fixture(
            &repository,
            &["config", "--local", "branch.main.merge", "refs/heads/main"],
        );
        let repository = std::fs::canonicalize(repository).unwrap();
        let session = config::Session {
            id: format!("pull-first-{}", Uuid::new_v4()),
            cwd: repository.clone(),
            permitted_directories: vec![repository.clone()],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
        };

        assert_eq!(
            git_current_upstream_remote(&session, &repository)
                .await
                .unwrap(),
            Some("origin".to_owned())
        );
        let error = git_pull_output(&session, repository, None)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), GITHUB_CREDENTIAL_MAPPING_ERROR);
    }

    #[tokio::test]
    async fn github_https_push_gate_uses_all_actual_push_urls() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repo");
        let local_remote = root.path().join("local.git");
        std::fs::create_dir(&repository).unwrap();
        init_git_repository(&repository);
        run_git_fixture(
            root.path(),
            &["init", "--quiet", "--bare", local_remote.to_str().unwrap()],
        );
        run_git_fixture(
            &repository,
            &["remote", "add", "origin", local_remote.to_str().unwrap()],
        );
        let local_remote = std::fs::canonicalize(local_remote).unwrap();
        let repository = std::fs::canonicalize(repository).unwrap();
        let session = config::Session {
            id: format!("push-destinations-{}", Uuid::new_v4()),
            cwd: repository.clone(),
            permitted_directories: vec![repository.clone(), local_remote.clone()],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
        };

        // A non-GitHub fetch URL with a GitHub pushurl still requires mapping.
        run_git_fixture(
            &repository,
            &[
                "config",
                "--local",
                "remote.origin.pushurl",
                "https://github.com/example/push.git",
            ],
        );
        let error = git_push_output(
            &session,
            repository.clone(),
            Some("origin".to_owned()),
            false,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), GITHUB_CREDENTIAL_MAPPING_ERROR);

        // A local pushurl is the only actual push destination; a GitHub fetch
        // URL must not force a mapping gate for this push.
        run_git_fixture(
            &repository,
            &["config", "--local", "--unset-all", "remote.origin.pushurl"],
        );
        run_git_fixture(
            &repository,
            &[
                "config",
                "--local",
                "remote.origin.url",
                "https://github.com/example/fetch-only.git",
            ],
        );
        run_git_fixture(
            &repository,
            &[
                "config",
                "--local",
                "remote.origin.pushurl",
                local_remote.to_str().unwrap(),
            ],
        );
        let output = git_push_output(
            &session,
            repository.clone(),
            Some("origin".to_owned()),
            true,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output.status, 0, "{}", output.stderr);

        // Every pushurl is inspected; a later GitHub destination cannot be
        // hidden behind an earlier local destination.
        run_git_fixture(
            &repository,
            &[
                "config",
                "--local",
                "--add",
                "remote.origin.pushurl",
                "https://github.com/example/second-push.git",
            ],
        );
        let destinations = resolve_git_remote_destinations(
            &session,
            &repository,
            "origin",
            GitRemoteOperation::Push,
        )
        .await
        .unwrap();
        assert_eq!(destinations.urls.len(), 2);
        assert!(destinations.requires_github_credential_mapping());
        let error = git_push_output(&session, repository, Some("origin".to_owned()), false, None)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), GITHUB_CREDENTIAL_MAPPING_ERROR);
    }

    #[test]
    fn github_https_network_git_error_classification_is_fixed_and_secret_free() {
        let unavailable = sandbox::Output {
            status: 128,
            stdout: String::new(),
            stderr: "fatal: could not read Username for https://github.com: terminal prompts disabled; secret-marker".to_owned(),
            truncated: false,
        };
        assert_eq!(
            classify_github_https_network_git_error(&unavailable),
            Some(GITHUB_CREDENTIAL_UNAVAILABLE_ERROR)
        );

        let permission = sandbox::Output {
            status: 1,
            stdout: String::new(),
            stderr: "remote: Write access denied (403) secret-marker".to_owned(),
            truncated: false,
        };
        assert_eq!(
            classify_github_https_network_git_error(&permission),
            Some(GITHUB_CREDENTIAL_PERMISSION_ERROR)
        );

        let unknown = sandbox::Output {
            status: 1,
            stdout: String::new(),
            stderr: "fatal: protocol failure secret-marker".to_owned(),
            truncated: true,
        };
        let generic = classify_github_https_network_git_error(&unknown).unwrap();
        assert_eq!(generic, GITHUB_NETWORK_GIT_ERROR);
        assert!(!generic.contains("secret-marker"));

        let success = sandbox::Output {
            status: 0,
            stdout: "ok".to_owned(),
            stderr: "403 warning secret-marker".to_owned(),
            truncated: false,
        };
        assert_eq!(classify_github_https_network_git_error(&success), None);
        let sanitized = sanitize_github_https_network_git_output(unknown);
        assert_eq!(sanitized.stderr, GITHUB_NETWORK_GIT_ERROR);
        assert!(!sanitized.stderr.contains("secret-marker"));
        assert!(sanitized.stdout.is_empty());
    }

    #[test]
    fn github_https_destination_detection_is_case_insensitive_and_authority_bound() {
        for url in [
            "HTTPS://GITHUB.COM/example/repo.git",
            "https://github.com:443/example/repo.git",
            "https://user:password@GitHub.Com/example/repo.git",
        ] {
            assert!(
                is_github_https_destination(url),
                "expected GitHub URL: {url}"
            );
        }
        for url in [
            "http://github.com/example/repo.git",
            "https://github.com.evil.example/example/repo.git",
            "https://github.com@evil.example/example/repo.git",
            "file:///tmp/repo.git",
        ] {
            assert!(
                !is_github_https_destination(url),
                "unexpected GitHub URL: {url}"
            );
        }
        assert!(
            GitRemoteDestinations {
                urls: vec!["HTTPS://GITHUB.COM/example/repo.git".to_owned()],
            }
            .requires_github_credential_mapping()
        );
    }

    #[test]
    fn github_credential_and_destination_inspection_errors_are_fixed_and_secret_free() {
        let marker = "inspection-marker";
        let mapping_invocation = map_github_credential_inspection_error::<()>(Err(
            anyhow::anyhow!("helper diagnostic: {marker}"),
        ))
        .unwrap_err();
        assert_eq!(
            mapping_invocation.to_string(),
            GITHUB_CREDENTIAL_MAPPING_ERROR
        );
        assert!(!mapping_invocation.to_string().contains(marker));

        let destination_invocation =
            map_git_remote_inspection_error::<()>(Err(anyhow::anyhow!("git diagnostic: {marker}")))
                .unwrap_err();
        assert_eq!(
            destination_invocation.to_string(),
            GIT_REMOTE_DESTINATION_ERROR
        );
        assert!(!destination_invocation.to_string().contains(marker));

        let mapping_truncated = validate_github_credential_mapping_inspection(
            &sandbox::Output {
                status: 0,
                stdout: "\n!gh git credential --managed\n".to_owned(),
                stderr: marker.to_owned(),
                truncated: true,
            },
            &sandbox::Output {
                status: 0,
                stdout: "true\n".to_owned(),
                stderr: String::new(),
                truncated: false,
            },
        )
        .unwrap_err();
        assert_eq!(
            mapping_truncated.to_string(),
            GITHUB_CREDENTIAL_MAPPING_ERROR
        );
        assert!(!mapping_truncated.to_string().contains(marker));

        let destination_truncated = parse_git_remote_destinations(
            &sandbox::Output {
                status: 0,
                stdout: "https://github.com/example/repo.git\n".to_owned(),
                stderr: marker.to_owned(),
                truncated: true,
            },
            GitRemoteOperation::Push,
        )
        .unwrap_err();
        assert_eq!(
            destination_truncated.to_string(),
            GIT_REMOTE_DESTINATION_ERROR
        );
        assert!(!destination_truncated.to_string().contains(marker));
    }

    #[tokio::test]
    async fn managed_worktree_remove_removes_only_the_selected_clean_worktree() {
        let (_root, canonical_root, checkout, session) = managed_worktree_removal_fixture();
        let managed_root = canonical_root.join("worktrees/repo");
        let target = managed_root.join("feature-remove-me");
        let sibling = managed_root.join("feature-sibling");
        let legacy = checkout.join(".wt/legacy");
        let legacy_sibling = canonical_root.join("repo-legacy-linked");
        let primary_before = worktree_snapshot(&checkout);
        let legacy_before = worktree_snapshot(&legacy);
        let legacy_sibling_before = worktree_snapshot(&legacy_sibling);

        let result = git_worktree_remove_in_src_root_with_snapshots(
            &json!({"session_id": session.id, "task": "feature-remove-me"}),
            &session,
            &canonical_root,
            None,
            &[],
            &[],
        )
        .await
        .unwrap();
        let value = result_text(&result);
        assert_eq!(value["status"], "removed");
        assert_eq!(value["repository"], "repo");
        assert_eq!(value["task"], "feature-remove-me");
        assert_eq!(value["branch"], "feature/remove-me");
        assert_eq!(value["mutation_committed"], true);
        assert_eq!(value["directory_removed"], true);
        assert_eq!(value["metadata_removed"], true);
        assert_eq!(value["siblings_preserved"], true);
        assert_eq!(value["branch_preserved"], true);
        assert!(value.get("verification_error").is_none());
        assert!(!target.exists());
        assert!(!checkout.join(".git/worktrees/feature-remove-me").exists());
        assert!(
            !git_fixture_stdout(&checkout, &["branch", "--list", "feature/remove-me"]).is_empty(),
            "the branch ref must be preserved"
        );

        // The path selector is accepted only as the exact derived path.
        let result = git_worktree_remove_in_src_root_with_snapshots(
            &json!({"session_id": session.id, "path": sibling.to_string_lossy()}),
            &session,
            &canonical_root,
            None,
            &[],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(result_text(&result)["status"], "removed");
        assert!(!sibling.exists());

        // Primary, legacy and out-of-root sibling worktrees are unchanged.
        assert_eq!(worktree_snapshot(&checkout), primary_before);
        assert_eq!(worktree_snapshot(&legacy), legacy_before);
        assert_eq!(worktree_snapshot(&legacy_sibling), legacy_sibling_before);
        assert!(legacy.join("tracked.txt").exists());
        assert!(legacy_sibling.join("tracked.txt").exists());
        assert!(
            !git_fixture_stdout(&checkout, &["branch", "--list", "feature/sibling"]).is_empty(),
            "the sibling branch ref must be preserved"
        );
    }

    #[tokio::test]
    async fn managed_worktree_remove_rejects_existing_owner_before_mutation() {
        let (_root, canonical_root, checkout, session) = managed_worktree_removal_fixture();
        let target = canonical_root.join("worktrees/repo/feature-remove-me");
        let metadata = checkout.join(".git/worktrees/feature-remove-me");
        let jobs = [("owner-job".to_owned(), target.join("nested"))];

        let error = git_worktree_remove_in_src_root_with_snapshots(
            &json!({"session_id": session.id, "task": "feature-remove-me"}),
            &session,
            &canonical_root,
            None,
            &[],
            &jobs,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("refusing removal"), "{error:#}");
        assert!(target.exists());
        assert!(metadata.exists());
    }

    #[tokio::test]
    async fn managed_worktree_remove_rejects_dirty_and_untracked_work() {
        let (_root, canonical_root, checkout, session) = managed_worktree_removal_fixture();
        let managed_root = canonical_root.join("worktrees/repo");
        let target = managed_root.join("feature-remove-me");

        std::fs::write(target.join("tracked.txt"), "modified\n").unwrap();
        let error = git_worktree_remove_in_src_root_with_snapshots(
            &json!({"session_id": session.id, "task": "feature-remove-me"}),
            &session,
            &canonical_root,
            None,
            &[],
            &[],
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("modified or untracked"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("tracked.txt")).unwrap(),
            "modified\n"
        );

        run_git_fixture(&target, &["checkout", "--", "tracked.txt"]);
        std::fs::write(target.join("untracked.txt"), "keep me\n").unwrap();
        let error = git_worktree_remove_in_src_root_with_snapshots(
            &json!({"session_id": session.id, "task": "feature-remove-me"}),
            &session,
            &canonical_root,
            None,
            &[],
            &[],
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("modified or untracked"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("untracked.txt")).unwrap(),
            "keep me\n"
        );
        assert!(target.exists());
        assert!(checkout.join(".git/worktrees/feature-remove-me").exists());
    }

    #[tokio::test]
    async fn managed_worktree_remove_rejects_unknown_wrong_repo_and_arbitrary_paths() {
        let (_root, canonical_root, checkout, session) = managed_worktree_removal_fixture();
        let managed_root = canonical_root.join("worktrees/repo");

        for args in [
            json!({"session_id": session.id, "task": "missing-task"}),
            json!({"session_id": session.id, "task": "../escape"}),
            json!({"session_id": session.id, "task": "feature/remove-me"}),
            json!({"session_id": session.id, "repository": "other", "task": "feature-remove-me"}),
            json!({"session_id": session.id, "path": "/tmp/escape"}),
            json!({"session_id": session.id, "path": managed_root.join("other-task").to_string_lossy()}),
            json!({"session_id": session.id, "path": "relative/task"}),
            json!({"session_id": session.id}),
            json!({"session_id": session.id, "cwd": "/tmp", "task": "feature-remove-me"}),
            json!({"session_id": session.id, "base": "HEAD", "task": "feature-remove-me"}),
        ] {
            let error = git_worktree_remove_in_src_root_with_snapshots(
                &args,
                &session,
                &canonical_root,
                None,
                &[],
                &[],
            )
            .await
            .unwrap_err();
            assert!(
                !error.to_string().contains("escape/"),
                "unexpected error for {args}: {error:#}"
            );
        }
        // A task and a path that select different managed worktrees fail closed.
        let error = git_worktree_remove_in_src_root_with_snapshots(
            &json!({
                "session_id": session.id,
                "task": "feature-remove-me",
                "path": managed_root.join("feature-sibling").to_string_lossy()
            }),
            &session,
            &canonical_root,
            None,
            &[],
            &[],
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("different managed worktrees"),
            "{error:#}"
        );

        // Nothing was removed and the repository identity is unchanged.
        assert!(managed_root.join("feature-remove-me").exists());
        assert!(managed_root.join("feature-sibling").exists());
        assert!(checkout.join(".git/worktrees/feature-remove-me").exists());
        assert!(checkout.join(".git/worktrees/feature-sibling").exists());
    }

    #[tokio::test]
    async fn managed_worktree_remove_rejects_the_current_session_cwd() {
        let (_root, canonical_root, checkout, session) = managed_worktree_removal_fixture();
        let target = canonical_root.join("worktrees/repo/feature-remove-me");
        let inside = config::Session {
            cwd: target.clone(),
            permitted_directories: vec![checkout.clone(), target.clone()],
            ..session
        };
        let error = git_worktree_remove_in_src_root_with_snapshots(
            &json!({"session_id": inside.id, "task": "feature-remove-me"}),
            &inside,
            &canonical_root,
            None,
            &[],
            &[],
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("in use by"), "{error:#}");
        assert!(target.exists());
        assert!(checkout.join(".git/worktrees/feature-remove-me").exists());
    }

    #[test]
    fn managed_worktree_ownership_is_fail_closed_for_live_sessions_and_jobs() {
        let target = PathBuf::from("/src/worktrees/repo/task");
        let session = config::Session {
            id: "current".to_owned(),
            cwd: PathBuf::from("/src/repo"),
            permitted_directories: vec![PathBuf::from("/src/repo")],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
        };

        // The current session cwd inside the target is an owner.
        let mut nested = session.clone();
        nested.cwd = target.join("sub");
        let ownership = managed_worktree_owners_from(&nested, &target, &[], &[]);
        assert_eq!(ownership.owning_sessions, vec!["current".to_owned()]);

        // Live sibling sessions owning the worktree or a parent path are owners.
        let views = [
            session_view_for_test(
                "live-workspace",
                PathBuf::from("/elsewhere"),
                "active",
                Some(target.clone()),
            ),
            session_view_for_test("live-cwd", target.join("sub"), "starting", None),
            session_view_for_test("unknown", target.clone(), "unknown", None),
            session_view_for_test("stopped", target.clone(), "stopped", Some(target.clone())),
            session_view_for_test("crashed", target.clone(), "crashed", Some(target.clone())),
            session_view_for_test("degraded", target.clone(), "degraded", Some(target.clone())),
            session_view_for_test(
                "unrelated",
                PathBuf::from("/src/other"),
                "active",
                Some(PathBuf::from("/src/other")),
            ),
        ];
        let ownership = managed_worktree_owners_from(&session, &target, &views, &[]);
        assert_eq!(
            ownership.owning_sessions,
            vec![
                "live-cwd".to_owned(),
                "live-workspace".to_owned(),
                "unknown".to_owned()
            ]
        );
        assert!(ownership.owning_jobs.is_empty());

        // A running job whose resolved cwd is inside the target is an owner.
        let jobs = [
            ("job-inside".to_owned(), target.join("sub")),
            ("job-outside".to_owned(), PathBuf::from("/src/other")),
        ];
        let ownership = managed_worktree_owners_from(&session, &target, &[], &jobs);
        assert_eq!(ownership.owning_jobs, vec!["job-inside".to_owned()]);
        assert!(!ownership.is_empty());
        assert!(
            managed_worktree_owners_from(&session, &target, &[], &[]).is_empty(),
            "an unowned worktree must not report owners"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_worktree_remove_rejects_symlinked_targets_and_swapped_roots() {
        let (_root, canonical_root, checkout, session) = managed_worktree_removal_fixture();
        let managed_root = canonical_root.join("worktrees/repo");
        let target = managed_root.join("feature-remove-me");

        // A symlinked managed-root child is never a managed worktree.
        let linked = managed_root.join("linked-task");
        std::os::unix::fs::symlink(&target, &linked).unwrap();
        let error = git_worktree_remove_in_src_root(
            &json!({"session_id": session.id, "task": "linked-task"}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("normal directory"), "{error:#}");
        assert!(target.exists());

        // A swapped managed root disables the managed authority entirely.
        let swapped = canonical_root.join("swapped");
        std::fs::rename(&managed_root, &swapped).unwrap();
        std::os::unix::fs::symlink(&swapped, &managed_root).unwrap();
        let error = git_worktree_remove_in_src_root(
            &json!({"session_id": session.id, "task": "feature-remove-me"}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("trusted normal directory"),
            "{error:#}"
        );
        assert!(swapped.join("feature-remove-me").exists());
        assert!(checkout.join(".git/worktrees/feature-remove-me").exists());
    }

    #[tokio::test]
    async fn managed_worktree_remove_after_remove_is_a_bounded_failure() {
        let (_root, canonical_root, checkout, session) = managed_worktree_removal_fixture();
        let args = json!({"session_id": session.id, "task": "feature-remove-me"});
        git_worktree_remove_in_src_root(&args, &session, &canonical_root, None)
            .await
            .unwrap();

        let error = git_worktree_remove_in_src_root(&args, &session, &canonical_root, None)
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("cannot inspect managed worktree"),
            "{message}"
        );
        // The legacy sibling worktree and the primary checkout are untouched.
        assert!(
            canonical_root
                .join("repo-legacy-linked")
                .join("tracked.txt")
                .exists()
        );
        assert!(checkout.join("tracked.txt").exists());
    }

    #[test]
    fn managed_worktree_removal_targets_never_escape_the_managed_root() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(fixture.path()).unwrap();
        let checkout = src_root.join("repo");
        std::fs::create_dir(&checkout).unwrap();
        let repository =
            managed_worktree::ManagedRepository::resolve(&checkout, &src_root).unwrap();

        test_support::run(0x5257_4d54_4152_4745, 1024, |ctx| {
            let length = noprop::sample_usize_in(ctx, 0..=80);
            let candidate = (0..length)
                .map(|_| match noprop::sample_usize_in(ctx, 0..=7) {
                    0 => '/',
                    1 => '.',
                    2 => '\\',
                    3 => '-',
                    4 => '_',
                    5 => 'a',
                    6 => ':',
                    _ => char::from_u32(0x20 + noprop::sample_u32(ctx) % 95).unwrap(),
                })
                .collect::<String>();
            if let Ok((task, target)) =
                resolve_managed_worktree_target(&repository, Some(&candidate), None)
            {
                assert_eq!(target.parent(), Some(repository.managed_root()));
                assert!(target.starts_with(repository.managed_root()));
                assert_eq!(target.file_name().unwrap().to_str(), Some(task.as_str()));
                managed_worktree::validate_task_name(&task).unwrap();
            }
            // A caller path never grants authority: any accepted path is the
            // exact derived direct child of the trusted managed root.
            if let Ok((_, target)) =
                resolve_managed_worktree_target(&repository, None, Some(&candidate))
            {
                assert_eq!(target.parent(), Some(repository.managed_root()));
                assert!(target.starts_with(repository.managed_root()));
            }
            Ok(())
        })
    }

    #[tokio::test]
    async fn local_agent_worktree_binding_rejects_path_injection_and_wrong_reuse() {
        let (_root, canonical_root, _checkout, session) = managed_worktree_fixture();

        let injection = local_agent_managed_worktree_binding_with_src_root(
            &json!({
                "session_id": session.id,
                "cwd": "/tmp",
                "worktree": {"branch": "main"}
            }),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(injection.to_string().contains("not both"), "{injection:#}");

        let unknown = local_agent_managed_worktree_binding_with_src_root(
            &json!({
                "session_id": session.id,
                "worktree": {"branch": "main", "path": "/tmp/escape"}
            }),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            unknown.to_string().contains("only branch and task"),
            "{unknown:#}"
        );

        // A legacy worktree is never adopted through the derived task path.
        #[cfg(unix)]
        {
            let legacy_target = canonical_root.join("worktrees/repo/legacy-task");
            std::fs::create_dir_all(legacy_target.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(canonical_root.join("repo-legacy-linked"), &legacy_target)
                .unwrap();
            let legacy = local_agent_managed_worktree_binding_with_src_root(
                &json!({
                    "session_id": session.id,
                    "worktree": {"branch": "sibling-branch", "task": "legacy-task"}
                }),
                &session,
                &canonical_root,
                None,
            )
            .await
            .unwrap_err();
            assert!(
                legacy.to_string().contains("cannot be reused"),
                "{legacy:#}"
            );
            assert!(
                std::fs::symlink_metadata(&legacy_target)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[tokio::test]
    async fn local_agent_worktree_binding_denied_approval_creates_nothing() {
        let (_root, canonical_root, checkout, session) = managed_worktree_fixture();
        let ask_session = config::Session {
            permission_mode: config::PermissionMode::Ask,
            ..session
        };
        let worktrees_before = git_fixture_stdout(&checkout, &["worktree", "list", "--porcelain"]);

        let error = local_agent_managed_worktree_binding_with_src_root(
            &json!({"session_id": ask_session.id, "worktree": {"branch": "main", "task": "denied"}}),
            &ask_session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("not running"), "{error:#}");
        assert!(!canonical_root.join("worktrees/repo/denied").exists());
        assert_eq!(
            git_fixture_stdout(&checkout, &["worktree", "list", "--porcelain"]),
            worktrees_before
        );
    }

    /// Host acceptance (nested Linux/macOS sandbox required): the structured
    /// local agent run must start inside the Temote-derived managed worktree
    /// with no caller-supplied path, while the primary checkout is unchanged.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn local_agent_run_binds_a_managed_worktree_without_a_caller_path() {
        let fixture = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(fixture.path()).unwrap();
        let checkout = src_root.join("repo");
        std::fs::create_dir(&checkout).unwrap();
        init_git_repository(&checkout);
        std::fs::write(checkout.join("untracked.txt"), "keep\n").unwrap();
        run_git_fixture(&checkout, &["branch", "feature/foo/bar"]);

        let id = format!("local-agent-worktree-{}", Uuid::new_v4());
        let (sender, _receiver) = approvals::approval_channel();
        let runtime = approvals::spawn_runtime_with_logical_path_and_environment(
            &checkout,
            Some(&id),
            config::PermissionMode::Agent,
            sender,
            None,
            approvals::CapturedStartEnvironment::default(),
        )
        .await
        .unwrap();
        let session = config::load_session(&id).await.unwrap();

        let fake_dir = tempfile::tempdir().unwrap();
        let fake_agent = activity_delegated_job_executable(
            fake_dir.path(),
            "codex",
            "#!/bin/sh\npwd > ran-in.txt\nprintf 'managed-worktree-agent\\n'\n",
        );
        let result = local_agent_run_with_src_root(
            &json!({
                "session_id": id,
                "agent": "codex",
                "task": "implement the managed worktree task",
                "access": "workspace_write",
                "worktree": {"branch": "feature/foo/bar"}
            }),
            &session,
            Some(&fake_agent),
            None,
            Some(&src_root),
        )
        .await
        .unwrap();
        assert!(
            serde_json::to_string(&result)
                .unwrap()
                .contains("managed-worktree-agent")
        );

        let target = src_root.join("worktrees/repo/feature-foo-bar");
        assert_eq!(
            std::fs::read_to_string(target.join("ran-in.txt"))
                .unwrap()
                .trim(),
            target.to_string_lossy()
        );
        assert_eq!(
            git_fixture_stdout(&checkout, &["branch", "--show-current"]),
            "main"
        );
        assert!(
            git_fixture_stdout(&checkout, &["status", "--porcelain"]).contains("?? untracked.txt")
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn local_agent_worktree_binding_requires_the_primary_checkout_authority() {
        let (_root, canonical_root, checkout, session) = managed_worktree_fixture();
        run_git_fixture(&checkout, &["branch", "feature/foo/bar"]);
        // The session only permits a legacy worktree, so it has no authority
        // over the canonical primary checkout that anchors the managed root.
        let legacy = canonical_root.join("repo-legacy-linked");
        let legacy_session = config::Session {
            cwd: legacy.clone(),
            permitted_directories: vec![legacy.clone()],
            ..session.clone()
        };

        let error = local_agent_managed_worktree_binding_with_src_root(
            &json!({
                "session_id": legacy_session.id,
                "worktree": {"branch": "feature/foo/bar"}
            }),
            &legacy_session,
            &canonical_root,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("permitted session root"),
            "{error:#}"
        );
        assert!(!canonical_root.join("worktrees").exists());
    }

    /// The managed binding is re-derived from filesystem state immediately
    /// before launch: a swapped target, managed root, `.git` pointer or
    /// repository identity must fail closed instead of spawning in an
    /// unauthorized workspace.
    #[cfg(unix)]
    #[tokio::test]
    async fn local_agent_worktree_binding_revalidates_swapped_workspace_identity() {
        use std::os::unix::fs::symlink;

        let (_root, canonical_root, checkout, session) = managed_worktree_fixture();
        run_git_fixture(&checkout, &["branch", "feature/foo/bar"]);
        let managed_root = canonical_root.join("worktrees").join("repo");
        let target = managed_root.join("feature-foo-bar");
        let binding = local_agent_managed_worktree_binding_with_src_root(
            &json!({"session_id": session.id, "worktree": {"branch": "feature/foo/bar"}}),
            &session,
            &canonical_root,
            None,
        )
        .await
        .unwrap()
        .expect("managed worktree binding");
        binding.revalidate(&canonical_root).unwrap();

        // A target swapped for a symbolic link is never trusted again.
        let moved = managed_root.join("moved-away");
        std::fs::rename(&target, &moved).unwrap();
        symlink(&moved, &target).unwrap();
        let error = binding.revalidate(&canonical_root).unwrap_err();
        assert!(error.to_string().contains("normal directory"), "{error:#}");
        std::fs::remove_file(&target).unwrap();
        std::fs::rename(&moved, &target).unwrap();
        binding.revalidate(&canonical_root).unwrap();

        // A swapped managed root is no longer the trusted authority.
        let real_root = canonical_root.join("worktrees").join("repo-real");
        std::fs::rename(&managed_root, &real_root).unwrap();
        symlink(&real_root, &managed_root).unwrap();
        let error = binding.revalidate(&canonical_root).unwrap_err();
        assert!(
            error.to_string().contains("trusted normal directory"),
            "{error:#}"
        );
        std::fs::remove_file(&managed_root).unwrap();
        std::fs::rename(&real_root, &managed_root).unwrap();
        binding.revalidate(&canonical_root).unwrap();

        // A `.git` pointer swapped to another repository's structurally valid
        // private metadata keeps the target path but changes the identity.
        let other = canonical_root.join("other-repo");
        std::fs::create_dir(&other).unwrap();
        init_git_repository(&other);
        let other_linked = canonical_root.join("other-linked");
        run_git_fixture(
            &other,
            &[
                "worktree",
                "add",
                "--quiet",
                other_linked.to_str().unwrap(),
                "-b",
                "other-branch",
            ],
        );
        let other_private = other.join(".git").join("worktrees").join("other-linked");
        let original_pointer = std::fs::read_to_string(target.join(".git")).unwrap();
        let original_other_gitdir = std::fs::read_to_string(other_private.join("gitdir")).unwrap();
        std::fs::write(
            target.join(".git"),
            format!("gitdir: {}\n", other_private.display()),
        )
        .unwrap();
        std::fs::write(
            other_private.join("gitdir"),
            format!("{}\n", target.join(".git").display()),
        )
        .unwrap();
        let error = binding.revalidate(&canonical_root).unwrap_err();
        assert!(
            error.to_string().contains("common Git directory mismatch"),
            "{error:#}"
        );
        std::fs::write(target.join(".git"), &original_pointer).unwrap();
        std::fs::write(other_private.join("gitdir"), &original_other_gitdir).unwrap();
        binding.revalidate(&canonical_root).unwrap();

        // Another repository's worktree moved into the expected path is a
        // different repository identity, not a reusable managed worktree.
        let backup = managed_root.join("backup");
        std::fs::rename(&target, &backup).unwrap();
        run_git_fixture(
            &other,
            &[
                "worktree",
                "add",
                "--quiet",
                target.to_str().unwrap(),
                "-b",
                "other-replacement",
            ],
        );
        let error = binding.revalidate(&canonical_root).unwrap_err();
        assert!(
            error.to_string().contains("common Git directory mismatch"),
            "{error:#}"
        );
        run_git_fixture(
            &other,
            &["worktree", "remove", "--force", target.to_str().unwrap()],
        );
        std::fs::rename(&backup, &target).unwrap();
        binding.revalidate(&canonical_root).unwrap();
    }

    /// A rejected managed-worktree binding must fail before the agent is
    /// prepared or spawned, even when a wrong-branch managed target already
    /// exists at the derived path.
    #[tokio::test]
    async fn local_agent_run_worktree_binding_failure_starts_no_agent() {
        let (_root, canonical_root, checkout, session) = managed_worktree_fixture();
        run_git_fixture(&checkout, &["branch", "feature/foo/bar"]);
        run_git_fixture(&checkout, &["branch", "wrong-branch"]);
        let target = canonical_root.join("worktrees/repo/feature-foo-bar");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        run_git_fixture(
            &checkout,
            &[
                "worktree",
                "add",
                "--quiet",
                target.to_str().unwrap(),
                "wrong-branch",
            ],
        );
        let target_before = worktree_snapshot(&target);
        let checkout_before = worktree_snapshot(&checkout);

        let fake_dir = tempfile::tempdir().unwrap();
        let fake_agent = activity_delegated_job_executable(
            fake_dir.path(),
            "codex",
            "#!/bin/sh\npwd > ran-in.txt\nprintf 'must-not-run\\n'\n",
        );
        let error = local_agent_run_with_src_root(
            &json!({
                "session_id": session.id,
                "agent": "codex",
                "task": "must not start",
                "access": "workspace_write",
                "worktree": {"branch": "feature/foo/bar"}
            }),
            &session,
            Some(&fake_agent),
            None,
            Some(&canonical_root),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("cannot be reused"), "{error:#}");
        assert!(!target.join("ran-in.txt").exists());
        assert!(!checkout.join("ran-in.txt").exists());
        assert_eq!(worktree_snapshot(&target), target_before);
        assert_eq!(worktree_snapshot(&checkout), checkout_before);
    }

    /// The approval boundary is not an authority transfer: the workspace
    /// identity is re-derived and re-verified after approval, so a target whose
    /// `.git` pointer is swapped while the approval is pending fails closed and
    /// the agent never starts.
    #[cfg(unix)]
    #[tokio::test]
    async fn local_agent_run_worktree_binding_swapped_during_approval_starts_no_agent() {
        let fixture = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(fixture.path()).unwrap();
        let checkout = src_root.join("repo");
        std::fs::create_dir(&checkout).unwrap();
        init_git_repository(&checkout);
        run_git_fixture(&checkout, &["branch", "feature/foo/bar"]);
        std::fs::write(checkout.join("untracked.txt"), "keep\n").unwrap();
        let managed_root = src_root.join("worktrees").join("repo");
        std::fs::create_dir_all(&managed_root).unwrap();
        let target = managed_root.join("feature-foo-bar");
        run_git_fixture(
            &checkout,
            &[
                "worktree",
                "add",
                "--quiet",
                target.to_str().unwrap(),
                "feature/foo/bar",
            ],
        );

        // A structurally valid private metadata directory of another
        // repository, prepared before the run so the swap itself is a pair of
        // bounded writes while approval is pending.
        let other = src_root.join("other-repo");
        std::fs::create_dir(&other).unwrap();
        init_git_repository(&other);
        let other_linked = src_root.join("other-linked");
        run_git_fixture(
            &other,
            &[
                "worktree",
                "add",
                "--quiet",
                other_linked.to_str().unwrap(),
                "-b",
                "other-branch",
            ],
        );
        let other_private = other.join(".git").join("worktrees").join("other-linked");

        let id = format!("wt-approval-{}", Uuid::new_v4());
        let (sender, mut receiver) = approvals::approval_channel();
        let runtime = approvals::spawn_runtime_with_logical_path_and_environment(
            &checkout,
            Some(&id),
            config::PermissionMode::Ask,
            sender,
            None,
            approvals::CapturedStartEnvironment::default(),
        )
        .await
        .unwrap();
        let session = config::load_session(&id).await.unwrap();

        let fake_dir = tempfile::tempdir().unwrap();
        let fake_agent = activity_delegated_job_executable(
            fake_dir.path(),
            "codex",
            "#!/bin/sh\npwd > ran-in.txt\nprintf 'must-not-run\\n'\n",
        );
        let args = json!({
            "session_id": id,
            "agent": "codex",
            "task": "approval-time identity swap",
            "access": "workspace_write",
            "worktree": {"branch": "feature/foo/bar"}
        });
        let run_root = src_root.clone();
        let run_session = session.clone();
        let task = tokio::spawn(async move {
            let _fixture = fixture;
            let _fake_dir = fake_dir;
            local_agent_run_with_src_root(
                &args,
                &run_session,
                Some(&fake_agent),
                None,
                Some(&run_root),
            )
            .await
        });

        let prompt = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("local_agent_run did not request approval")
            .expect("approval channel closed before local_agent_run request");
        assert_eq!(prompt.request.operation, "local_agent_run");
        // The approval identifies the Temote-derived managed workspace, never a
        // caller-supplied path. Workspace classification reads the configured
        // named root from the environment, which the `_with_src_root` test seam
        // intentionally bypasses, so only the identity fields are asserted.
        assert!(
            prompt
                .request
                .detail
                .contains(&format!("workspace_root: {}", target.display())),
            "{}",
            prompt.request.detail
        );
        assert!(
            prompt.request.detail.contains("repository: repo"),
            "{}",
            prompt.request.detail
        );
        assert!(
            prompt.request.detail.contains("branch: feature/foo/bar"),
            "{}",
            prompt.request.detail
        );

        std::fs::write(
            target.join(".git"),
            format!("gitdir: {}\n", other_private.display()),
        )
        .unwrap();
        std::fs::write(
            other_private.join("gitdir"),
            format!("{}\n", target.join(".git").display()),
        )
        .unwrap();
        prompt.respond(true);

        let error = task
            .await
            .unwrap()
            .expect_err("swapped workspace unexpectedly started the agent");
        assert!(
            error.to_string().contains("common Git directory mismatch"),
            "{error:#}"
        );
        assert!(!target.join("ran-in.txt").exists());
        assert!(!checkout.join("ran-in.txt").exists());
        runtime.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    const R3_APPROVAL_IDENTITY_TEST_NAME: &str =
        "mcp::tests::local_agent_worktree_approval_identity_uses_the_configured_src_named_root";

    /// Child-process body for the configured named-root integration test.
    ///
    /// Runs the production authority path (`local_agent_managed_worktree_binding`
    /// reads `TEMOTE_MCP_ROOTS` from the environment) and asserts the approval
    /// detail and metadata identity derived from it.
    #[cfg(unix)]
    fn run_r3_approval_identity_fixture() -> Result<()> {
        const SRC: &str = "TEMOTE_TEST_R3_SRC_ROOT";
        let src_root = PathBuf::from(std::env::var(SRC).context("missing src root")?);
        let src_root = std::fs::canonicalize(&src_root)?;
        let checkout = src_root.join("repo");
        let session = config::Session {
            id: format!("r3-{}", Uuid::new_v4()),
            cwd: checkout.clone(),
            permitted_directories: vec![checkout.clone()],
            started_at: 1,
            process_id: 1,
            permission_mode: config::PermissionMode::Agent,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("cannot start the R3 child runtime")?;
        runtime.block_on(async move {
            let binding = local_agent_managed_worktree_binding(
                &json!({"session_id": session.id, "worktree": {"branch": "feature/r3"}}),
                &session,
                None,
            )
            .await?
            .expect("managed worktree binding");
            let target = src_root.join("worktrees").join("repo").join("feature-r3");
            assert_eq!(binding.workspace_root(), target);

            // The classification uses the same configured named root as the
            // production binding resolution.
            let workspace = managed_worktree::inspect_session_workspace(
                binding.workspace_root(),
                managed_worktree::configured_src_root_from_env().as_deref(),
            )
            .expect("workspace identity");
            assert_eq!(
                workspace.workspace_type,
                managed_worktree::SessionWorkspaceType::ManagedWorktree
            );
            assert_eq!(workspace.repository_root, checkout);
            assert_eq!(workspace.workspace_root, target);
            assert_eq!(workspace.repository.as_deref(), Some("repo"));
            assert_eq!(workspace.branch.as_deref(), Some("feature/r3"));
            assert_eq!(workspace.task.as_deref(), Some("feature-r3"));

            // The branch comes from the linked worktree's own HEAD, never from
            // the primary checkout's HEAD.
            assert_eq!(
                sandbox::git_current_branch(&target)?.as_deref(),
                Some("feature/r3")
            );
            assert_eq!(
                sandbox::git_current_branch(&checkout)?.as_deref(),
                Some("main")
            );

            let run_session = binding.run_session(&session);
            assert_eq!(
                run_session.permitted_directories,
                vec![checkout.clone(), target.clone()]
            );
            assert_eq!(session.cwd, checkout);
            assert_eq!(session.permitted_directories, vec![checkout.clone()]);

            let fake_dir = tempfile::tempdir()?;
            let fake_agent = activity_delegated_job_executable(
                fake_dir.path(),
                "codex",
                "#!/bin/sh\nprintf 'r3\\n'\n",
            );
            let effective_args = json!({
                "session_id": session.id,
                "agent": "codex",
                "task": "r3 identity",
                "access": "workspace_write",
                "cwd": target.to_string_lossy(),
            });
            let prepared =
                local_agent::prepare_with_executable(&effective_args, &run_session, &fake_agent)?;
            assert_eq!(prepared.cwd, target);

            let detail = prepared.approval_detail();
            assert!(
                detail.contains("workspace_type: managed_worktree"),
                "{detail}"
            );
            assert!(
                detail.contains(&format!("repository_root: {}", checkout.display())),
                "{detail}"
            );
            assert!(
                detail.contains(&format!("workspace_root: {}", target.display())),
                "{detail}"
            );
            assert!(detail.contains("repository: repo"), "{detail}");
            assert!(detail.contains("branch: feature/r3"), "{detail}");
            assert!(detail.contains("task: feature-r3"), "{detail}");

            let metadata = prepared.approval_metadata();
            let keys = metadata.keys().cloned().collect::<Vec<_>>();
            assert_eq!(
                keys,
                vec![
                    "access",
                    "agent",
                    "branch",
                    "cwd",
                    "provenance",
                    "repository",
                    "repository_root",
                    "scope",
                    "source",
                    "task",
                    "task_bytes",
                    "task_sha256",
                    "workspace_root",
                    "workspace_type",
                ]
            );
            assert_eq!(metadata["workspace_type"], "managed_worktree");
            assert_eq!(
                metadata["repository_root"],
                checkout.to_string_lossy().as_ref()
            );
            assert_eq!(
                metadata["workspace_root"],
                target.to_string_lossy().as_ref()
            );
            assert_eq!(metadata["repository"], "repo");
            assert_eq!(metadata["branch"], "feature/r3");
            assert_eq!(metadata["task"], "feature-r3");
            assert!(
                !metadata
                    .values()
                    .any(|value| value.contains("TEMOTE_MCP") || value.contains("keep me")),
                "{metadata:?}"
            );
            Ok(())
        })
    }

    /// R3: the configured `TEMOTE_MCP_ROOTS` production path must classify the
    /// derived workspace as a managed worktree and report the validated
    /// identity in the approval detail/metadata. The child process isolates the
    /// process-global environment from parallel tests.
    #[cfg(unix)]
    #[test]
    fn local_agent_worktree_approval_identity_uses_the_configured_src_named_root() {
        const ROLE: &str = "TEMOTE_TEST_R3_APPROVAL_IDENTITY_ROLE";
        const SRC: &str = "TEMOTE_TEST_R3_SRC_ROOT";

        if std::env::var(ROLE).as_deref() == Ok("fixture") {
            run_r3_approval_identity_fixture().expect("R3 child fixture failed");
            println!("R3-APPROVAL-IDENTITY-OK");
            return;
        }

        let fixture = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(fixture.path()).unwrap();
        let checkout = src_root.join("repo");
        std::fs::create_dir(&checkout).unwrap();
        init_git_repository(&checkout);
        run_git_fixture(&checkout, &["branch", "feature/r3"]);
        std::fs::write(checkout.join("untracked.txt"), "keep me\n").unwrap();

        let current_exe = std::env::current_exe().unwrap();
        let output = std::process::Command::new(current_exe)
            .arg("--exact")
            .arg(R3_APPROVAL_IDENTITY_TEST_NAME)
            .arg("--nocapture")
            .env(ROLE, "fixture")
            .env(SRC, src_root.to_string_lossy().into_owned())
            .env("TEMOTE_MCP_ROOTS", format!("src={}", src_root.display()))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("R3-APPROVAL-IDENTITY-OK"),
            "child did not complete\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// Observable state of one repository after the attacker-equivalent swap
    /// and immediately before the launch boundary for a regression case.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[derive(Clone, Debug, PartialEq)]
    struct RepositorySnapshot {
        head: String,
        refs: String,
        status: String,
        config: String,
        worktrees: String,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn repository_snapshot(worktree: &Path) -> RepositorySnapshot {
        let common = sandbox::git_common_dir(worktree).unwrap();
        RepositorySnapshot {
            head: git_fixture_stdout(worktree, &["rev-parse", "HEAD"]),
            refs: git_fixture_stdout(
                worktree,
                &["for-each-ref", "--format=%(refname) %(objectname)"],
            ),
            status: git_fixture_stdout(worktree, &["status", "--porcelain"]),
            config: std::fs::read_to_string(common.join("config")).unwrap(),
            worktrees: git_fixture_stdout(worktree, &["worktree", "list", "--porcelain"]),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct BoundaryRun {
        result: Result<Value>,
        _root: tempfile::TempDir,
        canonical_root: PathBuf,
        checkout: PathBuf,
        target: PathBuf,
        session: config::Session,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl BoundaryRun {
        /// The launch must fail before any child result exists: the tool
        /// reports the bounded sandbox-setup failure and never the fake
        /// agent's stdout.
        fn assert_launch_failed_before_child(&self) {
            let error = self
                .result
                .as_ref()
                .expect_err("a swapped managed-worktree identity must not start the agent");
            let text = error.to_string();
            assert!(text.contains("sandbox_setup_failed"), "{text}");
            assert!(
                text.contains("local agent failed before a child result was available"),
                "{text}"
            );
            assert!(!text.contains("boundary-agent"), "{text}");
        }
    }

    /// Runs the production `local_agent_run` path with a managed worktree and
    /// invokes `swap` at the last validation boundary: after the validated
    /// repository identity has been attached to the prepared run and before
    /// the local agent task starts.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn run_managed_worktree_at_launch_boundary<F>(swap: F) -> BoundaryRun
    where
        F: FnOnce(&Path, &Path),
    {
        let (root, canonical_root, checkout, _fixture_session) = managed_worktree_fixture();
        run_git_fixture(&checkout, &["branch", "feature/foo/bar"]);
        let target = canonical_root
            .join("worktrees")
            .join("repo")
            .join("feature-foo-bar");
        let id = format!("wt-boundary-{}", Uuid::new_v4());
        let (sender, _receiver) = approvals::approval_channel();
        let runtime = approvals::spawn_runtime_with_logical_path_and_environment(
            &checkout,
            Some(&id),
            config::PermissionMode::Agent,
            sender,
            None,
            approvals::CapturedStartEnvironment::default(),
        )
        .await
        .unwrap();
        let session = config::load_session(&id).await.unwrap();
        let fake_dir = tempfile::tempdir().unwrap();
        let fake_agent = activity_delegated_job_executable(
            fake_dir.path(),
            "codex",
            "#!/bin/sh\npwd > ran-in.txt\nprintf 'boundary-agent\\n'\n",
        );
        let args = json!({
            "session_id": id,
            "agent": "codex",
            "task": "launch boundary swap",
            "access": "workspace_write",
            "worktree": {"branch": "feature/foo/bar"}
        });
        let swap_target = target.clone();
        let swap_root = canonical_root.clone();
        let result = local_agent_run_with_src_root_at_boundary(
            &args,
            &session,
            Some(&fake_agent),
            None,
            Some(&canonical_root),
            move || swap(&swap_target, &swap_root),
        )
        .await;
        runtime.shutdown().await.unwrap();
        BoundaryRun {
            result,
            _root: root,
            canonical_root,
            checkout,
            target,
            session,
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn local_agent_worktree_binding_runs_in_the_validated_workspace() {
        let run = run_managed_worktree_at_launch_boundary(|_target, _canonical_root| {}).await;
        let result = run
            .result
            .expect("a validated managed worktree must start the agent");
        assert!(
            serde_json::to_string(&result)
                .unwrap()
                .contains("boundary-agent")
        );
        assert!(run.target.join("ran-in.txt").is_file());
        assert_eq!(
            git_fixture_stdout(&run.target, &["branch", "--show-current"]),
            "feature/foo/bar"
        );
        assert!(!run.checkout.join("ran-in.txt").exists());
        assert_eq!(
            run.session.permitted_directories,
            vec![run.checkout.clone()]
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn local_agent_worktree_identity_swap_to_other_repository_metadata_starts_no_agent() {
        let snapshot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let recorded = std::rc::Rc::clone(&snapshot);
        let run = run_managed_worktree_at_launch_boundary(move |target, canonical_root| {
            let other = canonical_root.join("other-repo");
            std::fs::create_dir(&other).unwrap();
            init_git_repository(&other);
            let other_linked = canonical_root.join("other-linked");
            run_git_fixture(
                &other,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    other_linked.to_str().unwrap(),
                    "-b",
                    "other-branch",
                ],
            );
            let other_private = other.join(".git").join("worktrees").join("other-linked");
            std::fs::write(
                target.join(".git"),
                format!("gitdir: {}\n", other_private.display()),
            )
            .unwrap();
            std::fs::write(
                other_private.join("gitdir"),
                format!("{}\n", target.join(".git").display()),
            )
            .unwrap();
            *recorded.borrow_mut() = Some(repository_snapshot(&other));
        })
        .await;

        run.assert_launch_failed_before_child();
        assert!(!run.target.join("ran-in.txt").exists());
        assert!(!run.checkout.join("ran-in.txt").exists());
        let other = run.canonical_root.join("other-repo");
        let before = snapshot.borrow().clone().expect("recorded B snapshot");
        assert_eq!(repository_snapshot(&other), before);
        assert_eq!(
            run.session.permitted_directories,
            vec![run.checkout.clone()]
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn local_agent_worktree_identity_swap_to_other_repository_worktree_starts_no_agent() {
        let snapshot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let recorded = std::rc::Rc::clone(&snapshot);
        let run = run_managed_worktree_at_launch_boundary(move |target, canonical_root| {
            let other = canonical_root.join("other-repo");
            std::fs::create_dir(&other).unwrap();
            init_git_repository(&other);
            run_git_fixture(&other, &["branch", "other-replacement"]);
            std::fs::rename(target, canonical_root.join("worktrees/repo/backup")).unwrap();
            run_git_fixture(
                &other,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    target.to_str().unwrap(),
                    "other-replacement",
                ],
            );
            *recorded.borrow_mut() = Some(repository_snapshot(&other));
        })
        .await;

        run.assert_launch_failed_before_child();
        assert!(!run.target.join("ran-in.txt").exists());
        assert!(
            !run.canonical_root
                .join("worktrees/repo/backup/ran-in.txt")
                .exists()
        );
        assert!(!run.checkout.join("ran-in.txt").exists());
        let other = run.canonical_root.join("other-repo");
        let before = snapshot.borrow().clone().expect("recorded B snapshot");
        assert_eq!(repository_snapshot(&other), before);
        assert_eq!(
            run.session.permitted_directories,
            vec![run.checkout.clone()]
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn local_agent_worktree_identity_symlink_target_swap_starts_no_agent() {
        let run = run_managed_worktree_at_launch_boundary(move |target, canonical_root| {
            let moved = canonical_root.join("worktrees/repo/moved");
            std::fs::rename(target, &moved).unwrap();
            std::os::unix::fs::symlink(&moved, target).unwrap();
        })
        .await;

        run.assert_launch_failed_before_child();
        let moved = run.canonical_root.join("worktrees/repo/moved");
        assert!(!moved.join("ran-in.txt").exists());
        assert!(!run.target.join("ran-in.txt").exists());
        assert!(!run.checkout.join("ran-in.txt").exists());
        assert_eq!(
            run.session.permitted_directories,
            vec![run.checkout.clone()]
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn local_agent_worktree_identity_symlink_managed_root_swap_starts_no_agent() {
        let run = run_managed_worktree_at_launch_boundary(move |_target, canonical_root| {
            let managed_root = canonical_root.join("worktrees/repo");
            let real_root = canonical_root.join("worktrees/repo-real");
            std::fs::rename(&managed_root, &real_root).unwrap();
            std::os::unix::fs::symlink(&real_root, &managed_root).unwrap();
        })
        .await;

        run.assert_launch_failed_before_child();
        let real_root = run.canonical_root.join("worktrees/repo-real");
        assert!(!real_root.join("feature-foo-bar/ran-in.txt").exists());
        assert!(!run.target.join("ran-in.txt").exists());
        assert!(!run.checkout.join("ran-in.txt").exists());
        assert_eq!(
            run.session.permitted_directories,
            vec![run.checkout.clone()]
        );
    }

    #[test]
    fn github_repository_and_workflow_inputs_are_bounded_and_repo_scoped() {
        for (remote, owner, repo) in [
            ("https://github.com/openai/example.git", "openai", "example"),
            ("git@github.com:openai/example.git", "openai", "example"),
            ("ssh://git@github.com/openai/example", "openai", "example"),
        ] {
            assert_eq!(
                github_repository_from_remote_url(remote).unwrap(),
                GithubRepository {
                    owner: owner.to_owned(),
                    repo: repo.to_owned(),
                }
            );
        }
        for remote in [
            "https://gitlab.com/openai/example.git",
            "https://token@github.com/openai/example.git",
            "https://github.com/openai/example/extra.git",
            "https://github.com/openai/../example.git",
        ] {
            assert!(
                github_repository_from_remote_url(remote).is_err(),
                "{remote}"
            );
        }

        for workflow in ["release.yml", "release.yaml", "123456"] {
            validate_github_workflow(workflow).unwrap();
        }
        for workflow in [
            "",
            "release",
            "../release.yml",
            ".github/workflows/release.yml",
        ] {
            assert!(validate_github_workflow(workflow).is_err(), "{workflow}");
        }
        for git_ref in ["main", "latest", "release/2026.09"] {
            validate_github_ref(git_ref).unwrap();
        }
        for git_ref in ["", "-main", "refs/heads/main", "bad\nref"] {
            assert!(validate_github_ref(git_ref).is_err(), "{git_ref:?}");
        }
        assert_eq!(validate_github_run_id("123").unwrap(), 123);
        for run_id in ["", "0", "-1", "1.5", "18446744073709551616"] {
            assert!(validate_github_run_id(run_id).is_err(), "{run_id}");
        }
    }

    #[test]
    fn github_workflow_paths_and_responses_are_bounded() {
        let repository = GithubRepository {
            owner: "openai".to_owned(),
            repo: "example".to_owned(),
        };
        assert_eq!(repository.owner, "openai");
        assert_eq!(repository.repo, "example");
        assert_eq!(
            github_workflow_dispatch_path(&repository, "release.yml"),
            "repos/openai/example/actions/workflows/release.yml/dispatches"
        );
        assert_eq!(
            github_workflow_run_get_path(&repository, 42),
            "repos/openai/example/actions/runs/42"
        );
        assert_eq!(GithubApiMethod::Get.as_str(), "GET");
        assert_eq!(GithubApiMethod::Post.as_str(), "POST");

        assert_eq!(
            parse_github_workflow_dispatch_response(
                r#"{"workflow_run_id":42,"run_url":"https://api.github.com/repos/openai/example/actions/runs/42","html_url":"https://github.com/openai/example/actions/runs/42"}"#,
            )
            .unwrap(),
            json!({
                "workflow_run_id": "42",
                "html_url": "https://github.com/openai/example/actions/runs/42",
            })
        );
        assert_eq!(
            parse_github_workflow_run_response(
                r#"{"id":42,"status":"in_progress","conclusion":null,"event":"workflow_dispatch","head_sha":"0123456789abcdef0123456789abcdef01234567","html_url":"https://github.com/openai/example/actions/runs/42"}"#,
                42,
            )
            .unwrap(),
            json!({
                "run_id": "42",
                "status": "in_progress",
                "conclusion": Value::Null,
                "event": "workflow_dispatch",
                "head_sha": "0123456789abcdef0123456789abcdef01234567",
                "html_url": "https://github.com/openai/example/actions/runs/42",
            })
        );
        assert!(parse_github_workflow_run_response(
            r#"{"id":43,"status":"completed","conclusion":"success","event":"workflow_dispatch","head_sha":"0123456789abcdef0123456789abcdef01234567","html_url":"https://github.com/openai/example/actions/runs/43"}"#,
            42,
        )
        .is_err());
        assert_eq!(
            github_api_error_message(401),
            "GitHub repository credential was rejected"
        );
        assert_eq!(
            github_api_error_message(403),
            "GitHub repository credential lacks required permission"
        );
        assert_eq!(
            github_api_error_message(404),
            "GitHub repository workflow/run is unavailable"
        );
        assert_eq!(
            github_api_error_message(422),
            "GitHub workflow request was rejected"
        );
        assert_eq!(github_api_error_message(500), "GitHub API operation failed");
    }

    #[test]
    fn github_repository_credential_mapping_requires_managed_repo_local_helper() {
        assert!(repo_scoped_github_credential_mapping_valid(
            "\n!gh git credential --managed\n",
            "true\n",
        ));
        assert_eq!(
            github_managed_credential_command(),
            ["gh", "git", "credential", "--managed", "get"]
        );
        for helpers in [
            "!gh git credential --managed\n",
            "\nstore\n!gh git credential --managed\n",
            "\n!gh auth token\n",
            "\n!gh git credential --managed\nstore\n",
            "",
        ] {
            assert!(
                !repo_scoped_github_credential_mapping_valid(helpers, "true\n"),
                "unexpectedly accepted helpers {helpers:?}"
            );
        }
        for use_http_path in ["false\n", "", "1\n"] {
            assert!(!repo_scoped_github_credential_mapping_valid(
                "\n!gh git credential --managed\n",
                use_http_path,
            ));
        }
    }

    #[test]
    fn generated_github_credential_mapping_rejects_extra_repo_local_helpers() -> noprop::TestResult
    {
        test_support::run(0x4748_4d41_5050_494e, 1024, |ctx| {
            let prefix_count = noprop::sample_usize_in(ctx, 0..8);
            let mut local = String::new();
            for index in 0..prefix_count {
                local.push_str(&format!("helper-{index}\n"));
            }
            local.push_str("\n!gh git credential --managed\n");
            assert_eq!(
                repo_scoped_github_credential_mapping_valid(&local, "true\n"),
                prefix_count == 0
            );
            Ok(())
        })
    }

    #[test]
    fn github_repository_credential_parser_is_repo_bound_and_secret_safe() {
        let secret = "ghp_secret_sentinel_123";
        let valid =
            format!("protocol=https\nhost=github.com\nusername=f4ah6o\npassword={secret}\n\n");
        assert_eq!(parse_repo_scoped_github_credential(&valid).unwrap(), secret);

        for invalid in [
            format!("protocol=https\nhost=gitlab.com\nusername=user\npassword={secret}\n"),
            format!(
                "protocol=https\nhost=github.com\nusername=user\npassword={secret}\npassword=second\n"
            ),
            "protocol=https\nhost=github.com\nusername=user\npassword=\n".to_owned(),
        ] {
            let error = parse_repo_scoped_github_credential(&invalid).unwrap_err();
            assert!(
                !error.to_string().contains(secret),
                "credential error leaked the secret: {error:#}"
            );
        }
    }

    #[test]
    fn generated_github_credential_parser_never_echoes_secret_on_mismatch() -> noprop::TestResult {
        test_support::run(0x4748_4352_4544_454e, 1024, |ctx| {
            let secret = format!("ghp_{:016x}", noprop::sample_u64(ctx));
            let response =
                format!("protocol=https\nhost=gitlab.com\nusername=user\npassword={secret}\n");
            let error = parse_repo_scoped_github_credential(&response).unwrap_err();
            assert!(!error.to_string().contains(&secret));
            Ok(())
        })
    }

    #[test]
    fn generated_github_dispatches_stay_on_configured_repository() -> noprop::TestResult {
        test_support::run(0x4748_4449_5350_4154, 1024, |ctx| {
            let owner = format!("owner-{:016x}", noprop::sample_u64(ctx));
            let repo = format!("repo-{:016x}", noprop::sample_u64(ctx));
            let workflow = format!("release-{:016x}.yml", noprop::sample_u64(ctx));
            let git_ref = format!("release/{:016x}", noprop::sample_u64(ctx));
            let repository = github_repository_from_remote_url(&format!(
                "https://github.com/{owner}/{repo}.git"
            ))
            .unwrap();
            validate_github_workflow(&workflow).unwrap();
            validate_github_ref(&git_ref).unwrap();
            let path = github_workflow_dispatch_path(&repository, &workflow);
            assert_eq!(
                path,
                format!("repos/{owner}/{repo}/actions/workflows/{workflow}/dispatches")
            );
            let body = json!({"ref": git_ref, "return_run_details": true});
            assert_eq!(body["ref"], git_ref);
            assert_eq!(body["return_run_details"], true);
            assert!(!path.contains("token"));
            Ok(())
        })
    }

    fn activity_git_pull_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        tempfile::TempDir,
        PathBuf,
    ) {
        let remote = tempfile::tempdir().unwrap();
        let seed = tempfile::tempdir().unwrap();
        let checkout_root = tempfile::tempdir().unwrap();
        run_git_fixture(remote.path(), &["init", "--bare", "--quiet"]);
        run_git_fixture(seed.path(), &["init", "--quiet"]);
        run_git_fixture(seed.path(), &["config", "user.name", "Temote Test"]);
        run_git_fixture(
            seed.path(),
            &["config", "user.email", "temote-test@example.invalid"],
        );
        std::fs::write(seed.path().join("tracked.txt"), "one\n").unwrap();
        run_git_fixture(seed.path(), &["add", "tracked.txt"]);
        run_git_fixture(seed.path(), &["commit", "--quiet", "-m", "initial"]);
        run_git_fixture(seed.path(), &["branch", "-M", "main"]);
        run_git_fixture(
            seed.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        run_git_fixture(seed.path(), &["push", "--quiet", "-u", "origin", "main"]);
        run_git_fixture(remote.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        let checkout = checkout_root.path().join("checkout");
        run_git_fixture(
            checkout_root.path(),
            &[
                "clone",
                "--quiet",
                remote.path().to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );
        std::fs::write(seed.path().join("tracked.txt"), "two\n").unwrap();
        run_git_fixture(seed.path(), &["add", "tracked.txt"]);
        run_git_fixture(seed.path(), &["commit", "--quiet", "-m", "update"]);
        run_git_fixture(seed.path(), &["push", "--quiet"]);
        (remote, seed, checkout_root, checkout)
    }

    #[tokio::test]
    async fn git_push_tag_is_create_only_by_default_and_updates_with_exact_lease() {
        let (remote, _seed, _checkout_root, checkout) = activity_git_pull_fixture();
        run_git_fixture(&checkout, &["fetch", "--quiet", "origin"]);
        let source_one = git_fixture_stdout(&checkout, &["rev-parse", "HEAD"]);
        let source_two = git_fixture_stdout(&checkout, &["rev-parse", "origin/main"]);
        assert_ne!(source_one, source_two);

        let cwd = config::canonical_directory(&checkout).unwrap();
        let session = config::Session {
            id: format!("tag-push-{}", Uuid::new_v4()),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
            started_at: 1,
            process_id: std::process::id(),
            permission_mode: config::PermissionMode::Agent,
        };
        let base_args = json!({
            "session_id": session.id,
            "cwd": cwd,
            "remote": "origin",
            "tag": "latest",
        });

        let mut create = base_args.clone();
        create["source_sha"] = json!(source_one);
        git_push_tag(&create, &session, None).await.unwrap();
        assert_eq!(
            git_fixture_stdout(remote.path(), &["rev-parse", "refs/tags/latest"]),
            source_one
        );
        assert!(git_fixture_stdout(&checkout, &["tag", "--list", "latest"]).is_empty());

        let mut create_again = base_args.clone();
        create_again["source_sha"] = json!(source_two);
        assert!(git_push_tag(&create_again, &session, None).await.is_err());
        assert_eq!(
            git_fixture_stdout(remote.path(), &["rev-parse", "refs/tags/latest"]),
            source_one
        );

        let mut stale_update = base_args.clone();
        stale_update["source_sha"] = json!(source_two);
        stale_update["expected_remote_sha"] = json!(source_two);
        assert!(git_push_tag(&stale_update, &session, None).await.is_err());
        assert_eq!(
            git_fixture_stdout(remote.path(), &["rev-parse", "refs/tags/latest"]),
            source_one
        );

        let mut exact_update = base_args;
        exact_update["source_sha"] = json!(source_two);
        exact_update["expected_remote_sha"] = json!(source_one);
        git_push_tag(&exact_update, &session, None).await.unwrap();
        assert_eq!(
            git_fixture_stdout(remote.path(), &["rev-parse", "refs/tags/latest"]),
            source_two
        );
    }

    #[tokio::test]
    async fn git_push_branch_behavior_remains_non_force_and_host_side() {
        let remote = tempfile::tempdir().unwrap();
        let repository = tempfile::tempdir().unwrap();
        run_git_fixture(remote.path(), &["init", "--bare", "--quiet"]);
        run_git_fixture(repository.path(), &["init", "--quiet"]);
        run_git_fixture(repository.path(), &["config", "user.name", "Temote Test"]);
        run_git_fixture(
            repository.path(),
            &["config", "user.email", "temote-test@example.invalid"],
        );
        std::fs::write(repository.path().join("tracked.txt"), "one\n").unwrap();
        run_git_fixture(repository.path(), &["add", "tracked.txt"]);
        run_git_fixture(repository.path(), &["commit", "--quiet", "-m", "initial"]);
        run_git_fixture(repository.path(), &["branch", "-M", "main"]);
        run_git_fixture(
            repository.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );

        let cwd = config::canonical_directory(repository.path()).unwrap();
        let session = config::Session {
            id: format!("branch-push-{}", Uuid::new_v4()),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
            started_at: 1,
            process_id: std::process::id(),
            permission_mode: config::PermissionMode::Agent,
        };
        git_push(
            &json!({
                "session_id": session.id,
                "cwd": cwd,
                "remote": "origin",
                "set_upstream": true,
            }),
            &session,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            git_fixture_stdout(remote.path(), &["rev-parse", "refs/heads/main"]),
            git_fixture_stdout(repository.path(), &["rev-parse", "HEAD"])
        );

        std::fs::write(repository.path().join("tracked.txt"), "two\n").unwrap();
        run_git_fixture(repository.path(), &["add", "tracked.txt"]);
        run_git_fixture(repository.path(), &["commit", "--quiet", "-m", "update"]);
        git_push(
            &json!({
                "session_id": session.id,
                "cwd": session.cwd,
                "remote": "origin",
            }),
            &session,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            git_fixture_stdout(remote.path(), &["rev-parse", "refs/heads/main"]),
            git_fixture_stdout(repository.path(), &["rev-parse", "HEAD"])
        );
    }

    fn recorded_scope(
        operation: ActivityOperation,
        summary: ActivitySummary,
    ) -> (ActivityScope, RecordingActivityEmitter) {
        let emitter = RecordingActivityEmitter::default();
        let scope = ActivityScope::with_summary(operation, summary, emitter.clone());
        (scope, emitter)
    }

    fn recorded_git_pull_scope() -> (ActivityScope, RecordingActivityEmitter) {
        recorded_scope(
            ActivityOperation::GitPull,
            ActivitySummary::git(ActivityRemote::Origin),
        )
    }

    fn assert_git_completed(
        emitter: &RecordingActivityEmitter,
        operation: ActivityOperation,
        summary: &ActivitySummary,
    ) {
        let updates = emitter.updates();
        assert_eq!(
            updates
                .iter()
                .map(ActivityUpdate::state)
                .collect::<Vec<_>>(),
            vec![
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Completed,
            ]
        );
        assert!(updates.iter().all(|update| update.operation() == operation));
        assert!(updates.iter().all(|update| update.summary() == summary));
    }

    fn spawn_git_pull(
        session: config::Session,
        activity: ActivityScope,
    ) -> tokio::task::JoinHandle<Result<Value>> {
        tokio::spawn(async move {
            let result = git_pull(
                &json!({"session_id": session.id, "cwd": session.cwd}),
                &session,
                Some(&activity),
            )
            .await;
            finish_tool_activity(Some(&activity), &result);
            result
        })
    }

    #[test]
    fn activity_approval_maps_all_git_operations_and_safe_remote_classes() {
        for (name, expected) in [
            ("git_add", ActivityOperation::GitAdd),
            ("git_commit", ActivityOperation::GitCommit),
            ("git_fetch", ActivityOperation::GitFetch),
            ("git_pull", ActivityOperation::GitPull),
            ("git_push", ActivityOperation::GitPush),
            ("git_push_tag", ActivityOperation::GitPush),
            ("git_branch_create", ActivityOperation::GitBranchCreate),
            ("git_switch", ActivityOperation::GitSwitch),
            ("git_worktree_add", ActivityOperation::GitWorktreeAdd),
            ("git_worktree_create", ActivityOperation::GitWorktreeCreate),
            ("git_worktree_list", ActivityOperation::GitWorktreeList),
            ("git_worktree_remove", ActivityOperation::GitWorktreeRemove),
            ("git_worktree_prune", ActivityOperation::GitWorktreePrune),
        ] {
            assert_eq!(git_activity_operation(name), Some(expected));
        }
        assert_eq!(git_activity_operation("git_status"), None);
        assert_eq!(
            git_activity_summary(&json!({}), ActivityOperation::GitPull).safe_summary(),
            "remote=origin"
        );
        assert_eq!(
            git_activity_summary(
                &json!({"remote": "private-name"}),
                ActivityOperation::GitFetch,
            )
            .safe_summary(),
            "remote=other"
        );
        assert_eq!(
            git_activity_summary(
                &json!({"remote": "secret-remote-marker"}),
                ActivityOperation::GitPush,
            )
            .safe_summary(),
            "remote=other"
        );
        assert_eq!(
            git_activity_summary(&json!({}), ActivityOperation::GitCommit).safe_summary(),
            ""
        );
    }

    #[tokio::test]
    async fn activity_approval_git_pull_allow_and_deny_have_ordered_states() {
        let (_remote, _seed, _checkout_root, checkout) = activity_git_pull_fixture();
        let session_id = format!("activity-approval-{}", Uuid::new_v4());
        let (sender, mut receiver) = approvals::approval_channel();
        let handle = approvals::spawn_runtime(&checkout, Some(&session_id), false, sender)
            .await
            .unwrap();
        let session = config::load_session(&session_id).await.unwrap();

        let (allowed_scope, allowed_emitter) = recorded_git_pull_scope();
        let allowed_task = spawn_git_pull(session.clone(), allowed_scope);
        let allowed_prompt = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("git_pull did not request approval")
            .expect("approval channel closed");
        assert_eq!(allowed_prompt.request.operation, "git_pull");
        assert_eq!(
            allowed_emitter.states(),
            vec![ActivityState::Started, ActivityState::WaitingApproval]
        );
        allowed_prompt.respond(true);
        let allowed = allowed_task.await.unwrap().unwrap();
        assert!(
            allowed["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"exit_code\":0")
        );
        assert_eq!(
            allowed_emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::WaitingApproval,
                ActivityState::Running,
                ActivityState::Completed,
            ]
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "two\n"
        );

        let (denied_scope, denied_emitter) = recorded_git_pull_scope();
        let denied_task = spawn_git_pull(session, denied_scope);
        let denied_prompt = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("second git_pull did not request approval")
            .expect("approval channel closed");
        assert_eq!(
            denied_emitter.states(),
            vec![ActivityState::Started, ActivityState::WaitingApproval]
        );
        denied_prompt.respond(false);
        let denied = denied_task
            .await
            .unwrap()
            .expect_err("denied git_pull unexpectedly succeeded");
        assert!(denied.to_string().contains("user denied git_pull"));
        let denied_updates = denied_emitter.updates();
        assert_eq!(
            denied_updates
                .iter()
                .map(ActivityUpdate::state)
                .collect::<Vec<_>>(),
            vec![
                ActivityState::Started,
                ActivityState::WaitingApproval,
                ActivityState::Failed,
            ]
        );
        assert_eq!(
            denied_updates.last().unwrap().summary(),
            &ActivitySummary::failure(ActivityErrorKind::ApprovalDenied)
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn activity_approval_agent_skips_waiting_and_absent_console_fails_closed() {
        let (_remote, _seed, _checkout_root, checkout) = activity_git_pull_fixture();
        let cwd = config::canonical_directory(&checkout).unwrap();
        let agent_session = config::Session {
            id: format!("activity-agent-{}", Uuid::new_v4()),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
            started_at: 1,
            process_id: 1,
            permission_mode: config::PermissionMode::Agent,
        };
        let (agent_scope, agent_emitter) = recorded_git_pull_scope();
        let agent_result = git_pull(
            &json!({"session_id": agent_session.id, "cwd": cwd}),
            &agent_session,
            Some(&agent_scope),
        )
        .await;
        finish_tool_activity(Some(&agent_scope), &agent_result);
        assert!(agent_result.is_ok());
        assert_eq!(
            agent_emitter.states(),
            vec![
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Completed,
            ]
        );

        let ask_session = config::Session {
            id: format!("activity-no-console-{}", Uuid::new_v4()),
            permission_mode: config::PermissionMode::Ask,
            ..agent_session
        };
        let (absent_scope, absent_emitter) = recorded_git_pull_scope();
        let absent_result = git_pull(
            &json!({"session_id": ask_session.id, "cwd": ask_session.cwd}),
            &ask_session,
            Some(&absent_scope),
        )
        .await;
        finish_tool_activity(Some(&absent_scope), &absent_result);
        assert!(absent_result.is_err());
        let absent_updates = absent_emitter.updates();
        assert_eq!(
            absent_updates
                .iter()
                .map(ActivityUpdate::state)
                .collect::<Vec<_>>(),
            vec![
                ActivityState::Started,
                ActivityState::WaitingApproval,
                ActivityState::Failed,
            ]
        );
        assert_eq!(
            absent_updates.last().unwrap().summary(),
            &ActivitySummary::failure(ActivityErrorKind::OperationFailed)
        );
    }

    #[tokio::test]
    async fn activity_approval_local_git_tools_keep_results_and_complete_once() {
        let (_remote, _seed, _checkout_root, checkout) = activity_git_pull_fixture();
        run_git_fixture(&checkout, &["pull", "--quiet", "--ff-only"]);
        run_git_fixture(&checkout, &["config", "user.name", "Temote Test"]);
        run_git_fixture(
            &checkout,
            &["config", "user.email", "temote-test@example.invalid"],
        );
        let cwd = config::canonical_directory(&checkout).unwrap();
        let session = config::Session {
            id: format!("activity-git-tools-{}", Uuid::new_v4()),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
            started_at: 1,
            process_id: 1,
            permission_mode: config::PermissionMode::Agent,
        };
        std::fs::write(checkout.join("local.txt"), "local\n").unwrap();

        let (add_scope, add_emitter) =
            recorded_scope(ActivityOperation::GitAdd, ActivitySummary::empty());
        let add_result = git_add(
            &json!({"session_id": session.id, "cwd": cwd, "paths": ["local.txt"]}),
            &session,
            Some(&add_scope),
        )
        .await;
        finish_tool_activity(Some(&add_scope), &add_result);
        assert!(add_result.is_ok());
        assert_git_completed(
            &add_emitter,
            ActivityOperation::GitAdd,
            &ActivitySummary::empty(),
        );

        let (commit_scope, commit_emitter) =
            recorded_scope(ActivityOperation::GitCommit, ActivitySummary::empty());
        let commit_result = git_commit(
            &json!({"session_id": session.id, "cwd": cwd, "message": "local update"}),
            &session,
            Some(&commit_scope),
        )
        .await;
        finish_tool_activity(Some(&commit_scope), &commit_result);
        assert!(commit_result.is_ok());
        assert_git_completed(
            &commit_emitter,
            ActivityOperation::GitCommit,
            &ActivitySummary::empty(),
        );

        let origin_summary = ActivitySummary::git(ActivityRemote::Origin);
        let (push_scope, push_emitter) =
            recorded_scope(ActivityOperation::GitPush, origin_summary.clone());
        let push_result = git_push(
            &json!({"session_id": session.id, "cwd": cwd}),
            &session,
            Some(&push_scope),
        )
        .await;
        finish_tool_activity(Some(&push_scope), &push_result);
        assert!(push_result.is_ok());
        assert_git_completed(&push_emitter, ActivityOperation::GitPush, &origin_summary);

        let (fetch_scope, fetch_emitter) =
            recorded_scope(ActivityOperation::GitFetch, origin_summary.clone());
        let fetch_result = git_fetch(
            &json!({"session_id": session.id, "cwd": cwd}),
            &session,
            Some(&fetch_scope),
        )
        .await;
        finish_tool_activity(Some(&fetch_scope), &fetch_result);
        assert!(fetch_result.is_ok());
        assert_git_completed(&fetch_emitter, ActivityOperation::GitFetch, &origin_summary);
    }

    #[tokio::test]
    async fn agent_git_fetch_skips_the_local_console() {
        let repo = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        run_git_fixture(remote.path(), &["init", "--bare", "--quiet"]);
        run_git_fixture(repo.path(), &["init", "--quiet"]);
        run_git_fixture(
            repo.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        let cwd = config::canonical_directory(repo.path()).unwrap();
        let session = config::Session {
            id: "agent-git-fetch".to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
        };
        let result = git_fetch(
            &json!({"session_id": "agent-git-fetch", "cwd": cwd}),
            &session,
            None,
        )
        .await
        .expect("agent git_fetch must not require a local approval console");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"exit_code\":0"), "{text}");
    }

    #[tokio::test]
    async fn ask_git_fetch_still_fails_closed_without_a_console() {
        let repo = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        run_git_fixture(remote.path(), &["init", "--bare", "--quiet"]);
        run_git_fixture(repo.path(), &["init", "--quiet"]);
        run_git_fixture(
            repo.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        let cwd = config::canonical_directory(repo.path()).unwrap();
        let session = config::Session {
            id: format!("ask-git-fetch-{}", Uuid::new_v4()),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Ask,
        };
        let error = git_fetch(
            &json!({"session_id": session.id, "cwd": cwd}),
            &session,
            None,
        )
        .await
        .expect_err("ask git_fetch must fail closed without a running console");
        assert!(
            error.to_string().contains("not running"),
            "unexpected error: {error}"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn agent_dev_tool_run_executes_without_a_local_console() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().unwrap();
        let fake_dir = tempfile::tempdir().unwrap();
        let fake = fake_dir.path().join("fake-cargo");
        std::fs::write(&fake, "#!/bin/sh\nprintf 'devtool-ok %s\\n' \"$*\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = format!("dev-tool-agent-{}", Uuid::new_v4());
        let (sender, _receiver) = approvals::approval_channel();
        let handle = approvals::spawn_runtime_with_logical_path_and_environment(
            workspace.path(),
            Some(&id),
            config::PermissionMode::Agent,
            sender,
            None,
            approvals::CapturedStartEnvironment::default(),
        )
        .await
        .unwrap();
        let session = config::load_session(&id).await.unwrap();
        let args = json!({
            "session_id": id,
            "tool": "cargo",
            "operation": "check",
            "args": ["--workspace"]
        });
        let result = dev_tool_run_with_executable(&args, &session, Some(&fake), None)
            .await
            .expect("agent dev_tool_run must not require a local approval console");
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(
            encoded.contains("devtool-ok check --workspace"),
            "{encoded}"
        );
        handle.shutdown().await.unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn agent_mode_command_remains_sandboxed() {
        let workspace = tempfile::tempdir().unwrap();
        let marker = PathBuf::from(format!("/var/tmp/temote-agent-marker-{}", Uuid::new_v4()));
        let command = vec!["/usr/bin/touch".to_owned(), marker.display().to_string()];
        let output = run_session_command(
            &command,
            workspace.path(),
            &[workspace.path().to_path_buf()],
            config::PermissionMode::Agent,
        )
        .await
        .unwrap();
        assert_ne!(output.status, 0, "sandbox must deny writes outside roots");
        assert!(!marker.exists());
        let _ = std::fs::remove_file(&marker);
    }
}
