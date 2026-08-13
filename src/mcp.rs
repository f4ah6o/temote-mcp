use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{approvals, config, sandbox};

const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ACTIVE_JOBS_PER_SESSION: usize = 8;
const MAX_JOB_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);
const COMPLETED_JOB_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_GIT_ADD_PATHS: usize = 256;
const MAX_GIT_COMMIT_MESSAGE_BYTES: usize = 16 * 1024;
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

#[derive(Clone)]
enum CachedJobResult {
    Success(String),
    Error(String),
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
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
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

async fn write_message(stdout: &mut tokio::io::Stdout, message: &Value) -> Result<()> {
    stdout
        .write_all(serde_json::to_string(message)?.as_bytes())
        .await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

pub(crate) async fn dispatch(request: &Value) -> Result<Value> {
    dispatch_with_mode(request, false).await
}

#[cfg(feature = "network")]
pub(crate) async fn dispatch_public(request: &Value) -> Result<Value> {
    dispatch_with_mode(request, true).await
}

async fn dispatch_with_mode(request: &Value, public: bool) -> Result<Value> {
    match request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "initialize" => Ok(json!({
            "protocolVersion": negotiate_protocol_version(request),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "local-mcp", "title": "Local MCP", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Every tool call requires the local-mcp session_id supplied by the user, except session_list. Call session_list to discover active sessions, then session_info to inspect a session's working directory, mode, and sandbox roots. Normal sessions keep file paths scoped, execute commands sandboxed, and remote host operations approval-gated. A session started with `local-mcp start <session-id> --yolo` runs with the host permissions of the local-mcp user and skips local-mcp approval prompts. The session mode does not control confirmation or authorization enforced by the MCP client."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools(public)})),
        "tools/call" => call_tool(request.get("params").unwrap_or(&Value::Null), public).await,
        method => anyhow::bail!("method not found: {method}"),
    }
}

fn negotiate_protocol_version(request: &Value) -> &'static str {
    let requested = request
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str);
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .find(|version| Some(*version) == requested)
        .unwrap_or(LATEST_PROTOCOL_VERSION)
}

fn tools(public: bool) -> Value {
    let mut tools = json!([
        {"name":"session_list","title":"List local MCP sessions","description":"List currently active local-mcp sessions. Returns session IDs, working directories, start times, status, and whether each session is in yolo mode.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
        {"name":"session_info","title":"Inspect a local MCP session","description":"Show a local-mcp session's ID, working directory, allowed sandbox roots, and yolo mode state.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"read_file","title":"Read a local file","description":"Read a UTF-8 file from the local machine. Relative paths use the session working directory.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"get_image","title":"Read a local image","description":"Read a local image and return it as MCP image content. Relative paths use the session working directory.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string","description":"Path to a PNG, JPEG, GIF, WebP, BMP, TIFF, or AVIF image."}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"list_directory","title":"List a local directory","description":"List entries in a local directory. Relative paths use the session working directory.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"write_file","title":"Write a local file","description":"Write a UTF-8 file using the selected session permission mode. Normal sessions are restricted to permitted roots and use the Codex sandbox; yolo sessions may write anywhere the local user can.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"},"content":{"type":"string"}},"required":["session_id","path","content"],"additionalProperties":false}},
        {"name":"git_add","title":"Stage files with Git","description":"Stage existing files or directories in the session repository with git add. Only the specified paths are staged; Git hooks and network access are unavailable.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":256},"cwd":{"type":"string"}},"required":["session_id","paths"],"additionalProperties":false}},
        {"name":"git_commit","title":"Create a local Git commit","description":"Create a local commit from the current Git index. This does not push, hooks and signing are disabled, and network access is unavailable.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"message":{"type":"string","minLength":1,"maxLength":16384},"cwd":{"type":"string"}},"required":["session_id","message"],"additionalProperties":false}},
        {"name":"git_fetch","title":"Fetch Git remote updates","description":"Run git fetch --prune for a configured remote on the host. The remote must be a safe configured name and arbitrary URLs and refspecs are not accepted. local-mcp requests local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"remote":{"type":"string","default":"origin"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"git_pull","title":"Fast-forward Git branch","description":"Run git pull --ff-only for the current branch and its configured upstream on the host. Hooks are disabled. local-mcp requests local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"git_push","title":"Push current Git branch","description":"Push the current branch on the host without force options. Optionally set origin (or another safe configured remote) as the upstream. Hooks are disabled. local-mcp requests local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"cwd":{"type":"string"},"remote":{"type":"string"},"set_upstream":{"type":"boolean","default":false}},"required":["session_id"],"additionalProperties":false}},
        {"name":"execute","title":"Run a command","description":"Execute argv without a shell using the selected session permission mode. Normal sessions run in the Codex sandbox with network disabled; yolo sessions run directly on the host with the local user's filesystem, environment, process, and network permissions. Returns the normal result when it finishes within 30 seconds; otherwise returns a job_id.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"],"additionalProperties":false}},
        {"name":"start_command","title":"Start a command","description":"Start argv immediately as a background job using the selected session permission mode. Normal sessions use the Codex sandbox with network disabled; yolo sessions run directly on the host with the local user's permissions.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"],"additionalProperties":false}},
        {"name":"poll_job","title":"Poll a sandbox job","description":"Poll a background command returned by execute or start_command. Returns running while active, or the command result once completed.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"job_id":{"type":"string"}},"required":["session_id","job_id"],"additionalProperties":false}},
        {"name":"stop_job","title":"Stop a sandbox job","description":"Stop a background command returned by execute or start_command.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"job_id":{"type":"string"}},"required":["session_id","job_id"],"additionalProperties":false}},
        {"name":"without_sandbox","title":"Run a host command","description":"Execute argv directly on the host with the local user's permissions and network access. local-mcp requests local approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"],"additionalProperties":false}}
    ]);
    if public {
        tools
            .as_array_mut()
            .unwrap()
            .retain(|tool| tool["name"] != "without_sandbox");
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

async fn call_tool(params: &Value, public: bool) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("missing tool name")?;
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
    anyhow::ensure!(
        !public || name != "without_sandbox",
        "without_sandbox is unavailable on the public MCP endpoint"
    );
    let session_id = required_session_id(&args)?;
    let session = config::load_session(&session_id).await?;
    match name {
        "session_info" => {
            approvals::activity(&session.id, "Read session info", None).await;
            text_result(serde_json::to_string_pretty(&session)?)
        }
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
        "read_file" => {
            let path = config::resolve_existing_path(&session, &required_path(&args, "path")?)?;
            let result = tokio::fs::read_to_string(&path)
                .await
                .context("failed to read file");
            report_result(
                &session.id,
                format!("Read {}", display_path(&path, &session.cwd)),
                &result,
            )
            .await;
            text_result(result?)
        }
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
        "git_add" => git_add(&args, &session).await,
        "git_commit" => git_commit(&args, &session).await,
        "git_fetch" => git_fetch(&args, &session).await,
        "git_pull" => git_pull(&args, &session).await,
        "git_push" => git_push(&args, &session).await,
        "execute" => execute(&args, &session).await,
        "start_command" => start_command(&args, &session).await,
        "poll_job" => poll_job(&args, &session).await,
        "stop_job" => stop_job(&args, &session).await,
        "without_sandbox" => without_sandbox(&args, &session).await,
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

async fn session_list() -> Result<Value> {
    let directory = config::sessions_dir()?;
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return text_result("[]".to_owned());
        }
        Err(error) => return Err(error).context("failed to read session metadata"),
    };
    let mut sessions = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let session: config::Session = match serde_json::from_slice(&bytes) {
            Ok(session) => session,
            Err(_) => continue,
        };
        if !config::session_is_active(&session.id)
            .await
            .unwrap_or(false)
        {
            continue;
        }
        sessions.push(json!({
            "session_id": session.id,
            "cwd": session.cwd,
            "started_at": session.started_at,
            "status": "active",
            "yolo": session.yolo,
        }));
    }
    sessions.sort_by_key(|session| {
        session["session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    });
    text_result(serde_json::to_string_pretty(&sessions)?)
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

fn text_result(text: String) -> Result<Value> {
    Ok(json!({"content":[{"type":"text","text":text}]}))
}

async fn get_image(path: &Path) -> Result<Value> {
    let path = tokio::fs::canonicalize(&path)
        .await
        .with_context(|| format!("cannot resolve image {}", path.display()))?;
    let metadata = tokio::fs::metadata(&path).await?;
    anyhow::ensure!(
        metadata.is_file(),
        "image path is not a file: {}",
        path.display()
    );
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("cannot read image {}", path.display()))?;
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

fn required_path(args: &Value, name: &str) -> Result<PathBuf> {
    args.get(name)
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context(format!("missing {name}"))
}

fn required_session_id(args: &Value) -> Result<String> {
    let value = args
        .get("session_id")
        .and_then(Value::as_str)
        .context("missing session_id; ask the user to run `local-mcp start` and provide its ID")?;
    config::validate_session_id(value)?;
    Ok(value.to_owned())
}

fn cwd(args: &Value, session: &config::Session) -> Result<PathBuf> {
    let path = args.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    config::resolve_cwd(session, path.as_deref())
}

async fn list_directory(path: &Path) -> Result<String> {
    let mut entries = tokio::fs::read_dir(path).await?;
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let suffix = if entry.file_type().await?.is_dir() {
            "/"
        } else {
            ""
        };
        names.push(format!("{}{}", entry.file_name().to_string_lossy(), suffix));
    }
    names.sort();
    Ok(names.join("\n"))
}

async fn write_file(args: &Value, session: &config::Session) -> Result<Value> {
    let absolute = config::resolve_write_path(session, &required_path(args, "path")?)?;
    let parent = absolute.parent().context("file has no parent directory")?;
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("parent does not exist: {}", parent.display()))?;
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .context("missing content")?;
    let previous = tokio::fs::read_to_string(&absolute)
        .await
        .unwrap_or_default();
    let command = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "cat > \"$1\"".to_owned(),
        "local-mcp-write".to_owned(),
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
    run_and_report(session.id.clone(), command, repository_root, true, &[]).await
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
    report_command_finished(session.id.clone(), &rendered_command, &result).await;
    text_result(result?)
}

async fn execute(args: &Value, session: &config::Session) -> Result<Value> {
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
            cached_job_result(result)
        }
        Err(_) => {
            store_job(
                session,
                rendered_command,
                handle,
                completion,
                "Backgrounded",
            )
            .await
        }
    }
}

async fn start_command(args: &Value, session: &config::Session) -> Result<Value> {
    let (rendered_command, handle, completion) = spawn_sandboxed_command(args, session).await?;
    store_job(session, rendered_command, handle, completion, "Started").await
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
        let cached = cache_job_result(&result);
        {
            let mut completion = task_completion.lock().unwrap();
            completion.result = Some(cached);
            completion.completed_at = Some(Instant::now());
        }
        drop(slot);
        report_command_finished(session_id, &task_command, &result).await;
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
    activity: &str,
) -> Result<Value> {
    let job_id = Uuid::new_v4();
    jobs().lock().unwrap().jobs.insert(
        job_id,
        Job {
            session_id: session.id.clone(),
            command: rendered_command.clone(),
            handle,
            completion,
        },
    );
    approvals::activity(
        &session.id,
        format!("{activity} {rendered_command}"),
        Some(format!("└ job {job_id}")),
    )
    .await;
    text_result(json!({"status":"running","job_id":job_id}).to_string())
}

fn cache_job_result(result: &Result<String>) -> CachedJobResult {
    match result {
        Ok(text) => CachedJobResult::Success(text.clone()),
        Err(error) => CachedJobResult::Error(format!("{error:#}")),
    }
}

fn cached_job_result(result: CachedJobResult) -> Result<Value> {
    match result {
        CachedJobResult::Success(text) => text_result(text),
        CachedJobResult::Error(error) => anyhow::bail!(error),
    }
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

fn remove_job(job_id: Uuid) -> Option<Job> {
    jobs().lock().unwrap().jobs.remove(&job_id)
}

fn reap_jobs() {
    let now = Instant::now();
    let expired = {
        let state = jobs().lock().unwrap();
        state
            .jobs
            .iter()
            .filter_map(|(job_id, job)| {
                let completed_at = job.completion.lock().unwrap().completed_at?;
                (now.saturating_duration_since(completed_at) >= COMPLETED_JOB_TTL)
                    .then_some(*job_id)
            })
            .collect::<Vec<_>>()
    };
    if expired.is_empty() {
        return;
    }
    let mut state = jobs().lock().unwrap();
    for job_id in expired {
        state.jobs.remove(&job_id);
    }
}

async fn wait_for_session_stop(session_id: String) {
    loop {
        if !config::session_is_active(&session_id)
            .await
            .unwrap_or(false)
        {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn poll_job(args: &Value, session: &config::Session) -> Result<Value> {
    let job_id = required_job_id(args)?;
    let result = {
        let state = jobs().lock().unwrap();
        let job = state.jobs.get(&job_id).context("unknown job_id")?;
        anyhow::ensure!(
            job.session_id == session.id,
            "job does not belong to this session"
        );
        job.completion.lock().unwrap().result.clone()
    };
    if let Some(result) = result {
        return cached_job_result(result);
    }

    let task_finished_without_result = {
        let state = jobs().lock().unwrap();
        let job = state.jobs.get(&job_id).context("unknown job_id")?;
        job.handle.is_finished()
    };
    if task_finished_without_result {
        anyhow::bail!("background command task finished without a cached result")
    }
    text_result(json!({"status":"running","job_id":job_id}).to_string())
}

async fn stop_job(args: &Value, session: &config::Session) -> Result<Value> {
    let job_id = required_job_id(args)?;
    let job = {
        let state = jobs().lock().unwrap();
        let job = state.jobs.get(&job_id).context("unknown job_id")?;
        anyhow::ensure!(
            job.session_id == session.id,
            "job does not belong to this session"
        );
        drop(state);
        remove_job(job_id).context("unknown job_id")?
    };
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
    args.get("command")
        .and_then(Value::as_array)
        .context("missing command")?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .context("command entries must be strings")
        })
        .collect()
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
    report_command_finished(session_id, &rendered_command, &result).await;
    text_result(result?)
}

fn render_command(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| shell_word(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn report_command_finished(session_id: String, command: &str, result: &Result<String>) {
    let detail = match result {
        Ok(text) => command_summary(text),
        Err(error) => Some(format!("└ Error: {error:#}")),
    };
    approvals::activity(&session_id, format!("Ran {command}"), detail).await;
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
    let diff = TextDiff::from_lines(old, new);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    let rendered = diff.unified_diff().context_radius(3).to_string();
    (added, removed, rendered.trim_end().to_owned())
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

    #[test]
    fn detects_supported_image_types() {
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(image_mime_type(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(image_mime_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(image_mime_type(b"RIFF\0\0\0\0WEBP"), Some("image/webp"));
        assert_eq!(image_mime_type(b"not an image"), None);
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
        assert_eq!(result["protocolVersion"], LATEST_PROTOCOL_VERSION);
    }

    #[test]
    fn public_tools_have_chatgpt_display_metadata() {
        let tools = tools(true).as_array().unwrap().to_owned();
        assert_eq!(tools.len(), 15);
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
        assert!(tools.iter().any(|tool| tool["name"] == "git_add"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_commit"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_fetch"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_pull"));
        assert!(tools.iter().any(|tool| tool["name"] == "git_push"));
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
        let path = std::env::temp_dir().join(format!("local-mcp-{}.png", uuid::Uuid::new_v4()));
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
        let directory = std::env::temp_dir().join(format!("local-mcp-{}", uuid::Uuid::new_v4()));
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
            result: Some(CachedJobResult::Success("cached-result".to_owned())),
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
            },
        );
        let args = json!({"job_id": job_id.to_string()});

        let first = poll_job(&args, &session).await.unwrap();
        let second = poll_job(&args, &session).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(first["content"][0]["text"], "cached-result");
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

    #[tokio::test]
    async fn completed_job_cache_is_reaped_only_after_ttl() {
        let session_id = format!("test-job-ttl-{}", Uuid::new_v4());
        let job_id = Uuid::new_v4();
        let completion = Arc::new(Mutex::new(JobCompletion {
            result: Some(CachedJobResult::Success("expired".to_owned())),
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
            },
        );

        reap_jobs();

        assert!(!jobs().lock().unwrap().jobs.contains_key(&job_id));
    }
}
