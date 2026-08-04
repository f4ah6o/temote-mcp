use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{approvals, config, sandbox};

const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ACTIVE_JOBS_PER_SESSION: usize = 4;
const MAX_JOB_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

struct Job {
    session_id: String,
    command: String,
    handle: JoinHandle<Result<String>>,
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
            "instructions": "Every tool call requires the local-mcp session_id supplied by the user, except session_list. Call session_list to discover active sessions, then session_info to inspect a session's working directory and sandbox roots."
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
        {"name":"session_list","title":"List local MCP sessions","description":"List currently active local-mcp sessions. Returns only session IDs, working directories, start times, and status.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
        {"name":"session_info","title":"Inspect a local MCP session","description":"Show a local-mcp session's ID, working directory, and allowed sandbox roots.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"read_file","title":"Read a local file","description":"Read a UTF-8 file from the local machine. Relative paths use the session working directory.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"get_image","title":"Read a local image","description":"Read a local image and return it as MCP image content. Relative paths use the session working directory.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string","description":"Path to a PNG, JPEG, GIF, WebP, BMP, TIFF, or AVIF image."}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"list_directory","title":"List a local directory","description":"List entries in a local directory. Relative paths use the session working directory.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"write_file","title":"Write a local file","description":"Write a UTF-8 file in the Codex sandbox. Relative paths use the session working directory. ChatGPT should confirm before calling this tool.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"},"content":{"type":"string"}},"required":["session_id","path","content"],"additionalProperties":false}},
        {"name":"execute","title":"Run a sandboxed command","description":"Execute argv without a shell in the Codex sandbox. Returns the normal result when it finishes within 30 seconds; otherwise returns a job_id for use with poll_job or stop_job. Network is disabled and approval is not required. ChatGPT should confirm before calling this tool.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"],"additionalProperties":false}},
        {"name":"start_command","title":"Start a sandboxed command","description":"Start argv immediately as a background job in the Codex sandbox and return a job_id without waiting for completion. Network is disabled and approval is not required. ChatGPT should confirm before calling this tool.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"],"additionalProperties":false}},
        {"name":"poll_job","title":"Poll a sandbox job","description":"Poll a background command returned by execute or start_command. Returns running while active, or the command result once completed.","annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"job_id":{"type":"string"}},"required":["session_id","job_id"],"additionalProperties":false}},
        {"name":"stop_job","title":"Stop a sandbox job","description":"Stop a background command returned by execute or start_command. ChatGPT should confirm before calling this tool.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"job_id":{"type":"string"}},"required":["session_id","job_id"],"additionalProperties":false}},
        {"name":"without_sandbox","title":"Run a host command","description":"Execute argv directly on the host with full user permissions and network access. Every call requires approval unless the session is in yolo mode.","annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"],"additionalProperties":false}}
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
    reap_inactive_jobs().await;
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
    let output = sandbox::run(
        &command,
        &parent,
        std::slice::from_ref(&parent),
        Some(content.as_bytes()),
    )
    .await?;
    let result = render_output(output);
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

async fn execute(args: &Value, session: &config::Session) -> Result<Value> {
    let (rendered_command, mut handle) = spawn_sandboxed_command(args, session).await?;

    match tokio::time::timeout(FOREGROUND_TIMEOUT, &mut handle).await {
        Ok(joined) => {
            release_job_slot(&session.id);
            text_result(joined.context("command task failed")??)
        }
        Err(_) => store_job(session, rendered_command, handle, "Backgrounded").await,
    }
}

async fn start_command(args: &Value, session: &config::Session) -> Result<Value> {
    let (rendered_command, handle) = spawn_sandboxed_command(args, session).await?;
    store_job(session, rendered_command, handle, "Started").await
}

async fn spawn_sandboxed_command(
    args: &Value,
    session: &config::Session,
) -> Result<(String, JoinHandle<Result<String>>)> {
    let command = required_command(args)?;
    let cwd = cwd(args, session)?;
    let roots = session.permitted_directories.clone();
    reserve_job_slot(&session.id)?;
    let rendered_command = render_command(&command);
    approvals::activity(&session.id, format!("Running {rendered_command}"), None).await;
    let session_id = session.id.clone();
    let task_command = rendered_command.clone();
    let handle = tokio::spawn(async move {
        let result = tokio::select! {
            result = sandbox::run(&command, &cwd, &roots, None) => {
                result.and_then(render_output)
            }
            _ = wait_for_session_stop(session_id.clone()) => {
                anyhow::bail!("session stopped; sandbox job cancelled")
            }
            _ = tokio::time::sleep(MAX_JOB_LIFETIME) => {
                anyhow::bail!("sandbox job exceeded the two-hour lifetime limit")
            }
        };
        report_command_finished(session_id, &task_command, &result).await;
        result
    });
    Ok((rendered_command, handle))
}

async fn store_job(
    session: &config::Session,
    rendered_command: String,
    handle: JoinHandle<Result<String>>,
    activity: &str,
) -> Result<Value> {
    let job_id = Uuid::new_v4();
    jobs().lock().unwrap().jobs.insert(
        job_id,
        Job {
            session_id: session.id.clone(),
            command: rendered_command.clone(),
            handle,
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

fn reserve_job_slot(session_id: &str) -> Result<()> {
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
    Ok(())
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
    let mut state = jobs().lock().unwrap();
    let job = state.jobs.remove(&job_id)?;
    if let Some(active) = state.active_by_session.get_mut(&job.session_id) {
        *active = active.saturating_sub(1);
        if *active == 0 {
            state.active_by_session.remove(&job.session_id);
        }
    }
    Some(job)
}

async fn reap_inactive_jobs() {
    let session_ids = {
        let state = jobs().lock().unwrap();
        state
            .jobs
            .values()
            .map(|job| job.session_id.clone())
            .collect::<Vec<_>>()
    };
    for session_id in session_ids {
        if config::session_is_active(&session_id)
            .await
            .unwrap_or(false)
        {
            continue;
        }
        let job_ids = {
            let state = jobs().lock().unwrap();
            state
                .jobs
                .iter()
                .filter(|(_, job)| job.session_id == session_id)
                .map(|(job_id, _)| *job_id)
                .collect::<Vec<_>>()
        };
        for job_id in job_ids {
            if let Some(job) = remove_job(job_id) {
                job.handle.abort();
                let _ = job.handle.await;
            }
        }
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
    let finished = {
        let jobs = jobs().lock().unwrap();
        let job = jobs.jobs.get(&job_id).context("unknown job_id")?;
        anyhow::ensure!(
            job.session_id == session.id,
            "job does not belong to this session"
        );
        job.handle.is_finished()
    };
    if !finished {
        return text_result(json!({"status":"running","job_id":job_id}).to_string());
    }

    let job = remove_job(job_id).context("unknown job_id")?;
    let result = job.handle.await.context("background command task failed")?;
    text_result(result?)
}

async fn stop_job(args: &Value, session: &config::Session) -> Result<Value> {
    let job_id = required_job_id(args)?;
    let job = {
        let jobs = jobs().lock().unwrap();
        let job = jobs.jobs.get(&job_id).context("unknown job_id")?;
        anyhow::ensure!(
            job.session_id == session.id,
            "job does not belong to this session"
        );
        drop(jobs);
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
        assert_eq!(tools.len(), 10);
        assert!(tools.iter().all(|tool| {
            tool["name"].is_string()
                && tool["title"].is_string()
                && tool["description"].is_string()
                && tool["inputSchema"].is_object()
                && tool["annotations"].is_object()
        }));
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
}
