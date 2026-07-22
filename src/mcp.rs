use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{approvals, config, sandbox};

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

async fn dispatch(request: &Value) -> Result<Value> {
    match request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "local-mcp", "version": env!("CARGO_PKG_VERSION")}
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call_tool(request.get("params").unwrap_or(&Value::Null)).await,
        method => anyhow::bail!("method not found: {method}"),
    }
}

fn tools() -> Value {
    json!([
        {"name":"read_file","description":"Read a UTF-8 file from the local machine.","inputSchema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},
        {"name":"list_directory","description":"List entries in a local directory.","inputSchema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},
        {"name":"write_file","description":"Write a UTF-8 file. Requires approval unless its parent is permitted.","inputSchema":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}},
        {"name":"execute","description":"Execute argv without a shell in the Codex sandbox. Network is disabled. Requires approval unless cwd is permitted.","inputSchema":{"type":"object","properties":{"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["command"]}},
        {"name":"network_request","description":"Perform an HTTP request with curl in the Codex sandbox. Always requires approval.","inputSchema":{"type":"object","properties":{"url":{"type":"string"},"method":{"type":"string","default":"GET"},"body":{"type":"string"},"cwd":{"type":"string"}},"required":["url"]}}
    ])
}

async fn call_tool(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("missing tool name")?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = match name {
        "read_file" => tokio::fs::read_to_string(required_path(&args, "path")?).await?,
        "list_directory" => list_directory(&required_path(&args, "path")?).await?,
        "write_file" => write_file(&args).await?,
        "execute" => execute(&args).await?,
        "network_request" => network_request(&args).await?,
        _ => anyhow::bail!("unknown tool: {name}"),
    };
    Ok(json!({"content":[{"type":"text","text":result}]}))
}

fn required_path(args: &Value, name: &str) -> Result<PathBuf> {
    args.get(name)
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context(format!("missing {name}"))
}

fn cwd(args: &Value) -> Result<PathBuf> {
    let path = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    std::fs::canonicalize(&path).with_context(|| format!("cannot resolve cwd {}", path.display()))
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

async fn write_file(args: &Value) -> Result<String> {
    let path = required_path(args, "path")?;
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute.parent().context("file has no parent directory")?;
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("parent does not exist: {}", parent.display()))?;
    let config = config::load().await?;
    if !config::is_permitted(&parent, &config.permitted_directories)
        && !approvals::request("write_file", absolute.display().to_string(), parent.clone()).await?
    {
        anyhow::bail!("user denied write_file")
    }
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .context("missing content")?;
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
        &[parent.clone()],
        false,
        Some(content.as_bytes()),
    )
    .await?;
    render_output(output)
}

async fn execute(args: &Value) -> Result<String> {
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
    let cwd = cwd(args)?;
    let config = config::load().await?;
    let permitted = config::is_permitted(&cwd, &config.permitted_directories);
    if !permitted
        && !approvals::request("execute", format!("argv: {command:?}"), cwd.clone()).await?
    {
        anyhow::bail!("user denied execute")
    }
    let roots = if permitted {
        config.permitted_directories
    } else {
        vec![cwd.clone()]
    };
    render_output(sandbox::run(&command, &cwd, &roots, false, None).await?)
}

async fn network_request(args: &Value) -> Result<String> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .context("missing url")?;
    let method = args.get("method").and_then(Value::as_str).unwrap_or("GET");
    let cwd = cwd(args)?;
    if !approvals::request("network_request", format!("{method} {url}"), cwd.clone()).await? {
        anyhow::bail!("user denied network_request")
    }
    let mut command = vec![
        "curl".to_owned(),
        "--fail-with-body".to_owned(),
        "--silent".to_owned(),
        "--show-error".to_owned(),
        "--request".to_owned(),
        method.to_owned(),
        url.to_owned(),
    ];
    if let Some(body) = args.get("body").and_then(Value::as_str) {
        command.extend(["--data-binary".to_owned(), body.to_owned()]);
    }
    render_output(sandbox::run(&command, &cwd, &[], true, None).await?)
}

fn render_output(output: sandbox::Output) -> Result<String> {
    let text = json!({"exit_code":output.status,"stdout":output.stdout,"stderr":output.stderr})
        .to_string();
    if output.status == 0 {
        Ok(text)
    } else {
        anyhow::bail!(text)
    }
}
