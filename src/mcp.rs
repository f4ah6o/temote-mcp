use std::collections::{BTreeMap, HashMap};
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
    apply_patch, approvals, checkpoints, child_env, codex_app_server, config, evidence, friction,
    local_agent, onepassword_cli, onepassword_mcp, onepassword_sdk, recall, sandbox,
    session_control::SessionBackend, work_handoff,
};

const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ACTIVE_JOBS_PER_SESSION: usize = 8;
const MAX_JOB_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);
const COMPLETED_JOB_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_COMPLETED_JOBS_PER_SESSION: usize = 128;
const MAX_COMPLETED_JOBS_TOTAL: usize = 1024;
const MAX_GIT_ADD_PATHS: usize = 256;
const MAX_PATH_ARGUMENT_BYTES: usize = 4096;
const MAX_RPC_METHOD_BYTES: usize = 256;
const MAX_RPC_ID_STRING_BYTES: usize = 256;
const MAX_MCP_TOOL_NAME_BYTES: usize = 256;
const MAX_COMMAND_ARGUMENTS: usize = 256;
const MAX_COMMAND_ARGUMENT_BYTES: usize = 32 * 1024;
const MAX_COMMAND_TOTAL_BYTES: usize = 128 * 1024;
const MAX_GIT_COMMIT_MESSAGE_BYTES: usize = 16 * 1024;
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
}

struct Job {
    session_id: String,
    command: String,
    handle: JoinHandle<()>,
    completion: Arc<Mutex<JobCompletion>>,
    output_policy: OutputPolicy,
}

struct JobSlot {
    session_id: String,
}

impl Drop for JobSlot {
    fn drop(&mut self) {
        release_job_slot(&self.session_id);
    }
}

struct JobState {
    jobs: HashMap<Uuid, Job>,
    active_by_session: HashMap<String, usize>,
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
        "initialize" => Ok(json!({
            "protocolVersion": negotiate_protocol_version(request),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "temote-mcp", "title": "Temote MCP", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Call session_list first. On the public serve endpoint, use session_start with a configured named-root path when the required project session is absent, then call session_info before normal tools. Managed sessions are always normal sandboxed sessions; remote clients cannot create yolo sessions or self-approve host operations. A CLI session started locally with `temote-mcp start <session-id> --yolo` remains a separate local choice. The session mode does not control confirmation or authorization enforced by the MCP client."
        })),
        "server/discover" => Ok(discover_result()),
        "ping" => Ok(json!({})),
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

fn server_info() -> Value {
    json!({
        "name": "temote-mcp",
        "title": "Temote MCP",
        "version": env!("CARGO_PKG_VERSION")
    })
}

fn discover_result() -> Value {
    json!({
        "resultType": "complete",
        "supportedVersions": [MODERN_PROTOCOL_VERSION],
        "capabilities": {"tools": {"listChanged": false}},
        "instructions": SERVER_INSTRUCTIONS,
        "ttlMs": 0,
        "cacheScope": "private",
        "_meta": {"io.modelcontextprotocol/serverInfo": server_info()}
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
    object.insert(
        "_meta".to_owned(),
        json!({"io.modelcontextprotocol/serverInfo": server_info()}),
    );
    if method == "tools/list" {
        object.insert("ttlMs".to_owned(), json!(0));
        object.insert("cacheScope".to_owned(), json!("private"));
    }
    result
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
            "access":{"type":"string","enum":["read_only","workspace_write"]},
            "model":{"type":"string","minLength":1,"maxLength":256},
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

fn tools(public: bool, managed_sessions: bool) -> Value {
    let mut tools = json!([
        {"name":"session_list","title":"List Temote MCP sessions","description":"List active temote-mcp sessions and surface sessions whose liveness cannot be safely determined. Returns session IDs, working directories, start times, status, and whether each session is in yolo mode.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
        {"name":"session_start","title":"Start a managed Temote MCP session","description":"Start a normal sandboxed session under a host-configured named root. Path must be <root-name> or <root-name>/<relative-path>; absolute paths and yolo creation are unavailable.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"path":{"type":"string"},"session_id":{"type":"string"}},"required":["path"],"additionalProperties":false}},
        {"name":"session_stop","title":"Stop a managed Temote MCP session","description":"Gracefully stop a session created through the authenticated HTTP endpoint and owned by the local Temote session supervisor. Local CLI/yolo sessions cannot be stopped remotely.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"session_restart","title":"Restart a managed Temote MCP session","description":"Restart an active normal sandboxed session created through the authenticated HTTP endpoint. Local CLI/yolo sessions cannot be restarted remotely.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"session_info","title":"Inspect a Temote MCP session","description":"Show durable lifecycle state, working directory, permission mode, exit reason, and last error for a temote-mcp session.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"read_file","title":"Read a local file","description":"Read a UTF-8 regular file up to 8 MiB. With optional start_line/end_line or offset_bytes plus max_bytes, return bounded range metadata with an unambiguous UTF-8 next offset. Omitting range arguments preserves whole-file behavior.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1},"offset_bytes":{"type":"integer","minimum":0},"max_bytes":{"type":"integer","minimum":4,"maximum":8388608}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"evidence_read","title":"Read scoped Temote evidence","description":"Read a bounded UTF-8 chunk from an opaque expiring evidence record previously returned by Temote. Evidence is in-memory, session-owned, canonical-scope-bound, and cannot address arbitrary filesystem paths.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"evidence_id":{"type":"string","format":"uuid"},"offset_bytes":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":65536,"default":16384}},"required":["session_id","evidence_id"],"additionalProperties":false}},
        {"name":"codex_status","title":"Check Codex app-server compatibility","description":"Check the locally installed Codex app-server through stdio, fail closed unless the supported protocol version is present, and return only model/effort compatibility metadata.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"codex_task_start","title":"Start a scoped Codex task","description":"Accept an idempotent scoped Codex task mutation, persist acceptance before child side effects, then start a workspace-write Codex app-server thread/turn. operation_id is mandatory; no sandbox escape option is exposed.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"operation_id":{"type":"string","format":"uuid"},"task":{"type":"string","minLength":1,"maxLength":1048576},"model":{"type":"string","minLength":1,"maxLength":256},"effort":{"type":"string","minLength":1,"maxLength":256}},"required":["session_id","operation_id","task","model","effort"],"additionalProperties":false}},
        {"name":"codex_task_get","title":"Read a scoped Codex task","description":"Read and reconcile a retained Codex task owned by the full Temote session instance and canonical scope. Detailed thread data is exposed only through bounded scoped evidence.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"after_revision":{"type":"integer","minimum":0}},"required":["session_id","task_id"],"additionalProperties":false}},
        {"name":"codex_task_control","title":"Control a scoped Codex task","description":"Idempotently steer, resume, or interrupt a retained scoped Codex task. Acceptance is persisted before the app-server side effect; uncertain crash gaps return reconciliation_required rather than replaying blindly.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"task_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid"},"action":{"type":"string","enum":["steer","resume","interrupt"]},"input":{"type":"string","minLength":1,"maxLength":1048576}},"required":["session_id","task_id","operation_id","action"],"additionalProperties":false}},
        {"name":"local_agent_run","title":"Run a local coding agent","description":"Run a verified Codex or OpenCode non-interactive agent in the selected session with canonical workspace scope, bounded task/output, isolated agent state, and local approval. The caller supplies a task and access mode, not an executable, raw argv, environment, or network policy.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":local_agent_input_schema()},
        {"name":"get_image","title":"Read a local image","description":"Read a local image up to 32 MiB and return it as MCP image content. Relative paths use the session working directory.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string","description":"Path to a PNG, JPEG, GIF, WebP, BMP, TIFF, or AVIF image."}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"list_directory","title":"List a local directory","description":"List up to 10,000 entries from a local directory, with at most 1 MiB of rendered names. Relative paths use the session working directory.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"write_file","title":"Write a local file","description":"Write a UTF-8 regular file using the selected session permission mode. Existing special-file targets are rejected. Normal sessions are restricted to permitted roots and use the temote-mcp sandbox; yolo sessions may write anywhere the local user can.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"},"content":{"type":"string"}},"required":["session_id","path","content"],"additionalProperties":false}},
        {"name":"apply_patch","title":"Apply a bounded multi-file patch","description":"Parse a Codex-style *** Begin Patch patch, preflight every source and destination inside the session roots, request approval once for normal sessions, then apply add/update/move/delete operations without invoking a shell parser. Partial I/O failure reports the exact committed operations.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"patch":{"type":"string","minLength":1,"maxLength":1048576}},"required":["session_id","patch"],"additionalProperties":false}},
        {"name":"git_add","title":"Stage files with Git","description":"Stage existing files or directories in the session repository with git add. Only the specified paths are staged; Git hooks and network access are unavailable.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":256},"cwd":{"type":"string"}},"required":["session_id","paths"],"additionalProperties":false}},
        {"name":"git_commit","title":"Create a local Git commit","description":"Create a local commit from the current Git index. This does not push, hooks and signing are disabled, and network access is unavailable.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"message":{"type":"string","minLength":1,"maxLength":16384},"cwd":{"type":"string"}},"required":["session_id","message"],"additionalProperties":false}},
        {"name":"git_fetch","title":"Fetch Git remote updates","description":"Run git fetch --prune for a configured remote on the host. The remote must be a safe configured name and arbitrary URLs and refspecs are not accepted. temote-mcp requests local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"remote":{"type":"string","default":"origin"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"git_pull","title":"Fast-forward Git branch","description":"Run git pull --ff-only for the current branch and its configured upstream on the host. Hooks are disabled. temote-mcp requests local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"git_push","title":"Push current Git branch","description":"Push the current branch on the host without force options. Optionally set origin (or another safe configured remote) as the upstream. Hooks are disabled. temote-mcp requests local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"remote":{"type":"string"},"set_upstream":{"type":"boolean","default":false}},"required":["session_id"],"additionalProperties":false}},
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
        anyhow::ensure!(
            args.as_object().is_some_and(|object| object.is_empty()),
            "session_list takes no arguments"
        );
        return session_list().await;
    }
    if name == "session_start" {
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
        let view = crate::session_control::inspect_session(&session_id).await?;
        if matches!(view.status.as_str(), "starting" | "active" | "stopping") {
            approvals::activity(&view.session_id, "Read session info", None).await;
        }
        return text_result(serde_json::to_string_pretty(&view)?);
    }
    let session = config::load_session(&session_id).await?;
    anyhow::ensure!(
        !public || !session.yolo,
        "yolo sessions are unavailable on the public MCP endpoint"
    );
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
            authorize_codex_operation(&session, "codex_status", detail, metadata).await?;
            text_result(serde_json::to_string_pretty(
                &codex_app_server::status(&session).await?,
            )?)
        }
        "codex_task_start" => {
            let (detail, metadata) = codex_task_start_approval(&args);
            authorize_codex_operation(&session, "codex_task_start", detail, metadata).await?;
            text_result(serde_json::to_string_pretty(
                &codex_app_server::task_start(&args, &session).await?,
            )?)
        }
        "codex_task_get" => text_result(serde_json::to_string_pretty(
            &codex_app_server::task_get(&args, &session).await?,
        )?),
        "codex_task_control" => {
            let (detail, metadata) = codex_task_control_approval(&args);
            authorize_codex_operation(&session, "codex_task_control", detail, metadata).await?;
            text_result(serde_json::to_string_pretty(
                &codex_app_server::task_control(&args, &session).await?,
            )?)
        }
        "local_agent_run" => local_agent_run(&args, &session, local_agent_executable).await,
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
        "write_file" => write_file(&args, &session).await,
        "apply_patch" => {
            let request = apply_patch::parse_request(&args)?;
            let outcome = apply_patch::apply(&session, request).await?;
            text_result(serde_json::to_string_pretty(&outcome)?)
        }
        "git_add" => git_add(&args, &session).await,
        "git_commit" => git_commit(&args, &session).await,
        "git_fetch" => git_fetch(&args, &session).await,
        "git_pull" => git_pull(&args, &session).await,
        "git_push" => git_push(&args, &session).await,
        "execute" => execute(&args, &session).await,
        "start_command" => start_command(&args, &session).await,
        "poll_job" => poll_job(&args, &session).await,
        "job_list" => job_list(&args, &session),
        "checkpoint_save" => {
            let request = checkpoints::parse_save_request(&args)?;
            anyhow::ensure!(request.session_id == session.id, "session ID mismatch");
            let approval_detail = checkpoints::approval_detail(&request.checkpoint);
            let approved = approvals::request(
                &session.id,
                "checkpoint_save",
                approval_detail,
                session.cwd.clone(),
            )
            .await?;
            let store = checkpoints::Store::default_store()?;
            let (saved, activity_detail) =
                save_checkpoint_after_approval(&session, request, &store, approved)?;
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
            let approved = if session.yolo {
                true
            } else {
                approvals::request(
                    &session.id,
                    "recall_feedback",
                    "signal: no_hit; query/content: not persisted".to_owned(),
                    session.cwd.clone(),
                )
                .await?
            };
            anyhow::ensure!(approved, "user denied recall feedback persistence");
            let event = friction::record_client_reported_recall_miss(&session, retry_group)?;
            text_result(serde_json::to_string_pretty(&event)?)
        }
        "stop_job" => stop_job(&args, &session).await,
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
            onepassword_mcp::call_tool(&session, tool_name, arguments).await
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
            if !approvals::request(
                &session.id,
                "onepassword_item_get",
                request.approval_summary(),
                session.cwd.clone(),
            )
            .await?
            {
                anyhow::bail!("user denied 1Password item read")
            }
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
            if !approvals::request(
                &session.id,
                "onepassword_secret_resolve",
                request.approval_summary(),
                session.cwd.clone(),
            )
            .await?
            {
                anyhow::bail!("user denied 1Password secret resolution")
            }
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
                    approvals::activity(&session.id, "1Password secret resolution failed", None)
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
            if !approvals::request(
                &session.id,
                "onepassword_service_account_run",
                detail,
                cwd.clone(),
            )
            .await?
            {
                anyhow::bail!("user denied 1Password service-account command")
            }
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
            if !approvals::request(
                &session.id,
                "kintone_mcp_call",
                safe_child_call_summary(tool_name, &arguments),
                session.cwd.clone(),
            )
            .await?
            {
                anyhow::bail!("user denied kintone MCP tool call")
            }
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
            if !approvals::request(
                &session.id,
                "kintone_cli_run",
                safe_kintone_cli_summary(&arguments, stdout_path.as_deref()),
                cwd.clone(),
            )
            .await?
            {
                anyhow::bail!("user denied cli-kintone command")
            }
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
        "without_sandbox" => without_sandbox(&args, &session).await,
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

async fn session_list() -> Result<Value> {
    let views = crate::session_control::session_views_for_mcp().await?;
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
) -> Result<()> {
    if session.yolo {
        return Ok(());
    }
    let approved = approvals::request_with_metadata(
        &session.id,
        action,
        detail,
        session.cwd.clone(),
        metadata,
    )
    .await?;
    anyhow::ensure!(approved, "user denied Codex operation");
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

async fn write_file(args: &Value, session: &config::Session) -> Result<Value> {
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
    let result = if session.yolo {
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

async fn git_add(args: &Value, session: &config::Session) -> Result<Value> {
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
    run_git_and_report(session, cwd, command, "Stage files").await
}

async fn git_commit(args: &Value, session: &config::Session) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .context("missing message")?;
    anyhow::ensure!(!message.trim().is_empty(), "message must not be empty");
    anyhow::ensure!(
        message.len() <= MAX_GIT_COMMIT_MESSAGE_BYTES,
        "message must be at most {MAX_GIT_COMMIT_MESSAGE_BYTES} bytes"
    );
    ensure_staged_paths_are_permitted(session, &cwd).await?;

    let command = vec![
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
    ];
    run_git_and_report(session, cwd, command, "Create Git commit").await
}

async fn git_fetch(args: &Value, session: &config::Session) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let remote = optional_git_remote(args)?.unwrap_or_else(|| "origin".to_owned());
    ensure_configured_git_remote(session, &cwd, &remote).await?;
    let command = vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "-c".to_owned(),
        "fetch.recurseSubmodules=false".to_owned(),
        "fetch".to_owned(),
        "--prune".to_owned(),
        remote,
    ];
    run_approved_git_command(session, cwd, command, "git_fetch").await
}

async fn git_pull(args: &Value, session: &config::Session) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let command = vec![
        "git".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "-c".to_owned(),
        "fetch.recurseSubmodules=false".to_owned(),
        "pull".to_owned(),
        "--ff-only".to_owned(),
        "--recurse-submodules=no".to_owned(),
    ];
    run_approved_git_command(session, cwd, command, "git_pull").await
}

async fn git_push(args: &Value, session: &config::Session) -> Result<Value> {
    let cwd = cwd(args, session)?;
    let remote = optional_git_remote(args)?;
    let set_upstream = args
        .get("set_upstream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let selected_remote = if set_upstream {
        Some(remote.clone().unwrap_or_else(|| "origin".to_owned()))
    } else {
        remote.clone()
    };
    if let Some(remote) = &selected_remote {
        ensure_configured_git_remote(session, &cwd, remote).await?;
    }
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
        command.push(selected_remote.expect("set_upstream selects a remote"));
        command.push("HEAD".to_owned());
    } else if let Some(remote) = remote {
        command.push(remote);
        command.push("HEAD".to_owned());
    }
    run_approved_git_command(session, cwd, command, "git_push").await
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
    let output = sandbox::run(
        &[
            "git".to_owned(),
            "remote".to_owned(),
            "get-url".to_owned(),
            remote.to_owned(),
        ],
        cwd,
        &session.permitted_directories,
        None,
    )
    .await?;
    anyhow::ensure!(
        output.status == 0,
        "Git remote {remote:?} is not configured: {}",
        output.stderr.trim()
    );
    Ok(())
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
) -> Result<Value> {
    let repository_root = sandbox::git_worktree_root(&cwd)?;
    config::ensure_permitted(session, &repository_root)
        .context("Git repository root must be inside a permitted session root")?;
    if !approvals::request(
        &session.id,
        operation,
        format!("argv: {command:?}"),
        repository_root.clone(),
    )
    .await?
    {
        anyhow::bail!("user denied {operation}")
    }
    let rendered_command = render_command(&command);
    approvals::activity(&session.id, format!("Running {rendered_command}"), None).await;
    let output = sandbox::run_unrestricted_with_env(
        &command,
        &repository_root,
        None,
        &HashMap::new(),
        child_env::SENSITIVE_ENV_NAMES,
    )
    .await;
    let result = output.and_then(render_output);
    report_command_finished(session.id.clone(), "git", &rendered_command, &result).await;
    text_result(result?)
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

fn validate_git_path(session: &config::Session, path: &str) -> Result<PathBuf> {
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

async fn ensure_staged_paths_are_permitted(session: &config::Session, cwd: &Path) -> Result<()> {
    let output = sandbox::run(
        &[
            "git".to_owned(),
            "diff".to_owned(),
            "--cached".to_owned(),
            "--name-only".to_owned(),
            "-z".to_owned(),
            "--no-renames".to_owned(),
        ],
        cwd,
        &session.permitted_directories,
        None,
    )
    .await?;
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
) -> Result<Value> {
    let rendered_command = render_command(&command);
    approvals::activity(&session.id, title, Some(rendered_command.clone())).await;
    let output = if session.yolo {
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

async fn execute(args: &Value, session: &config::Session) -> Result<Value> {
    let output_policy = parse_output_policy(args)?;
    let (rendered_command, mut handle, completion) = spawn_sandboxed_command(args, session).await?;

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

async fn start_command(args: &Value, session: &config::Session) -> Result<Value> {
    let output_policy = parse_output_policy(args)?;
    let (rendered_command, handle, completion) = spawn_sandboxed_command(args, session).await?;
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

async fn local_agent_run(
    args: &Value,
    session: &config::Session,
    executable: Option<&Path>,
) -> Result<Value> {
    let prepared = match executable {
        Some(executable) => local_agent::prepare_with_executable(args, session, executable)?,
        None => local_agent::prepare(args, session)?,
    };
    let detail = prepared.approval_detail();
    approvals::ensure_approval_detail_fits(&detail)?;
    let approved = approvals::request_user_approval_for_instance(
        session,
        "local_agent_run",
        detail,
        prepared.cwd.clone(),
        prepared.approval_metadata(),
    )
    .await?;
    anyhow::ensure!(approved, "user denied local_agent_run");

    let current_session = config::load_session(&session.id).await?;
    anyhow::ensure!(
        current_session.started_at == session.started_at
            && current_session.process_id == session.process_id,
        "session instance changed while local agent approval was pending"
    );
    match executable {
        Some(executable) => prepared.revalidate_with_executable(&current_session, executable)?,
        None => prepared.revalidate(&current_session)?,
    }
    let (description, mut handle, completion) =
        spawn_local_agent(prepared, &current_session).await?;
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
) -> Result<(String, JoinHandle<()>, Arc<Mutex<JobCompletion>>)> {
    let slot = reserve_job_slot(&session.id)?;
    let description = prepared.activity_label();
    approvals::activity(&session.id, format!("Running {description}"), None).await;
    let session_id = session.id.clone();
    let evidence_scope = session.cwd.clone();
    let activity_label = description.clone();
    let completion = Arc::new(Mutex::new(JobCompletion::default()));
    let task_completion = Arc::clone(&completion);
    let handle = tokio::spawn(async move {
        let result = tokio::select! {
            result = local_agent::run(prepared) => {
                result.and_then(render_output)
            }
            _ = wait_for_session_stop(session_id.clone()) => {
                Err(anyhow::anyhow!("session stopped; local agent job cancelled"))
            }
            _ = tokio::time::sleep(MAX_JOB_LIFETIME) => {
                Err(anyhow::anyhow!("local agent job exceeded the two-hour lifetime limit"))
            }
        };
        let cached = cache_job_result(&result, &session_id, &evidence_scope);
        {
            let mut completion = task_completion.lock().unwrap();
            completion.result = Some(cached);
            completion.completed_at = Some(Instant::now());
        }
        drop(slot);
        reap_jobs();
        report_local_agent_finished(session_id, activity_label, &result).await;
    });
    Ok((description, handle, completion))
}

async fn spawn_sandboxed_command(
    args: &Value,
    session: &config::Session,
) -> Result<(String, JoinHandle<()>, Arc<Mutex<JobCompletion>>)> {
    let command = required_command(args)?;
    let cwd = cwd(args, session)?;
    let roots = session.permitted_directories.clone();
    let yolo = session.yolo;
    let slot = reserve_job_slot(&session.id)?;
    let rendered_command = render_command(&command);
    approvals::activity(&session.id, format!("Running {rendered_command}"), None).await;
    let session_id = session.id.clone();
    let evidence_scope = session.cwd.clone();
    let task_command = rendered_command.clone();
    let completion = Arc::new(Mutex::new(JobCompletion::default()));
    let task_completion = Arc::clone(&completion);
    let handle = tokio::spawn(async move {
        let result = tokio::select! {
            result = run_session_command(&command, &cwd, &roots, yolo) => {
                result.and_then(render_output)
            }
            _ = wait_for_session_stop(session_id.clone()) => {
                Err(anyhow::anyhow!("session stopped; sandbox job cancelled"))
            }
            _ = tokio::time::sleep(MAX_JOB_LIFETIME) => {
                Err(anyhow::anyhow!("sandbox job exceeded the two-hour lifetime limit"))
            }
        };
        let cached = cache_job_result(&result, &session_id, &evidence_scope);
        {
            let mut completion = task_completion.lock().unwrap();
            completion.result = Some(cached);
            completion.completed_at = Some(Instant::now());
        }
        drop(slot);
        reap_jobs();
        report_command_finished(session_id, "execute", &task_command, &result).await;
    });
    Ok((rendered_command, handle, completion))
}

async fn run_session_command(
    command: &[String],
    cwd: &Path,
    roots: &[PathBuf],
    yolo: bool,
) -> Result<sandbox::Output> {
    if yolo {
        sandbox::run_unrestricted(command, cwd, None).await
    } else {
        sandbox::run(command, cwd, roots, None).await
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

fn reserve_job_slot(session_id: &str) -> Result<JobSlot> {
    let mut state = jobs().lock().unwrap();
    let active = state
        .active_by_session
        .entry(session_id.to_owned())
        .or_default();
    anyhow::ensure!(
        *active < MAX_ACTIVE_JOBS_PER_SESSION,
        "session {session_id} already has {MAX_ACTIVE_JOBS_PER_SESSION} active sandbox jobs"
    );
    *active += 1;
    Ok(JobSlot {
        session_id: session_id.to_owned(),
    })
}

fn release_job_slot(session_id: &str) {
    let mut state = jobs().lock().unwrap();
    if let Some(active) = state.active_by_session.get_mut(session_id) {
        *active = active.saturating_sub(1);
        if *active == 0 {
            state.active_by_session.remove(session_id);
        }
    }
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

async fn stop_job(args: &Value, session: &config::Session) -> Result<Value> {
    let job_id = required_job_id(args)?;
    let job = take_job_for_session(job_id, &session.id)?;
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

async fn without_sandbox(args: &Value, session: &config::Session) -> Result<Value> {
    let command = required_command(args)?;
    let cwd = cwd(args, session)?;
    if !approvals::request(
        &session.id,
        "without_sandbox",
        format!("argv: {command:?}"),
        cwd.clone(),
    )
    .await?
    {
        anyhow::bail!("user denied without_sandbox")
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

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
    fn public_tools_have_chatgpt_display_metadata() {
        let tools = tools(true, true).as_array().unwrap().to_owned();
        assert_eq!(tools.len(), 45);
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
            yolo: false,
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
            yolo: false,
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
                yolo: true,
            };
            let other = config::Session {
                id: other_id,
                cwd,
                permitted_directories: Vec::new(),
                started_at: 0,
                process_id: 0,
                yolo: true,
            };
            let job_id = Uuid::new_v4();
            let completion = Arc::new(Mutex::new(JobCompletion {
                result: Some(cached_success("owned")),
                completed_at: Some(Instant::now()),
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
                yolo: true,
            };
            let other = config::Session {
                id: other_id,
                cwd,
                permitted_directories: Vec::new(),
                started_at: 0,
                process_id: 0,
                yolo: true,
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
                yolo: true,
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
                yolo: true,
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
            yolo: false,
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
            yolo: true,
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
            true,
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
            yolo: true,
        };
        let job_id = Uuid::new_v4();
        let completion = Arc::new(Mutex::new(JobCompletion {
            result: Some(cached_success("cached-result")),
            completed_at: Some(Instant::now()),
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
            yolo: true,
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
            yolo: true,
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
}
