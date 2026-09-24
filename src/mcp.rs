use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::line_protocol::{BoundedLine, MAX_JSON_LINE_BYTES, next_bounded_line};
#[cfg(feature = "network")]
use crate::opencode_server;
use crate::{
    activity_runtime, approvals, child_env, codex_app_server, config, evidence, friction,
    local_agent, managed_worktree, sandbox, session_control, session_control::SessionBackend,
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
pub(crate) const MAX_GIT_COMMIT_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_GIT_BRANCH_NAME_BYTES: usize = 255;
const MAX_GIT_BASE_REF_BYTES: usize = 512;
const MAX_MANAGED_WORKTREE_LIST_ENTRIES: usize = 128;
const MAX_GITHUB_REMOTE_URL_BYTES: usize = 2048;
const MAX_GIT_REMOTE_DESTINATIONS: usize = 32;
const MAX_GIT_REMOTE_DESTINATIONS_BYTES: usize = 32 * MAX_GITHUB_REMOTE_URL_BYTES;
const MAX_GIT_CONFIG_VALUES: usize = 32;
const MAX_GIT_CONFIG_OUTPUT_BYTES: usize = 16 * 1024;
const GIT_PULL_UPSTREAM_CONFIGURATION_ERROR: &str =
    "Git pull upstream configuration is unavailable";
const GIT_PUSH_REMOTE_CONFIGURATION_ERROR: &str = "Git push remote configuration is unavailable";
const GIT_REMOTE_DESTINATION_ERROR: &str = "Git remote destination is unavailable";
const GIT_REMOTE_DEFAULT_BRANCH_ERROR: &str = "Git remote default branch is unavailable";
const GIT_REMOTE_TRACKING_REF_ERROR: &str = "Git remote-tracking ref is unavailable or invalid; fetch and inspect the configured remote branch before retrying";
const GIT_REMOTE_PROTECTION_POLICY_ERROR: &str =
    "Git remote branch protection policy is unavailable";
const GITHUB_CREDENTIAL_MAPPING_ERROR: &str = "GitHub repository credential mapping is unavailable; configure the repository-local mapping (`git config --local credential.helper '' && git config --local --add credential.helper '!gh git credential --managed' && git config --local credential.useHttpPath true`; see docs/usage.md) or request the ambient_git_credentials session grant via session_permission_request";
const GITHUB_CREDENTIAL_UNAVAILABLE_ERROR: &str = "GitHub repository credential is unavailable";
const GITHUB_CREDENTIAL_PERMISSION_ERROR: &str =
    "GitHub repository credential lacks required permission";
const GITHUB_NETWORK_GIT_ERROR: &str = "GitHub repository network Git operation failed";
const MAX_MCP_RESPONSE_BYTES: usize = 52 * 1024 * 1024;
const MIN_RETURN_OUTPUT_BYTES: usize = 256;
const MAX_RETURN_OUTPUT_BYTES: usize = sandbox::MAX_COMMAND_OUTPUT_BYTES;
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
    activity_tool(
        "opencode_status",
        ActivityOperation::OpencodeStatus,
        "OpenCode serve probe",
    ),
    activity_tool_accepted(
        "opencode_task_start",
        ActivityOperation::OpencodeTaskStart,
        "OpenCode start acceptance",
    ),
    activity_tool(
        "opencode_task_get",
        ActivityOperation::OpencodeTaskGet,
        "OpenCode get",
    ),
    activity_tool_accepted(
        "opencode_task_control",
        ActivityOperation::OpencodeTaskControl,
        "OpenCode control acceptance",
    ),
    activity_tool(
        "devin_status",
        ActivityOperation::DevinStatus,
        "Devin status",
    ),
    activity_tool_accepted(
        "devin_task_start",
        ActivityOperation::DevinTaskStart,
        "Devin start acceptance",
    ),
    activity_tool(
        "devin_task_get",
        ActivityOperation::DevinTaskGet,
        "Devin get",
    ),
    activity_tool_accepted(
        "devin_task_control",
        ActivityOperation::DevinTaskControl,
        "Devin control acceptance",
    ),
    activity_tool(
        "devin_cloud_status",
        ActivityOperation::DevinCloudStatus,
        "Devin Cloud status",
    ),
    activity_tool_accepted(
        "devin_cloud_task_start",
        ActivityOperation::DevinCloudTaskStart,
        "Devin Cloud start acceptance",
    ),
    activity_tool(
        "devin_cloud_task_get",
        ActivityOperation::DevinCloudTaskGet,
        "Devin Cloud get",
    ),
    activity_tool_accepted(
        "devin_cloud_task_control",
        ActivityOperation::DevinCloudTaskControl,
        "Devin Cloud control acceptance",
    ),
    activity_job_tool(
        "local_agent_run",
        ActivityOperation::LocalAgentRun,
        "fake local agent worker",
    ),
    activity_tool("poll_job", ActivityOperation::PollJob, "job poll"),
    activity_tool("job_list", ActivityOperation::JobList, "job list"),
    activity_tool(
        "stop_job",
        ActivityOperation::StopJob,
        "job cancellation request",
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
}

impl JobActivityFailure {
    const fn error_kind(self) -> ActivityErrorKind {
        match self {
            Self::ChildFailed => ActivityErrorKind::ChildFailed,
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

fn tools(_public: bool, managed_sessions: bool) -> Value {
    let mut tools = json!([
        {"name":"session_list","title":"List Temote MCP sessions","description":"List active temote-mcp sessions and surface sessions whose liveness or workspace cannot be safely determined (status degraded). Returns session IDs, working directories, start times, status, and permission mode (ask/agent/yolo).","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
        {"name":"session_start","title":"Start a managed Temote MCP session","description":"Start a normal sandboxed session under a host-configured named root. Path must be <root-name> or <root-name>/<relative-path>; absolute paths and yolo creation are unavailable.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"path":{"type":"string"},"session_id":{"type":"string"}},"required":["path"],"additionalProperties":false}},
        {"name":"session_stop","title":"Stop a managed Temote MCP session","description":"Gracefully stop a session created through the authenticated HTTP endpoint and owned by the local Temote session supervisor. Local CLI/yolo sessions cannot be stopped remotely.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"session_restart","title":"Restart a managed Temote MCP session","description":"Restart an active normal sandboxed session created through the authenticated HTTP endpoint. Local CLI/yolo sessions cannot be restarted remotely.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"session_info","title":"Inspect a Temote MCP session","description":"Show durable lifecycle state, working directory, permission mode, exit reason, and last error for a temote-mcp session.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"evidence_read","title":"Read scoped Temote evidence","description":"Read a bounded UTF-8 chunk from an opaque expiring evidence record previously returned by Temote. Evidence is in-memory, session-owned, canonical-scope-bound, and cannot address arbitrary filesystem paths.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"evidence_id":{"type":"string","format":"uuid"},"offset_bytes":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":65536,"default":16384}},"required":["session_id","evidence_id"],"additionalProperties":false}},
        {"name":"codex_status","title":"Check Codex app-server compatibility","description":"Check the locally installed Codex app-server through stdio, validate the concrete protocol response shapes Temote consumes, and return bounded model/effort plus best-effort version diagnostics without a release-number allowlist.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"codex_task_start","title":"Start a scoped Codex task","description":"Accept an idempotent scoped Codex task mutation, persist acceptance before child side effects, then start a workspace-write Codex app-server thread/turn. operation_id is mandatory; no sandbox escape option is exposed.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"operation_id":{"type":"string","format":"uuid"},"task":{"type":"string","minLength":1,"maxLength":1048576},"model":{"type":"string","minLength":1,"maxLength":256},"effort":{"type":"string","minLength":1,"maxLength":256}},"required":["session_id","operation_id","task","model","effort"],"additionalProperties":false}},
        {"name":"codex_task_get","title":"Read a scoped Codex task","description":"Read and reconcile a retained Codex task owned by the full Temote session instance and canonical scope. Detailed thread data is exposed only through bounded scoped evidence.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"after_revision":{"type":"integer","minimum":0}},"required":["session_id","task_id"],"additionalProperties":false}},
        {"name":"codex_task_control","title":"Control a scoped Codex task","description":"Idempotently steer, resume, or interrupt a retained scoped Codex task. Acceptance is persisted before the app-server side effect; uncertain crash gaps return reconciliation_required rather than replaying blindly.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid"},"action":{"type":"string","enum":["steer","resume","interrupt"]},"input":{"type":"string","minLength":1,"maxLength":1048576}},"required":["session_id","task_id","operation_id","action"],"additionalProperties":false}},
        {"name":"opencode_status","title":"Check OpenCode serve compatibility","description":"Probe the locally installed OpenCode binary by starting a short-lived opencode serve on loopback, validate health and provider inventory, and return bounded diagnostics without a release-number allowlist.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"opencode_task_start","title":"Start a scoped OpenCode task","description":"Accept an idempotent scoped OpenCode task mutation, persist acceptance before child side effects, then start a per-task opencode serve on loopback and create its session. operation_id is mandatory; no sandbox escape option is exposed.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"operation_id":{"type":"string","format":"uuid"},"task":{"type":"string","minLength":1,"maxLength":1048576},"model":{"type":"string","minLength":1,"maxLength":256},"agent":{"type":"string","minLength":1,"maxLength":256},"variant":{"type":"string","minLength":1,"maxLength":256}},"required":["session_id","operation_id","task"],"additionalProperties":false}},
        {"name":"opencode_task_get","title":"Read a scoped OpenCode task","description":"Read and reconcile a retained OpenCode task owned by the full Temote session instance and canonical scope. Detailed session data is exposed only through bounded scoped evidence.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"after_revision":{"type":"integer","minimum":0}},"required":["session_id","task_id"],"additionalProperties":false}},
        {"name":"opencode_task_control","title":"Control a scoped OpenCode task","description":"Idempotently steer, resume, or interrupt a retained scoped OpenCode task. Acceptance is persisted before the serve side effect; uncertain crash gaps return reconciliation_required rather than replaying blindly.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid"},"action":{"type":"string","enum":["steer","resume","interrupt"]},"input":{"type":"string","minLength":1,"maxLength":1048576}},"required":["session_id","task_id","operation_id","action"],"additionalProperties":false}},
        {"name":"devin_status","title":"Check Devin ACP compatibility","description":"Probe the locally installed Devin CLI by starting a short-lived devin acp on stdio, validate the initialize response shape Temote consumes, and return bounded agent capability diagnostics without a release-number allowlist.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"devin_task_start","title":"Start a scoped Devin task","description":"Accept an idempotent scoped Devin task mutation, persist acceptance before child side effects, then start a per-task devin acp child process over stdio and create its ACP session. operation_id is mandatory; no sandbox escape option is exposed. Set cloud=true to relay through `devin acp --cloud` to Devin Cloud instead of running the local agent; model and agent are ignored in cloud mode and must be omitted.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"operation_id":{"type":"string","format":"uuid"},"task":{"type":"string","minLength":1,"maxLength":1048576},"model":{"type":"string","minLength":1,"maxLength":256},"agent":{"type":"string","minLength":1,"maxLength":256},"cloud":{"type":"boolean"}},"required":["session_id","operation_id","task"],"additionalProperties":false}},
        {"name":"devin_task_get","title":"Read a scoped Devin task","description":"Read and reconcile a retained Devin task owned by the full Temote session instance and canonical scope. Detailed session data is exposed only through bounded scoped evidence.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"after_revision":{"type":"integer","minimum":0}},"required":["session_id","task_id"],"additionalProperties":false}},
        {"name":"devin_task_control","title":"Control a scoped Devin task","description":"Idempotently steer, resume, or interrupt a retained scoped Devin task. Acceptance is persisted before the acp side effect; uncertain crash gaps return reconciliation_required rather than replaying blindly. Resume requires the agent's advertised loadSession capability and fails closed without it.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid"},"action":{"type":"string","enum":["steer","resume","interrupt"]},"input":{"type":"string","minLength":1,"maxLength":1048576}},"required":["session_id","task_id","operation_id","action"],"additionalProperties":false}},
        {"name":"devin_cloud_status","title":"Check Devin Cloud API access","description":"Verify the configured Devin Cloud API v3 credential by calling /v3/self and return the authenticated principal, organization, and API base URL. The credential value itself is never returned.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"devin_cloud_task_start","title":"Start a Devin Cloud session task","description":"Accept an idempotent scoped Devin Cloud task mutation, persist acceptance before the remote side effect, then create a hosted Devin session (API v3) with a structured report schema. The session runs on Devin Cloud, not on this host; operation_id is mandatory.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"operation_id":{"type":"string","format":"uuid"},"task":{"type":"string","minLength":1,"maxLength":1048576},"title":{"type":"string","minLength":1,"maxLength":256},"devin_mode":{"type":"string","enum":["normal","fast","lite","ultra","fusion","swe-2-medium","swe-2-high","swe-2-max"]},"repos":{"type":"array","items":{"type":"string","minLength":1,"maxLength":256},"maxItems":16},"max_acu_limit":{"type":"integer","minimum":1,"maximum":100000}},"required":["session_id","operation_id","task"],"additionalProperties":false}},
        {"name":"devin_cloud_task_get","title":"Read a Devin Cloud session task","description":"Read and reconcile a retained Devin Cloud task owned by the full Temote session instance and canonical scope against the hosted session status. Final messages are exposed only through bounded scoped evidence.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"after_revision":{"type":"integer","minimum":0}},"required":["session_id","task_id"],"additionalProperties":false}},
        {"name":"devin_cloud_task_control","title":"Control a Devin Cloud session task","description":"Idempotently steer (send a follow-up message), resume (message a suspended session), or interrupt (terminate) a retained Devin Cloud task. Acceptance is persisted before the API side effect; uncertain transport failures return reconciliation_required rather than replaying blindly.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid"},"action":{"type":"string","enum":["steer","resume","interrupt"]},"input":{"type":"string","minLength":1,"maxLength":1048576}},"required":["session_id","task_id","operation_id","action"],"additionalProperties":false}},
        {"name":"local_agent_run","title":"Run a local coding agent","description":"Run a verified Codex or OpenCode non-interactive agent in the selected session with canonical workspace scope, bounded task/output, isolated agent state, and local approval. The caller supplies a task and access mode, not an executable, raw argv, environment, or network policy. With worktree.branch, Temote binds the run to the selected repository's managed worktree (<configured src root>/worktrees/<repo>/<task>) and derives the path itself: it reuses only a verified managed worktree of that repository and branch, otherwise creates one through the approved path, and rejects cwd combined with worktree.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":local_agent_input_schema()},
        {"name":"poll_job","title":"Poll a sandbox job","description":"Poll a background command returned by execute or start_command. Optional output_limit_bytes or status_only can request a stricter completed-result view; omitted options reuse the job's stored default view.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"job_id":{"type":"string"},"output_limit_bytes":{"type":"integer","minimum":256,"maximum":1048576},"status_only":{"type":"boolean"}},"required":["session_id","job_id"],"additionalProperties":false}},
        {"name":"job_list","title":"List current-session sandbox jobs","description":"Return a bounded redacted snapshot of in-memory sandbox jobs owned by this session. Command text and job output are never included.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":128,"default":50}},"required":["session_id"],"additionalProperties":false}},
        {"name":"stop_job","title":"Stop a sandbox job","description":"Stop a background command returned by execute or start_command.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"job_id":{"type":"string"}},"required":["session_id","job_id"],"additionalProperties":false}},
    ]);
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
            #[cfg(feature = "network")]
            "opencode_status" => {
                let (detail, metadata) = opencode_status_approval();
                authorize_opencode_operation(
                    &session,
                    "opencode_status",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &opencode_server::status(&session).await?,
                )?)
            }
            #[cfg(feature = "network")]
            "opencode_task_start" => {
                let (detail, metadata) = opencode_task_start_approval(&args);
                authorize_opencode_operation(
                    &session,
                    "opencode_task_start",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &opencode_server::task_start(&args, &session).await?,
                )?)
            }
            #[cfg(feature = "network")]
            "opencode_task_get" => text_result(serde_json::to_string_pretty(
                &opencode_server::task_get(&args, &session).await?,
            )?),
            #[cfg(feature = "network")]
            "opencode_task_control" => {
                let (detail, metadata) = opencode_task_control_approval(&args);
                authorize_opencode_operation(
                    &session,
                    "opencode_task_control",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &opencode_server::task_control(&args, &session).await?,
                )?)
            }
            "devin_status" => {
                let (detail, metadata) = devin_status_approval();
                authorize_devin_operation(
                    &session,
                    "devin_status",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &crate::devin_acp::status(&session).await?,
                )?)
            }
            "devin_task_start" => {
                let (detail, metadata) = devin_task_start_approval(&args);
                authorize_devin_operation(
                    &session,
                    "devin_task_start",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &crate::devin_acp::task_start(&args, &session).await?,
                )?)
            }
            "devin_task_get" => text_result(serde_json::to_string_pretty(
                &crate::devin_acp::task_get(&args, &session).await?,
            )?),
            "devin_task_control" => {
                let (detail, metadata) = devin_task_control_approval(&args);
                authorize_devin_operation(
                    &session,
                    "devin_task_control",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &crate::devin_acp::task_control(&args, &session).await?,
                )?)
            }
            #[cfg(feature = "network")]
            "devin_cloud_status" => {
                let (detail, metadata) = devin_cloud_status_approval();
                authorize_devin_cloud_operation(
                    &session,
                    "devin_cloud_status",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &crate::devin_cloud::status(&session).await?,
                )?)
            }
            #[cfg(feature = "network")]
            "devin_cloud_task_start" => {
                let (detail, metadata) = devin_cloud_task_start_approval(&args);
                authorize_devin_cloud_operation(
                    &session,
                    "devin_cloud_task_start",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &crate::devin_cloud::task_start(&args, &session).await?,
                )?)
            }
            #[cfg(feature = "network")]
            "devin_cloud_task_get" => text_result(serde_json::to_string_pretty(
                &crate::devin_cloud::task_get(&args, &session).await?,
            )?),
            #[cfg(feature = "network")]
            "devin_cloud_task_control" => {
                let (detail, metadata) = devin_cloud_task_control_approval(&args);
                authorize_devin_cloud_operation(
                    &session,
                    "devin_cloud_task_control",
                    detail,
                    metadata,
                    activity.as_ref(),
                )
                .await?;
                text_result(serde_json::to_string_pretty(
                    &crate::devin_cloud::task_control(&args, &session).await?,
                )?)
            }
            "local_agent_run" => {
                local_agent_run(&args, &session, local_agent_executable, activity.clone()).await
            }
            "poll_job" => poll_job(&args, &session).await,
            "job_list" => job_list(&args, &session),
            "stop_job" => stop_job_with_activity(&args, &session, activity.as_ref()).await,
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

#[cfg(feature = "network")]
async fn authorize_opencode_operation(
    session: &config::Session,
    action: &str,
    detail: String,
    metadata: BTreeMap<String, String>,
    activity: Option<&ActivityScope>,
) -> Result<()> {
    let approved = approvals::ensure_local_approval_with_activity(
        session,
        approvals::ApprovalClass::OpenCodeServer,
        action,
        detail,
        session.cwd.clone(),
        metadata,
        activity,
    )
    .await?;
    finish_activity_approval(approved, activity, "user denied OpenCode operation")?;
    Ok(())
}

#[cfg(feature = "network")]
fn opencode_status_approval() -> (String, BTreeMap<String, String>) {
    (
        "OpenCode delegation request\naccess: read-only\nscope: current session\nresult: serve compatibility and provider metadata".to_owned(),
        opencode_approval_metadata("opencode_status", "status", false, "session_scope"),
    )
}

#[cfg(feature = "network")]
fn opencode_task_start_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let operation_id = safe_codex_argument(args, "operation_id");
    let model = safe_codex_argument(args, "model");
    let agent = safe_codex_argument(args, "agent");
    let variant = safe_codex_argument(args, "variant");
    let mut metadata =
        opencode_approval_metadata("opencode_task_start", "task_start", true, "session_scope");
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("model".to_owned(), model.clone());
    metadata.insert("agent".to_owned(), agent.clone());
    metadata.insert("variant".to_owned(), variant.clone());
    metadata.insert("task_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "OpenCode delegation request\noperation: start task\nmutation: workspace-write\nscope: current session working directory\nmodel: {model}\nagent: {agent}\nvariant: {variant}\noperation_id: {operation_id}\ntask input: omitted"
        ),
        metadata,
    )
}

#[cfg(feature = "network")]
fn opencode_task_control_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let task_id = safe_codex_argument(args, "task_id");
    let operation_id = safe_codex_argument(args, "operation_id");
    let action = safe_codex_argument(args, "action");
    let mut metadata = opencode_approval_metadata(
        "opencode_task_control",
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
            "OpenCode delegation request\noperation: control task\naction: {action}\nmutation: task control\ntarget: task {task_id}\nscope: current session working directory\noperation_id: {operation_id}\ncontrol input: omitted"
        ),
        metadata,
    )
}

#[cfg(feature = "network")]
fn opencode_approval_metadata(
    tool: &str,
    operation_type: &str,
    mutation: bool,
    target: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("provenance".to_owned(), "opencode_delegation".to_owned()),
        ("source".to_owned(), "opencode_delegation".to_owned()),
        ("tool".to_owned(), tool.to_owned()),
        ("operation_type".to_owned(), operation_type.to_owned()),
        ("target".to_owned(), target.to_owned()),
        ("mutation".to_owned(), mutation.to_string()),
        ("read_only".to_owned(), (!mutation).to_string()),
        ("scope".to_owned(), "session_cwd".to_owned()),
    ])
}

async fn authorize_devin_operation(
    session: &config::Session,
    action: &str,
    detail: String,
    metadata: BTreeMap<String, String>,
    activity: Option<&ActivityScope>,
) -> Result<()> {
    let approved = approvals::ensure_local_approval_with_activity(
        session,
        approvals::ApprovalClass::DevinAcp,
        action,
        detail,
        session.cwd.clone(),
        metadata,
        activity,
    )
    .await?;
    finish_activity_approval(approved, activity, "user denied Devin operation")?;
    Ok(())
}

fn devin_status_approval() -> (String, BTreeMap<String, String>) {
    (
        "Devin delegation request\naccess: read-only\nscope: current session\nresult: acp capability and agent metadata".to_owned(),
        devin_approval_metadata("devin_status", "status", false, "session_scope"),
    )
}

fn devin_task_start_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let operation_id = safe_codex_argument(args, "operation_id");
    let model = safe_codex_argument(args, "model");
    let agent = safe_codex_argument(args, "agent");
    let cloud = args.get("cloud").and_then(Value::as_bool).unwrap_or(false);
    let mut metadata =
        devin_approval_metadata("devin_task_start", "task_start", true, "session_scope");
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("model".to_owned(), model.clone());
    metadata.insert("agent".to_owned(), agent.clone());
    metadata.insert("cloud".to_owned(), cloud.to_string());
    metadata.insert("task_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "Devin delegation request\noperation: start task\nmutation: workspace-write\nscope: current session working directory\nmodel: {model}\nagent: {agent}\ncloud: {cloud}\noperation_id: {operation_id}\ntask input: omitted"
        ),
        metadata,
    )
}

fn devin_task_control_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let task_id = safe_codex_argument(args, "task_id");
    let operation_id = safe_codex_argument(args, "operation_id");
    let action = safe_codex_argument(args, "action");
    let mut metadata = devin_approval_metadata(
        "devin_task_control",
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
            "Devin delegation request\noperation: control task\naction: {action}\nmutation: task control\ntarget: task {task_id}\nscope: current session working directory\noperation_id: {operation_id}\ncontrol input: omitted"
        ),
        metadata,
    )
}

fn devin_approval_metadata(
    tool: &str,
    operation_type: &str,
    mutation: bool,
    target: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("provenance".to_owned(), "devin_delegation".to_owned()),
        ("source".to_owned(), "devin_delegation".to_owned()),
        ("tool".to_owned(), tool.to_owned()),
        ("operation_type".to_owned(), operation_type.to_owned()),
        ("target".to_owned(), target.to_owned()),
        ("mutation".to_owned(), mutation.to_string()),
        ("read_only".to_owned(), (!mutation).to_string()),
        ("scope".to_owned(), "session_cwd".to_owned()),
    ])
}

#[cfg(feature = "network")]
async fn authorize_devin_cloud_operation(
    session: &config::Session,
    action: &str,
    detail: String,
    metadata: BTreeMap<String, String>,
    activity: Option<&ActivityScope>,
) -> Result<()> {
    let approved = approvals::ensure_local_approval_with_activity(
        session,
        approvals::ApprovalClass::DevinCloud,
        action,
        detail,
        session.cwd.clone(),
        metadata,
        activity,
    )
    .await?;
    finish_activity_approval(approved, activity, "user denied Devin Cloud operation")?;
    Ok(())
}

#[cfg(feature = "network")]
fn devin_cloud_status_approval() -> (String, BTreeMap<String, String>) {
    (
        "Devin Cloud delegation request\naccess: read-only\nscope: current session\nresult: authenticated principal and organization (credential value omitted)".to_owned(),
        devin_cloud_approval_metadata("devin_cloud_status", "status", false, "session_scope"),
    )
}

#[cfg(feature = "network")]
fn devin_cloud_task_start_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let operation_id = safe_codex_argument(args, "operation_id");
    let title = safe_codex_argument(args, "title");
    let devin_mode = safe_codex_argument(args, "devin_mode");
    let repos = args
        .get("repos")
        .and_then(Value::as_array)
        .map(|items| items.len().to_string())
        .unwrap_or_else(|| "0".to_owned());
    let mut metadata = devin_cloud_approval_metadata(
        "devin_cloud_task_start",
        "task_start",
        true,
        "devin_cloud_session",
    );
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("title".to_owned(), title.clone());
    metadata.insert("devin_mode".to_owned(), devin_mode.clone());
    metadata.insert("repos".to_owned(), repos.clone());
    metadata.insert("task_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "Devin Cloud delegation request\noperation: start hosted session\nmutation: remote Devin Cloud session (consumes ACUs)\nscope: Devin Cloud organization, not this host\ntitle: {title}\ndevin_mode: {devin_mode}\nrepos: {repos}\noperation_id: {operation_id}\ntask input: omitted"
        ),
        metadata,
    )
}

#[cfg(feature = "network")]
fn devin_cloud_task_control_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let task_id = safe_codex_argument(args, "task_id");
    let operation_id = safe_codex_argument(args, "operation_id");
    let action = safe_codex_argument(args, "action");
    let mut metadata = devin_cloud_approval_metadata(
        "devin_cloud_task_control",
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
            "Devin Cloud delegation request\noperation: control hosted session\naction: {action}\nmutation: remote Devin Cloud session\ntarget: task {task_id}\noperation_id: {operation_id}\ncontrol input: omitted"
        ),
        metadata,
    )
}

#[cfg(feature = "network")]
fn devin_cloud_approval_metadata(
    tool: &str,
    operation_type: &str,
    mutation: bool,
    target: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("provenance".to_owned(), "devin_cloud_delegation".to_owned()),
        ("source".to_owned(), "devin_cloud_delegation".to_owned()),
        ("tool".to_owned(), tool.to_owned()),
        ("operation_type".to_owned(), operation_type.to_owned()),
        ("target".to_owned(), target.to_owned()),
        ("mutation".to_owned(), mutation.to_string()),
        ("read_only".to_owned(), (!mutation).to_string()),
        ("scope".to_owned(), "devin_cloud".to_owned()),
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
        ActivityOperation::GitFetch
        | ActivityOperation::GitPull
        | ActivityOperation::GitPush
        | ActivityOperation::GitRemoteBranchDelete => {
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

async fn resolve_git_remote_destinations_pinned(
    pinned: &sandbox::PinnedGitRepository,
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
        map_git_remote_inspection_error(run_pinned_git_inspection(pinned, &command).await)?;
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
/// resolves, the structured push fails closed instead of delegating destination
/// or refspec selection back to repository configuration.
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
                // Preserve Git's local-remote semantics while still returning
                // the exact destination that the mutation command must use.
                return Ok(Some(remote));
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
    if remote.is_empty() {
        return Ok(None);
    }
    if remote == "." {
        return Ok(Some(remote.to_owned()));
    }
    validate_git_remote(remote)?;
    Ok(Some(remote.to_owned()))
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
    let explicit_remote = remote.is_some();
    let selected_remote = if set_upstream {
        remote.unwrap_or_else(|| "origin".to_owned())
    } else if remote.is_some() {
        remote.context(GIT_PUSH_REMOTE_CONFIGURATION_ERROR)?
    } else {
        git_current_push_remote(session, &cwd)
            .await?
            .context(GIT_PUSH_REMOTE_CONFIGURATION_ERROR)?
    };
    validate_git_remote(&selected_remote)?;
    let destinations = if !explicit_remote && !set_upstream && selected_remote == "." {
        GitRemoteDestinations { urls: Vec::new() }
    } else {
        resolve_git_remote_destinations(session, &cwd, &selected_remote, GitRemoteOperation::Push)
            .await?
    };
    ensure_github_https_destinations_credential_mapping(session, &cwd, &destinations).await?;
    // The inspected destination and the mutation destination are deliberately
    // the same value.  Supplying both the remote and HEAD prevents repository
    // push.default or remote.<name>.push configuration from widening scope.
    let command = build_git_push_command(&selected_remote, set_upstream);
    run_approved_git_network_output(
        session,
        cwd,
        command,
        "git_push",
        activity,
        destinations.requires_github_credential_mapping(),
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
pub(crate) fn build_git_push_command(remote: &str, set_upstream: bool) -> Vec<String> {
    let mut command = vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "-c".to_owned(),
        "push.recurseSubmodules=off".to_owned(),
        "-c".to_owned(),
        "push.followTags=false".to_owned(),
        "push".to_owned(),
    ];
    if set_upstream {
        command.push("--set-upstream".to_owned());
    }
    command.push(remote.to_owned());
    command.push("HEAD".to_owned());
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
    if session.grants.ambient_git_credentials {
        // Host-approved fallback: the command runs unrestricted anyway, so
        // ambient helpers/agent credentials may authenticate instead of the
        // repository-local managed mapping.
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

async fn ensure_github_https_destinations_credential_mapping_pinned(
    pinned: &sandbox::PinnedGitRepository,
    destinations: &GitRemoteDestinations,
    ambient_git_credentials: bool,
) -> Result<()> {
    if !destinations.requires_github_credential_mapping() {
        return Ok(());
    }
    if ambient_git_credentials {
        return Ok(());
    }
    let local_helpers = map_github_credential_inspection_error(
        run_pinned_git_inspection(
            pinned,
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
        run_pinned_git_inspection(
            pinned,
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

async fn git_branch_delete(
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let repository_root = sandbox::git_worktree_root(&cwd)?;
    config::ensure_permitted(session, &repository_root)
        .context("Git repository root must be inside a permitted session root")?;
    // Capture the worktree and both Git metadata roots before any approval
    // wait. The final bounded command uses these descriptors rather than
    // resolving the request pathname again, so a replaced cwd cannot redirect
    // the host-side mutation to another repository.
    let pinned = sandbox::pin_git_repository(&repository_root)?;
    let branch = args
        .get("branch")
        .and_then(Value::as_str)
        .context("missing or non-string branch")?;
    validate_git_branch_name(session, &cwd, branch).await?;
    ensure_local_branch_exists(session, &cwd, branch).await?;
    ensure_local_branch_not_checked_out(session, &cwd, branch).await?;
    approve_local_git_mutation(
        session,
        &cwd,
        "git_branch_delete",
        format!("branch={branch} mode=merged-only"),
        activity,
    )
    .await?;
    // Approval is not the trust boundary. Re-prove existence and worktree
    // ownership immediately before the exact host-side mutation. The normal
    // Git sandbox intentionally keeps packed-refs immutable, while native
    // merged-only branch deletion always acquires packed-refs.lock even for a
    // loose ref, so this dedicated structured path must own that metadata
    // mutation itself.
    ensure_local_branch_exists(session, &cwd, branch).await?;
    ensure_local_branch_not_checked_out(session, &cwd, branch).await?;
    run_git_branch_delete_and_report(session, pinned, branch, activity).await
}

/// Runs the structured merged-only branch deletion for the local-agent shim.
/// The shim supplies no alternate command shape or mutation path; this remains
/// the same approval, worktree-ownership, recheck, and pinned mutation path as
/// the structured MCP operation.
pub(crate) async fn git_branch_delete_for_shim(
    session: &config::Session,
    branch: &str,
) -> Result<Value> {
    git_branch_delete(&json!({"branch": branch}), session, None).await
}

/// Runs the ordinary local-agent remote-delete form. The request cwd has
/// already been validated by the broker; derive the repository root from that
/// cwd here so nested directories remain valid without allowing the caller to
/// pair an unrelated root path with a pinned repository.
pub(crate) async fn git_remote_branch_delete_for_shim(
    session: &config::Session,
    cwd: &Path,
    remote: &str,
    branch: &str,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let repository_root = sandbox::git_worktree_root(cwd)?;
    config::ensure_permitted(session, &repository_root)
        .context("Git repository root must be inside a permitted session root")?;
    let pinned = sandbox::pin_git_repository(&repository_root)?;
    let expected_remote_sha =
        resolve_git_remote_tracking_sha_pinned(&pinned, remote, branch).await?;
    git_remote_branch_delete_with_pinned_repository(
        session,
        &pinned,
        &repository_root,
        remote,
        branch,
        &expected_remote_sha,
        activity,
    )
    .await
}

/// Resolves the exact fetch-established remote-tracking ref used as the
/// ordinary shim push-delete lease. This intentionally never contacts the
/// remote: the tracking ref is the local review snapshot, and every failure is
/// reduced to one bounded instruction to fetch and inspect before retrying.
pub(crate) async fn resolve_git_remote_tracking_sha_pinned(
    pinned: &sandbox::PinnedGitRepository,
    remote: &str,
    branch: &str,
) -> Result<String> {
    validate_git_remote(remote).map_err(|_| anyhow::anyhow!(GIT_REMOTE_TRACKING_REF_ERROR))?;
    validate_git_branch_name_pinned(pinned, branch)
        .await
        .map_err(|_| anyhow::anyhow!(GIT_REMOTE_TRACKING_REF_ERROR))?;
    let tracking_ref = format!("refs/remotes/{remote}/{branch}");
    let output = run_pinned_git_inspection(
        pinned,
        &[
            "git".to_owned(),
            "show-ref".to_owned(),
            "--verify".to_owned(),
            "--hash".to_owned(),
            tracking_ref,
        ],
    )
    .await
    .map_err(|_| anyhow::anyhow!(GIT_REMOTE_TRACKING_REF_ERROR))?;
    let sha = parse_git_remote_tracking_sha(&output)
        .map_err(|_| anyhow::anyhow!(GIT_REMOTE_TRACKING_REF_ERROR))?;
    let object = run_pinned_git_inspection(
        pinned,
        &[
            "git".to_owned(),
            "cat-file".to_owned(),
            "-e".to_owned(),
            format!("{sha}^{{object}}"),
        ],
    )
    .await
    .map_err(|_| anyhow::anyhow!(GIT_REMOTE_TRACKING_REF_ERROR))?;
    anyhow::ensure!(
        object.status == 0 && !object.truncated,
        GIT_REMOTE_TRACKING_REF_ERROR
    );
    Ok(sha)
}

fn parse_git_remote_tracking_sha(output: &sandbox::Output) -> Result<String> {
    anyhow::ensure!(output.status == 0 && !output.truncated);
    let value = output
        .stdout
        .strip_suffix('\n')
        .context("missing SHA line")?;
    anyhow::ensure!(!value.is_empty() && !value.contains(['\n', '\r']));
    validate_git_object_id(value, "remote-tracking SHA")?;
    Ok(value.to_ascii_lowercase())
}

/// Deletes one exact remote branch using a repository descriptor pinned by the
/// caller before tracking-ref resolution. All destination, credential,
/// default/protection, approval, and lease-mutation operations below use this
/// same pinned repository.
pub(crate) async fn git_remote_branch_delete_with_pinned_repository(
    session: &config::Session,
    pinned: &sandbox::PinnedGitRepository,
    repository_root: &Path,
    remote: &str,
    branch: &str,
    expected_remote_sha: &str,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    config::ensure_permitted(session, repository_root)
        .context("Git repository root must be inside a permitted session root")?;
    validate_git_remote(remote)?;
    validate_git_branch_name_pinned(pinned, branch).await?;
    validate_git_object_id(expected_remote_sha, "expected_remote_sha")?;
    let expected_remote_sha = expected_remote_sha.to_ascii_lowercase();

    let destinations =
        resolve_git_remote_destinations_pinned(pinned, remote, GitRemoteOperation::Push).await?;
    anyhow::ensure!(
        destinations.urls.len() == 1,
        "remote branch deletion requires exactly one configured push destination"
    );
    ensure_github_https_destinations_credential_mapping_pinned(
        pinned,
        &destinations,
        session.grants.ambient_git_credentials,
    )
    .await?;
    let fetch_destinations =
        resolve_git_remote_destinations_pinned(pinned, remote, GitRemoteOperation::Fetch).await?;
    anyhow::ensure!(
        fetch_destinations.urls == destinations.urls,
        "remote branch deletion requires matching fetch and push destinations"
    );
    request_activity_approval(
        session,
        ActivityApprovalRequest {
            class: approvals::ApprovalClass::GitNetwork,
            operation: "git_remote_branch_delete",
            detail: format!(
                "remote={remote} branch={branch} expected_remote_sha={expected_remote_sha}"
            ),
            cwd: repository_root.to_path_buf(),
            metadata: BTreeMap::new(),
            denial: "user denied Git remote branch deletion",
        },
        activity,
    )
    .await?;

    // Approval is not the trust boundary. Re-read the configured destinations
    // and repository-local credential mapping from the pinned repository, then
    // use only those live values for the remaining authority checks and push.
    let destinations_after =
        resolve_git_remote_destinations_pinned(pinned, remote, GitRemoteOperation::Push).await?;
    anyhow::ensure!(
        destinations_after.urls.len() == 1,
        "remote branch deletion requires exactly one configured push destination"
    );
    anyhow::ensure!(
        destinations_after == destinations,
        "configured remote destination changed during approval"
    );
    let fetch_destinations_after =
        resolve_git_remote_destinations_pinned(pinned, remote, GitRemoteOperation::Fetch).await?;
    anyhow::ensure!(
        fetch_destinations_after.urls == destinations_after.urls,
        "remote branch deletion requires matching fetch and push destinations"
    );
    ensure_github_https_destinations_credential_mapping_pinned(
        pinned,
        &destinations_after,
        session.grants.ambient_git_credentials,
    )
    .await?;

    // The remote's live symbolic HEAD is authoritative; a cached
    // refs/remotes/origin/HEAD or a conventional branch name is not sufficient.
    let default_branch = resolve_remote_default_branch_after_approval(pinned, remote).await?;
    anyhow::ensure!(
        branch != default_branch,
        "remote default branch cannot be deleted"
    );

    if let Some(repository) = github_repository_from_destination(
        destinations_after
            .urls
            .first()
            .context("remote branch deletion destination is unavailable")?,
    )? {
        let protected =
            github_remote_branch_is_protected_after_approval(pinned, &repository, branch).await?;
        anyhow::ensure!(!protected, "remote protected branch cannot be deleted");
    } else {
        let protected_branches =
            non_github_remote_protected_branches_after_approval(pinned, remote).await?;
        anyhow::ensure!(
            !protected_branches
                .iter()
                .any(|protected| protected == branch),
            "remote protected branch cannot be deleted"
        );
    }

    let command = build_git_remote_branch_delete_command(remote, branch, &expected_remote_sha);
    let output = run_pinned_git_output_after_approval(
        session,
        pinned,
        command,
        activity,
        destinations_after.requires_github_credential_mapping(),
    )
    .await?;
    text_result(render_output(output)?)
}

/// Reads the live symbolic HEAD of the configured remote.  The remote-tracking
/// `origin/HEAD` ref is intentionally not consulted: it is a local cache and
/// can be stale or absent.
async fn resolve_remote_default_branch_after_approval(
    pinned: &sandbox::PinnedGitRepository,
    remote: &str,
) -> Result<String> {
    let output = run_pinned_git_inspection(
        pinned,
        &[
            "git".to_owned(),
            "ls-remote".to_owned(),
            "--symref".to_owned(),
            remote.to_owned(),
            "HEAD".to_owned(),
        ],
    )
    .await
    .map_err(|_| anyhow::anyhow!(GIT_REMOTE_DEFAULT_BRANCH_ERROR))?;
    let branch = parse_remote_default_branch(&output)
        .map_err(|_| anyhow::anyhow!(GIT_REMOTE_DEFAULT_BRANCH_ERROR))?;
    validate_git_branch_name_pinned(pinned, &branch)
        .await
        .map_err(|_| anyhow::anyhow!(GIT_REMOTE_DEFAULT_BRANCH_ERROR))?;
    Ok(branch)
}

fn parse_remote_default_branch(output: &sandbox::Output) -> Result<String> {
    anyhow::ensure!(output.status == 0 && !output.truncated);
    let mut lines = output.stdout.split('\n').collect::<Vec<_>>();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    anyhow::ensure!(lines.len() == 2);
    let (symref, symref_name) = lines[0].split_once('\t').context("missing remote symref")?;
    anyhow::ensure!(symref_name == "HEAD");
    let branch = symref
        .strip_prefix("ref: refs/heads/")
        .context("remote HEAD is not a branch symref")?;
    anyhow::ensure!(!branch.is_empty());
    let (sha, head_name) = lines[1].split_once('\t').context("missing remote HEAD")?;
    anyhow::ensure!(head_name == "HEAD");
    validate_git_object_id(sha, "remote HEAD")?;
    Ok(branch.to_owned())
}

/// A GitHub branch protection check is only authoritative when the deletion
/// destination itself identifies a GitHub repository.  A fetch URL is not used
/// for this decision when a different push URL is configured.
fn github_repository_from_destination(url: &str) -> Result<Option<GithubRepository>> {
    let github = is_github_https_destination(url)
        || url
            .strip_prefix("git@")
            .and_then(|value| value.split_once(':'))
            .is_some_and(|(host, _)| host.eq_ignore_ascii_case("github.com"))
        || url
            .strip_prefix("ssh://git@")
            .and_then(|value| value.split_once('/'))
            .is_some_and(|(host, _)| host.eq_ignore_ascii_case("github.com"));
    if !github {
        return Ok(None);
    }
    Ok(Some(github_repository_from_remote_url(url)?))
}

async fn github_remote_branch_is_protected_after_approval(
    pinned: &sandbox::PinnedGitRepository,
    repository: &GithubRepository,
    branch: &str,
) -> Result<bool> {
    let path = github_branch_metadata_path(repository, branch)?;
    let response = run_pinned_github_api_after_approval(
        pinned,
        repository,
        GithubApiCall {
            method: GithubApiMethod::Get,
            path: &path,
            body: None,
        },
    )
    .await
    .map_err(|_| anyhow::anyhow!(GIT_REMOTE_PROTECTION_POLICY_ERROR))?;
    parse_github_branch_protected_response(&response, branch)
        .map_err(|_| anyhow::anyhow!(GIT_REMOTE_PROTECTION_POLICY_ERROR))
}

async fn non_github_remote_protected_branches_after_approval(
    pinned: &sandbox::PinnedGitRepository,
    remote: &str,
) -> Result<Vec<String>> {
    let key = format!("temote.remote.{remote}.protectedBranch");
    let values = git_local_config_values_pinned(pinned, &key)
        .await
        .map_err(|_| anyhow::anyhow!(GIT_REMOTE_PROTECTION_POLICY_ERROR))?;
    anyhow::ensure!(!values.is_empty(), GIT_REMOTE_PROTECTION_POLICY_ERROR);
    for value in &values {
        validate_git_branch_name_pinned(pinned, value)
            .await
            .map_err(|_| anyhow::anyhow!(GIT_REMOTE_PROTECTION_POLICY_ERROR))?;
    }
    Ok(values)
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

pub(crate) fn validate_git_branch_name_syntax(branch: &str) -> Result<()> {
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
    Ok(())
}

pub(crate) async fn validate_git_branch_name(
    session: &config::Session,
    cwd: &Path,
    branch: &str,
) -> Result<()> {
    validate_git_branch_name_syntax(branch)?;
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

async fn validate_git_branch_name_pinned(
    pinned: &sandbox::PinnedGitRepository,
    branch: &str,
) -> Result<()> {
    validate_git_branch_name_syntax(branch)?;
    let output = run_pinned_git_inspection(
        pinned,
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

async fn ensure_local_branch_not_checked_out(
    session: &config::Session,
    cwd: &Path,
    branch: &str,
) -> Result<()> {
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
        output.status == 0 && !output.truncated,
        "Git worktree ownership is unavailable"
    );
    let worktrees = managed_worktree::parse_worktree_list(&output.stdout)
        .context("Git worktree ownership is unavailable")?;
    anyhow::ensure!(
        !worktrees
            .iter()
            .any(|worktree| worktree.branch.as_deref() == Some(branch)),
        "local Git branch is checked out in a worktree"
    );
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

fn build_git_branch_delete_command(branch: &str) -> Vec<String> {
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "branch".to_owned(),
        "--delete".to_owned(),
        "--".to_owned(),
        branch.to_owned(),
    ]
}

fn build_git_remote_branch_delete_command(
    remote: &str,
    branch: &str,
    expected_remote_sha: &str,
) -> Vec<String> {
    let branch_ref = format!("refs/heads/{branch}");
    vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "-c".to_owned(),
        "push.recurseSubmodules=off".to_owned(),
        "-c".to_owned(),
        "push.followTags=false".to_owned(),
        "push".to_owned(),
        format!("--force-with-lease={branch_ref}:{expected_remote_sha}"),
        remote.to_owned(),
        format!(":{branch_ref}"),
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

fn validate_git_object_id(value: &str, field: &str) -> Result<()> {
    anyhow::ensure!(
        matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{field} must be an exact 40- or 64-hex Git object ID"
    );
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GithubRepository {
    owner: String,
    repo: String,
}

fn github_repository_from_remote_url(remote_url: &str) -> Result<GithubRepository> {
    let path = if let Some(path) = strip_ascii_prefix(remote_url, "https://github.com/") {
        path
    } else if let Some(path) = strip_ascii_prefix(remote_url, "git@github.com:") {
        path
    } else if let Some(path) = strip_ascii_prefix(remote_url, "ssh://git@github.com/") {
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

fn strip_ascii_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &value[prefix.len()..])
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

fn github_branch_metadata_path(repository: &GithubRepository, branch: &str) -> Result<String> {
    let mut encoded = String::with_capacity(branch.len());
    for byte in branch.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    anyhow::ensure!(!encoded.is_empty(), "GitHub branch metadata path is empty");
    Ok(format!(
        "repos/{}/{}/branches/{encoded}",
        repository.owner, repository.repo
    ))
}

fn parse_github_branch_protected_response(response: &str, branch: &str) -> Result<bool> {
    let value: Value = serde_json::from_str(response)
        .context("GitHub branch metadata response is not valid JSON")?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .context("GitHub branch metadata response is missing name")?;
    anyhow::ensure!(
        name == branch,
        "GitHub branch metadata name does not match request"
    );
    value
        .get("protected")
        .and_then(Value::as_bool)
        .context("GitHub branch metadata response is missing protected state")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GithubApiMethod {
    Get,
}

struct GithubApiCall<'a> {
    method: GithubApiMethod,
    path: &'a str,
    #[cfg_attr(not(feature = "network"), allow(dead_code))]
    body: Option<&'a Value>,
}

/// Executes the remote-branch-delete protection query with the repository
/// context pinned before approval.  In particular, credential/config lookup
/// must not resolve a replacement `cwd` pathname after approval.
async fn run_pinned_github_api_after_approval(
    pinned: &sandbox::PinnedGitRepository,
    repository: &GithubRepository,
    call: GithubApiCall<'_>,
) -> Result<String> {
    #[cfg(feature = "network")]
    {
        let token = repo_scoped_github_token_pinned(pinned, repository).await?;
        github_api_request(&token, call.method, call.path, call.body).await
    }
    #[cfg(not(feature = "network"))]
    {
        let _ = (pinned, repository, call);
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
    github_api_request_with_query(token, method, path, None, body, GithubApiContext::Workflow).await
}

/// Bounded query allowlist for internally constructed request queries. A query
/// never carries caller text, credentials or arbitrary endpoints.
fn github_api_query_is_bounded(query: &str) -> bool {
    !query.is_empty()
        && query.len() <= 256
        && !query.starts_with('?')
        && !query.starts_with('&')
        && !query.ends_with('&')
        && !query.contains("&&")
        && !query.contains("..")
        && query.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '=' | '&' | '_' | '-' | '.')
        })
}

fn github_api_path_is_bounded(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes.len() > 1024 || bytes.starts_with(b"/") {
        return false;
    }

    // This is lexical containment validation, not an API routing allowlist.
    // Callers remain responsible for constructing their fixed GitHub routes.
    for segment in bytes.split(|byte| *byte == b'/') {
        if segment.is_empty() {
            return false;
        }

        let mut decoded = Vec::with_capacity(segment.len());
        let mut index = 0;
        while index < segment.len() {
            let byte = segment[index];
            if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
                decoded.push(byte);
                index += 1;
                continue;
            }

            if byte != b'%' || index + 2 >= segment.len() {
                return false;
            }
            let Some(high) = (segment[index + 1] as char).to_digit(16) else {
                return false;
            };
            let Some(low) = (segment[index + 2] as char).to_digit(16) else {
                return false;
            };
            decoded.push(((high << 4) | low) as u8);
            index += 3;
        }

        if decoded == b"." || decoded == b".." {
            return false;
        }
        if decoded
            .iter()
            .any(|byte| matches!(byte, b'/' | b'\\' | b'\0') || byte.is_ascii_control())
        {
            return false;
        }
        if std::str::from_utf8(&decoded).is_err() {
            return false;
        }
    }

    true
}

/// Selects the bounded, secret-free error wording for one GitHub API surface.
///
/// The status code is the only signal; response bodies are never echoed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GithubApiContext {
    Workflow,
}

impl GithubApiContext {
    const fn error_message(self, status: u16) -> &'static str {
        match (self, status) {
            (_, 401) => "GitHub repository credential was rejected",
            (_, 403) => "GitHub repository credential lacks required permission",
            (Self::Workflow, 404) => "GitHub repository workflow/run is unavailable",
            (Self::Workflow, 422) => "GitHub workflow request was rejected",
            _ => "GitHub API operation failed",
        }
    }
}

#[cfg(feature = "network")]
#[cfg(feature = "network")]
async fn repo_scoped_github_token_pinned(
    pinned: &sandbox::PinnedGitRepository,
    repository: &GithubRepository,
) -> Result<zeroize::Zeroizing<String>> {
    let local_helpers = run_pinned_git_inspection(
        pinned,
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
    let local_use_http_path = run_pinned_git_inspection(
        pinned,
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
    let output = sandbox::run_pinned_git_command(
        pinned,
        &github_managed_credential_command(),
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

async fn git_config_values(
    session: &config::Session,
    cwd: &Path,
    key: &str,
) -> Result<Vec<String>> {
    git_config_values_with_scope(session, cwd, key, false).await
}

async fn git_config_values_with_scope(
    session: &config::Session,
    cwd: &Path,
    key: &str,
    local_only: bool,
) -> Result<Vec<String>> {
    let mut command = vec!["git".to_owned(), "config".to_owned()];
    if local_only {
        command.push("--local".to_owned());
    }
    command.extend(["--get-all".to_owned(), key.to_owned()]);
    let output = run_host_git_inspection(session, cwd, &command).await?;
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

async fn git_local_config_values_pinned(
    pinned: &sandbox::PinnedGitRepository,
    key: &str,
) -> Result<Vec<String>> {
    let command = vec![
        "git".to_owned(),
        "config".to_owned(),
        "--local".to_owned(),
        "--get-all".to_owned(),
        key.to_owned(),
    ];
    let output = run_pinned_git_inspection(pinned, &command).await?;
    anyhow::ensure!(
        !output.truncated && output.stdout.len() <= MAX_GIT_CONFIG_OUTPUT_BYTES,
        GIT_PULL_UPSTREAM_CONFIGURATION_ERROR
    );
    if output.status != 0 {
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

async fn run_pinned_git_inspection(
    pinned: &sandbox::PinnedGitRepository,
    command: &[String],
) -> Result<sandbox::Output> {
    sandbox::run_pinned_git_command(
        pinned,
        command,
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
    run_git_output_after_approval(session, cwd, command, activity, github_https_destination).await
}

async fn run_git_output_after_approval(
    session: &config::Session,
    cwd: PathBuf,
    command: Vec<String>,
    activity: Option<&ActivityScope>,
    github_https_destination: bool,
) -> Result<sandbox::Output> {
    let repository_root = sandbox::git_worktree_root(&cwd)?;
    config::ensure_permitted(session, &repository_root)
        .context("Git repository root must be inside a permitted session root")?;
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

async fn run_pinned_git_output_after_approval(
    session: &config::Session,
    pinned: &sandbox::PinnedGitRepository,
    command: Vec<String>,
    activity: Option<&ActivityScope>,
    github_https_destination: bool,
) -> Result<sandbox::Output> {
    let rendered_command = render_command(&command);
    approvals::activity(&session.id, format!("Running {rendered_command}"), None).await;
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    let output = sandbox::run_pinned_git_command(
        pinned,
        &command,
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

/// Runs the exact merged-only local branch deletion on the host.
///
/// Native branch deletion always acquires `packed-refs.lock`, which the general
/// Git sandbox deliberately keeps read-only. The caller validates the branch,
/// obtains approval and re-proves the preconditions immediately before this
/// exact bounded host-side mutation.
async fn run_git_branch_delete_and_report(
    session: &config::Session,
    pinned: sandbox::PinnedGitRepository,
    branch: &str,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    let command = build_git_branch_delete_command(branch);
    let rendered_command = render_command(&command);
    approvals::activity(
        &session.id,
        "Delete merged Git branch",
        Some(rendered_command.clone()),
    )
    .await;
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    let output = sandbox::run_pinned_git_command(
        &pinned,
        &command,
        None,
        &HashMap::new(),
        child_env::SENSITIVE_ENV_NAMES,
    )
    .await;
    let result = output.and_then(render_output);
    report_command_finished(session.id.clone(), "git", &rendered_command, &result).await;
    text_result(result?)
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

pub(crate) async fn git_worktree_remove_in_src_root(
    args: &Value,
    session: &config::Session,
    src_root: &Path,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    git_worktree_remove_in_src_root_impl(args, session, src_root, activity, None).await
}

/// One bounded authenticated GitHub API request.
///
/// The path and query are always constructed by Temote; callers never pass
/// arbitrary request text. `query` is validated against a fixed allowlist and
/// is only ever the internal pull-request list shape.
#[cfg(feature = "network")]
async fn github_api_request_with_query(
    token: &str,
    method: GithubApiMethod,
    path: &str,
    query: Option<&str>,
    body: Option<&Value>,
    context: GithubApiContext,
) -> Result<String> {
    const MAX_GITHUB_API_RESPONSE_BYTES: u64 = 256 * 1024;
    anyhow::ensure!(
        !path.is_empty()
            && path.len() <= 1024
            && !path.starts_with('/')
            && !path.contains("..")
            && github_api_path_is_bounded(path),
        "GitHub API path is invalid"
    );
    if let Some(query) = query {
        anyhow::ensure!(
            github_api_query_is_bounded(query),
            "GitHub API query is invalid"
        );
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .user_agent(format!("temote-mcp/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to initialize GitHub API client")?;
    let mut url = format!("https://api.github.com/{path}");
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    let mut request = match method {
        GithubApiMethod::Get => client.get(url),
    }
    .header("Accept", "application/vnd.github+json")
    .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(body);
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("GitHub API is unavailable"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!(context.error_message(status.as_u16()));
    }
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= MAX_GITHUB_API_RESPONSE_BYTES,
            "GitHub API response is too large"
        );
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("GitHub API response could not be read"))?
    {
        append_github_api_response_chunk(
            &mut bytes,
            &chunk,
            MAX_GITHUB_API_RESPONSE_BYTES as usize,
        )?;
    }
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("GitHub API response is not UTF-8"))
}

#[cfg_attr(not(feature = "network"), allow(dead_code))]
fn append_github_api_response_chunk(
    buffer: &mut Vec<u8>,
    chunk: &[u8],
    maximum: usize,
) -> Result<()> {
    anyhow::ensure!(
        buffer.len() <= maximum && chunk.len() <= maximum.saturating_sub(buffer.len()),
        "GitHub API response is too large"
    );
    buffer.extend_from_slice(chunk);
    Ok(())
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
            ["local_agent_run"].into_iter().collect()
        );
        assert_eq!(
            ACTIVITY_TOOL_COVERAGE
                .iter()
                .filter(|coverage| coverage.success == ActivitySuccess::Accepted)
                .map(|coverage| coverage.name)
                .collect::<std::collections::BTreeSet<_>>(),
            [
                "codex_task_control",
                "codex_task_start",
                "devin_cloud_task_control",
                "devin_cloud_task_start",
                "devin_task_control",
                "devin_task_start",
                "opencode_task_control",
                "opencode_task_start",
            ]
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

        let (worker_scope, worker_emitter) = activity_job_scope(ActivityOperation::LocalAgentRun);
        finish_covered_tool_activity(
            activity_tool_coverage("local_agent_run"),
            Some(&worker_scope),
            &text_result("backgrounded".to_owned()),
        );
        assert_eq!(worker_emitter.states(), vec![ActivityState::Started]);

        let (failure_scope, failure_emitter) = activity_job_scope(ActivityOperation::StopJob);
        let failed: Result<Value> = Err(anyhow::anyhow!("raw-secret-sentinel"));
        finish_covered_tool_activity(
            activity_tool_coverage("stop_job"),
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
        let (allowed_scope, allowed_emitter) = activity_job_scope(ActivityOperation::StopJob);
        allowed_scope.waiting_approval().unwrap();
        finish_activity_approval(true, Some(&allowed_scope), "denied").unwrap();
        finish_covered_tool_activity(
            activity_tool_coverage("stop_job"),
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

        let (denied_scope, denied_emitter) = activity_job_scope(ActivityOperation::StopJob);
        denied_scope.waiting_approval().unwrap();
        let denied = finish_activity_approval(false, Some(&denied_scope), "denied");
        assert!(denied.is_err());
        let outer_failure: Result<Value> = Err(anyhow::anyhow!("denied"));
        finish_covered_tool_activity(
            activity_tool_coverage("stop_job"),
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
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
    fn quotes_command_arguments_for_activity_display() {
        assert_eq!(shell_word("README.md"), "README.md");
        assert_eq!(shell_word("hello world"), "\"hello world\"");
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
        assert_eq!(tools.len(), 26);
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
        for name in [
            "session_list",
            "session_start",
            "session_stop",
            "session_restart",
            "session_info",
            "evidence_read",
            "poll_job",
            "job_list",
            "stop_job",
        ] {
            assert!(tools.iter().any(|tool| tool["name"] == name), "{name}");
        }
        for name in ["git_add", "execute", "read_file", "onepassword_status"] {
            assert!(!tools.iter().any(|tool| tool["name"] == name), "{name}");
        }
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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
                grants: config::SessionGrants::default(),
            };
            let other = config::Session {
                id: other_id,
                cwd,
                permitted_directories: Vec::new(),
                started_at: 0,
                process_id: 0,
                permission_mode: config::PermissionMode::Yolo,
                grants: config::SessionGrants::default(),
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
                grants: config::SessionGrants::default(),
            };
            let other = config::Session {
                id: other_id,
                cwd,
                permitted_directories: Vec::new(),
                started_at: 0,
                process_id: 0,
                permission_mode: config::PermissionMode::Yolo,
                grants: config::SessionGrants::default(),
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
                grants: config::SessionGrants::default(),
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
                grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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

    fn git_ref_exists(cwd: &Path, reference: &str) -> bool {
        std::process::Command::new("git")
            .args(["show-ref", "--verify", "--quiet", reference])
            .current_dir(cwd)
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn generated_branch_delete_commands_stay_in_heads_namespace() -> noprop::TestResult {
        test_support::run(0x4252_414e_4348_444c, 1024, |ctx| {
            let branch = format!("review/{:016x}", noprop::sample_u64(ctx));
            let expected = format!("{:040x}", noprop::sample_u64(ctx));
            let local = build_git_branch_delete_command(&branch);
            let remote = build_git_remote_branch_delete_command("origin", &branch, &expected);
            assert_eq!(local.last(), Some(&branch));
            assert_eq!(local[local.len() - 2], "--");
            assert_eq!(
                remote[8],
                format!("--force-with-lease=refs/heads/{branch}:{expected}")
            );
            assert_eq!(remote[9], "origin");
            assert_eq!(remote[10], format!(":refs/heads/{branch}"));
            assert!(!remote.iter().any(|argument| argument == "--force"));
            assert!(
                !remote
                    .iter()
                    .any(|argument| argument.contains("refs/tags/"))
            );
            Ok(())
        })
    }

    #[test]
    fn branch_delete_authority_parsers_fail_closed_on_ambiguous_state() {
        let default = sandbox::Output {
            status: 0,
            stdout: "ref: refs/heads/main\tHEAD\n0123456789abcdef0123456789abcdef01234567\tHEAD\n"
                .to_owned(),
            stderr: String::new(),
            truncated: false,
        };
        assert_eq!(parse_remote_default_branch(&default).unwrap(), "main");
        for output in [
            sandbox::Output {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
            },
            sandbox::Output {
                status: 0,
                stdout: "ref: refs/heads/main\tHEAD\n".to_owned(),
                stderr: String::new(),
                truncated: false,
            },
            sandbox::Output {
                status: 0,
                stdout: "ref: refs/heads/main\tHEAD\nnot-a-sha\tHEAD\n".to_owned(),
                stderr: String::new(),
                truncated: false,
            },
        ] {
            assert!(parse_remote_default_branch(&output).is_err());
        }

        assert!(
            parse_github_branch_protected_response(r#"{"name":"main","protected":true}"#, "main")
                .unwrap()
        );
        assert!(
            !parse_github_branch_protected_response(
                r#"{"name":"review","protected":false}"#,
                "review"
            )
            .unwrap()
        );
        assert!(
            parse_github_branch_protected_response(r#"{"name":"main","protected":true}"#, "other")
                .is_err()
        );

        let repository = GithubRepository {
            owner: "example".to_owned(),
            repo: "repository".to_owned(),
        };
        let path = github_branch_metadata_path(&repository, "feature/review@1").unwrap();
        assert!(path.ends_with("/branches/feature/review%401"));
        assert!(github_api_path_is_bounded(&path));
        assert!(!github_api_path_is_bounded(
            "repos/example/repository/branches/%"
        ));
    }

    #[tokio::test]
    async fn structured_git_branch_delete_is_merged_only_and_worktree_safe() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        init_git_repository(&repository);
        let repository = std::fs::canonicalize(repository).unwrap();
        let session = config::Session {
            id: format!("branch-delete-{}", Uuid::new_v4()),
            cwd: repository.clone(),
            permitted_directories: vec![repository.clone()],
            started_at: 1,
            process_id: std::process::id(),
            permission_mode: config::PermissionMode::Agent,
            grants: config::SessionGrants::default(),
        };

        run_git_fixture(&repository, &["branch", "merged"]);
        std::fs::write(repository.join("preserve-untracked.txt"), "preserve\n").unwrap();
        git_branch_delete(
            &json!({"session_id": session.id, "branch": "merged"}),
            &session,
            None,
        )
        .await
        .unwrap();
        assert!(!git_ref_exists(&repository, "refs/heads/merged"));
        assert_eq!(
            std::fs::read_to_string(repository.join("preserve-untracked.txt")).unwrap(),
            "preserve\n"
        );

        run_git_fixture(&repository, &["switch", "--quiet", "-c", "unmerged"]);
        std::fs::write(repository.join("unmerged.txt"), "unmerged\n").unwrap();
        run_git_fixture(&repository, &["add", "unmerged.txt"]);
        run_git_fixture(&repository, &["commit", "--quiet", "-m", "unmerged"]);
        run_git_fixture(&repository, &["switch", "--quiet", "main"]);
        let unmerged_error = git_branch_delete(
            &json!({"session_id": session.id, "branch": "unmerged"}),
            &session,
            None,
        )
        .await
        .unwrap_err();
        assert!(unmerged_error.to_string().contains("not fully merged"));
        assert!(git_ref_exists(&repository, "refs/heads/unmerged"));

        let current_error = git_branch_delete(
            &json!({"session_id": session.id, "branch": "main"}),
            &session,
            None,
        )
        .await
        .unwrap_err();
        assert!(current_error.to_string().contains("checked out"));
        assert!(git_ref_exists(&repository, "refs/heads/main"));

        run_git_fixture(&repository, &["branch", "linked"]);
        let linked = root.path().join("linked");
        run_git_fixture(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                linked.to_str().unwrap(),
                "linked",
            ],
        );
        let linked_error = git_branch_delete(
            &json!({"session_id": session.id, "branch": "linked"}),
            &session,
            None,
        )
        .await
        .unwrap_err();
        assert!(linked_error.to_string().contains("checked out"));
        assert!(git_ref_exists(&repository, "refs/heads/linked"));

        let absent_error = git_branch_delete(
            &json!({"session_id": session.id, "branch": "absent"}),
            &session,
            None,
        )
        .await
        .unwrap_err();
        assert!(absent_error.to_string().contains("does not exist"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn structured_git_branch_delete_stays_bound_after_cwd_path_swap() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        let replacement = root.path().join("replacement");
        let original = root.path().join("original");
        std::fs::create_dir(&repository).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        init_git_repository(&repository);
        init_git_repository(&replacement);
        run_git_fixture(&repository, &["branch", "merged"]);
        run_git_fixture(&replacement, &["branch", "merged"]);

        let id = format!("branch-delete-swap-{}", Uuid::new_v4());
        let (sender, mut receiver) = approvals::approval_channel();
        let runtime = approvals::spawn_runtime(root.path(), Some(&id), false, sender)
            .await
            .unwrap();
        let session = config::load_session(&id).await.unwrap();
        let request = json!({
            "session_id": id,
            "cwd": repository.to_string_lossy(),
            "branch": "merged",
        });
        let task = tokio::spawn(async move { git_branch_delete(&request, &session, None).await });

        let prompt = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("branch delete did not request approval")
            .expect("approval channel closed before branch delete request");
        assert_eq!(prompt.request.operation, "git_branch_delete");
        std::fs::rename(&repository, &original).unwrap();
        std::fs::rename(&replacement, &repository).unwrap();
        prompt.respond(true);

        task.await.unwrap().unwrap();
        assert!(!git_ref_exists(&original, "refs/heads/merged"));
        assert!(git_ref_exists(&repository, "refs/heads/merged"));
        runtime.shutdown().await.unwrap();
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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
            build_git_push_command("origin", false),
            vec![
                "git".to_owned(),
                "-c".to_owned(),
                "core.hooksPath=/dev/null".to_owned(),
                "-c".to_owned(),
                "push.recurseSubmodules=off".to_owned(),
                "-c".to_owned(),
                "push.followTags=false".to_owned(),
                "push".to_owned(),
                "origin".to_owned(),
                "HEAD".to_owned(),
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
            grants: config::SessionGrants::default(),
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

        // A local destination never contacts a network remote, but remains
        // the exact destination passed to the bounded mutation command.
        run_git_fixture(
            &repository,
            &["config", "--local", "branch.main.remote", "."],
        );
        assert_eq!(
            git_current_push_remote(&session, &repository)
                .await
                .unwrap(),
            Some(".".to_owned())
        );
        assert_eq!(
            build_git_push_command(".", false)
                .iter()
                .rev()
                .take(2)
                .cloned()
                .collect::<Vec<_>>(),
            vec!["HEAD".to_owned(), ".".to_owned()]
        );
        let explicit_local = git_push_output(
            &session,
            repository.clone(),
            Some(".".to_owned()),
            false,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            explicit_local.to_string().contains("not configured"),
            "{explicit_local:#}"
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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
            grants: config::SessionGrants::default(),
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

    #[cfg(unix)]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn github_api_response_chunk_accumulator_is_bounded() {
        let mut empty = Vec::new();
        append_github_api_response_chunk(&mut empty, &[], 3).unwrap();
        assert!(empty.is_empty());

        append_github_api_response_chunk(&mut empty, b"abc", 3).unwrap();
        assert_eq!(empty, b"abc");

        let before = empty.clone();
        assert_eq!(
            append_github_api_response_chunk(&mut empty, b"d", 3)
                .unwrap_err()
                .to_string(),
            "GitHub API response is too large"
        );
        assert_eq!(empty, before);
    }

    #[test]
    fn github_api_response_chunk_rejects_preexisting_overflow() {
        let mut overflow = vec![1, 2, 3];
        assert!(append_github_api_response_chunk(&mut overflow, &[], 2).is_err());
        assert_eq!(overflow, vec![1, 2, 3]);

        let mut nonempty_at_zero = vec![1];
        assert!(append_github_api_response_chunk(&mut nonempty_at_zero, &[], 0).is_err());
        assert_eq!(nonempty_at_zero, vec![1]);

        let mut empty = Vec::new();
        assert!(append_github_api_response_chunk(&mut empty, &[1], 0).is_err());
        assert!(empty.is_empty());

        append_github_api_response_chunk(&mut empty, &[], 0).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn github_api_path_validation_rejects_traversal_and_preserves_internal_routes() {
        for path in [
            "repos/f4ah6o/temote-mcp/pulls/7",
            "repos/f4ah6o/temote-mcp/actions/workflows/release.yml/dispatches",
            "repos/example/repository/branches/feature/review%401",
            "repos/example/repository/branches/%E6%96%B0",
            "repos/example/repository/branches/literal%25data",
        ] {
            assert!(github_api_path_is_bounded(path), "{path}");
        }

        for path in [
            "",
            "../pulls",
            "/pulls",
            "repos/x/y/../pulls",
            "repos/x/y/./pulls",
            "repos//y/pulls",
            "repos/x/y/pulls/",
            "repos/x/y/%2e/pulls",
            "repos/x/y/.%2E/pulls",
            "repos/x/y/%2e%2E/pulls",
            "repos/x/y/%2Fpulls",
            "repos/x/y/%5cpulls",
            "repos/x/y/%00",
            "repos/x/y/%0a",
            "repos/x/y?state=open",
            "repos/x/y#f",
            "repos/x/y/malformed%",
            "repos/x/y/%G0",
        ] {
            assert!(!github_api_path_is_bounded(path), "{path}");
        }

        assert!(github_api_path_is_bounded(&"a".repeat(1024)));
        assert!(!github_api_path_is_bounded(&"a".repeat(1025)));
    }

    #[test]
    fn github_pr_repository_identity_resolution_fails_closed() {
        for valid in [
            "https://github.com/f4ah6o/temote-mcp",
            "https://github.com/f4ah6o/temote-mcp.git",
            "git@github.com:f4ah6o/temote-mcp.git",
            "ssh://git@github.com/f4ah6o/temote-mcp",
        ] {
            let repository = github_repository_from_remote_url(valid).unwrap();
            assert_eq!(repository.owner, "f4ah6o", "{valid}");
            assert_eq!(repository.repo, "temote-mcp", "{valid}");
        }
        for invalid in [
            "",
            "https://gitlab.com/f4ah6o/temote-mcp",
            "https://github.com/f4ah6o",
            "https://github.com/f4ah6o/temote-mcp/extra",
            "https://github.com/f4ah6o/temote-mcp?ref=main",
            "https://github.com/f4ah6o/temote-mcp#frag",
            "https://github.com/../temote-mcp",
            "file:///tmp/repo",
        ] {
            assert!(
                github_repository_from_remote_url(invalid).is_err(),
                "{invalid:?} must be rejected"
            );
        }
        assert_eq!(
            github_repository_from_destination("https://example.com/f4ah6o/temote-mcp").unwrap(),
            None
        );
        assert_eq!(
            github_repository_from_destination("https://github.com/f4ah6o/temote-mcp.git").unwrap(),
            Some(GithubRepository {
                owner: "f4ah6o".to_owned(),
                repo: "temote-mcp".to_owned(),
            })
        );
    }

    #[test]
    fn github_pr_broker_never_mutates_the_global_gh_account() {
        assert_eq!(
            github_managed_credential_command(),
            vec!["gh", "git", "credential", "--managed", "get"]
        );
        assert!(
            !github_managed_credential_command()
                .iter()
                .any(|argument| matches!(
                    argument.as_str(),
                    "auth" | "switch" | "login" | "logout" | "token"
                )),
            "the pull-request broker must never touch global gh account state"
        );
        // The credential mapping contract is the same repo-local one the
        // structured workflow operations already require.
        assert!(repo_scoped_github_credential_mapping_valid(
            "\n!gh git credential --managed\n",
            "true\n"
        ));
        assert!(!repo_scoped_github_credential_mapping_valid(
            "\n!gh auth token\n",
            "true\n"
        ));
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
            &[],
        )
        .await
        .unwrap();
        assert_ne!(output.status, 0, "sandbox must deny writes outside roots");
        assert!(!marker.exists());
        let _ = std::fs::remove_file(&marker);
    }
}
