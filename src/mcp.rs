use std::collections::{BTreeMap, HashMap};
#[cfg(test)]
use std::path::Path;
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
    activity_runtime, approvals, codex_app_server, config, evidence, sandbox,
    session_control::SessionBackend,
};
use temote_mcp::activity::contract::{
    ActivityCancellationReason, ActivityErrorKind, ActivityOperation, ActivityResult,
    ActivitySummary,
};
use temote_mcp::activity::scope::ActivityScope;

const COMPLETED_JOB_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_COMPLETED_JOBS_PER_SESSION: usize = 128;
const MAX_COMPLETED_JOBS_TOTAL: usize = 1024;
const MAX_PATH_ARGUMENT_BYTES: usize = 4096;
const MAX_RPC_METHOD_BYTES: usize = 256;
const MAX_RPC_ID_STRING_BYTES: usize = 256;
const MAX_MCP_TOOL_NAME_BYTES: usize = 256;
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

#[derive(Clone)]
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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

struct JobState {
    jobs: HashMap<Uuid, Job>,
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

fn activity_tool_summary(_args: &Value, _operation: ActivityOperation) -> ActivitySummary {
    ActivitySummary::empty()
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

fn required_session_id(args: &Value) -> Result<String> {
    let value = args
        .get("session_id")
        .and_then(Value::as_str)
        .context("missing session_id; ask the user to run `temote-mcp start` and provide its ID")?;
    config::validate_session_id(value)?;
    Ok(value.to_owned())
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

    fn activity_job_scope(
        operation: ActivityOperation,
    ) -> (ActivityScope, RecordingActivityEmitter) {
        let emitter = RecordingActivityEmitter::default();
        let scope = ActivityScope::new(operation, emitter.clone());
        (scope, emitter)
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
            std::collections::BTreeSet::new()
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
        assert_eq!(tools.len(), 25);
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
        assert!(tools.iter().all(|tool| tool["name"] != "without_sandbox"));
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
