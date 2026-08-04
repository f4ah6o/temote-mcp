use std::collections::VecDeque;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use uuid::Uuid;

use crate::config::{self, Session};

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub id: Uuid,
    pub operation: String,
    pub detail: String,
    pub cwd: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Probe,
    Approval {
        request: Request,
    },
    Activity {
        title: String,
        detail: Option<String>,
    },
}

pub async fn request(
    session_id: &str,
    operation: &str,
    detail: String,
    cwd: PathBuf,
) -> Result<bool> {
    let request = Request {
        id: Uuid::new_v4(),
        operation: operation.to_owned(),
        detail,
        cwd,
    };
    let path = config::socket_path(session_id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `local-mcp start`"))?;
    stream
        .write_all(&serde_json::to_vec(&Message::Approval { request })?)
        .await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    match response.trim() {
        "allow" => Ok(true),
        "deny" => Ok(false),
        value => anyhow::bail!("invalid response from session: {value:?}"),
    }
}

/// Sends a one-way activity update to the `start` screen. Activity reporting is
/// deliberately best-effort: an MCP operation must not fail just because its UI
/// was closed between loading the session and completing the operation.
pub async fn activity(session_id: &str, title: impl Into<String>, detail: Option<String>) {
    let Ok(path) = config::socket_path(session_id) else {
        return;
    };
    let Ok(mut stream) = UnixStream::connect(path).await else {
        return;
    };
    let message = Message::Activity {
        title: title.into(),
        detail,
    };
    let Ok(bytes) = serde_json::to_vec(&message) else {
        return;
    };
    let _ = stream.write_all(&bytes).await;
    let _ = stream.write_all(b"\n").await;
    let _ = stream.shutdown().await;
}

pub async fn start(session_id: Option<&str>) -> Result<()> {
    let id = config::session_id(session_id)?;
    config::remove_inactive_socket(&id).await?;
    let mut session = config::new_session(&std::env::current_dir()?, Some(&id))?;
    let path = config::socket_path(&session.id)?;
    let state_dir = path.parent().context("session socket has no parent")?;
    tokio::fs::create_dir_all(state_dir).await?;
    tokio::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).await?;

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to listen at {}", path.display()))?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    session.process_id = std::process::id();
    if let Err(error) = config::save_session(&session).await {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    eprintln!(
        "local-mcp session: {}\ncwd: {}\n\
         Give this session ID to the agent so it can include it in local-mcp tool calls.\n\
         Commands: /permission ask|yolo|allow <directory>|revoke <directory>|list|status\n\
         Press Ctrl-C to stop.",
        session.id,
        session.cwd.display()
    );

    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut pending = VecDeque::<(Request, UnixStream)>::new();
    let mut yolo = false;
    let result = run_session(&listener, &mut input, &mut session, &mut pending, &mut yolo).await;
    session.process_id = 0;
    if let Err(error) = config::save_session(&session).await {
        eprintln!("failed to mark session stopped: {error:#}");
    }
    let _ = tokio::fs::remove_file(&path).await;
    result
}

async fn run_session(
    listener: &UnixListener,
    input: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
    session: &mut Session,
    pending: &mut VecDeque<(Request, UnixStream)>,
    yolo: &mut bool,
) -> Result<()> {
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let (mut stream, _) = connection?;
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).await?;
                let message: Message = serde_json::from_str(&line).context("invalid session message")?;
                match message {
                    Message::Probe => {
                        stream.write_all(b"active\n").await?;
                    }
                    Message::Activity { title, detail } => show_activity(&title, detail.as_deref()),
                    Message::Approval { request } if *yolo => {
                        eprintln!("[yolo] allowing {}: {}", request.operation, request.detail);
                        stream.write_all(b"allow\n").await?;
                    }
                    Message::Approval { request } => {
                        show_request(&request)?;
                        pending.push_back((request, stream));
                    }
                }
            }
            line = input.next_line() => {
                let Some(line) = line? else { anyhow::bail!("session input closed") };
                handle_input(line.trim(), session, yolo, pending).await?;
            }
            signal = &mut ctrl_c => {
                signal.context("failed to receive Ctrl-C")?;
                eprintln!("Stopping local-mcp session {}", session.id);
                return Ok(());
            }
        }
    }
}

fn show_activity(title: &str, detail: Option<&str>) {
    eprintln!("\n• {title}");
    if let Some(detail) = detail.filter(|value| !value.is_empty()) {
        for line in detail.lines() {
            eprintln!("  {line}");
        }
    }
}

async fn handle_input(
    input: &str,
    session: &mut Session,
    yolo: &mut bool,
    pending: &mut VecDeque<(Request, UnixStream)>,
) -> Result<()> {
    match input {
        "/permissions yolo" | "/permission yolo" => {
            *yolo = true;
            eprintln!("Permissions: yolo (all unsandboxed calls are allowed for this session)");
            while let Some((request, mut stream)) = pending.pop_front() {
                eprintln!("[yolo] allowing {}: {}", request.operation, request.detail);
                stream.write_all(b"allow\n").await?;
            }
        }
        "/permissions ask" | "/permission ask" => {
            *yolo = false;
            eprintln!("Permissions: ask");
        }
        "y" | "Y" | "yes" | "YES" if !pending.is_empty() => {
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"allow\n").await?;
            show_next(pending)?;
        }
        "n" | "N" | "no" | "NO" if !pending.is_empty() => {
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"deny\n").await?;
            show_next(pending)?;
        }
        "/permission list" | "/permissions list" => show_permissions(session),
        "/permission status" | "/permissions status" => {
            eprintln!("Permissions: {}", if *yolo { "yolo" } else { "ask" });
            show_permissions(session);
        }
        command if permission_arg(command, "allow").is_some() => {
            let directory = config::canonical_directory(
                PathBuf::from(permission_arg(command, "allow").unwrap()).as_path(),
            )?;
            if !session.permitted_directories.contains(&directory) {
                session.permitted_directories.push(directory.clone());
                session.permitted_directories.sort();
                config::save_session(session).await?;
            }
            eprintln!("Allowed sandbox root: {}", directory.display());
        }
        command if permission_arg(command, "revoke").is_some() => {
            let directory = config::canonical_directory(
                PathBuf::from(permission_arg(command, "revoke").unwrap()).as_path(),
            )?;
            if directory == session.cwd {
                eprintln!("Cannot revoke the session cwd");
            } else {
                session
                    .permitted_directories
                    .retain(|item| item != &directory);
                config::save_session(session).await?;
                eprintln!("Revoked sandbox root: {}", directory.display());
            }
        }
        "/permission" | "/permissions" | "/permission help" | "/permissions help" => {
            eprintln!("/permission ask|yolo|allow <directory>|revoke <directory>|list|status");
        }
        "" => {}
        command if !pending.is_empty() => {
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"deny\n").await?;
            eprintln!("Denied request (unrecognized response: {command})");
            show_next(pending)?;
        }
        command => eprintln!("Unknown command: {command}"),
    }
    Ok(())
}

fn permission_arg<'a>(command: &'a str, action: &str) -> Option<&'a str> {
    ["/permission", "/permissions"]
        .into_iter()
        .find_map(|prefix| {
            command
                .strip_prefix(&format!("{prefix} {action} "))
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
}

fn show_permissions(session: &Session) {
    eprintln!("Sandbox roots:");
    for path in &session.permitted_directories {
        eprintln!("  {}", path.display());
    }
}

fn show_request(request: &Request) -> Result<()> {
    eprintln!(
        "\n[{}] {}\ncwd: {}\n{}",
        request.id,
        request.operation,
        request.cwd.display(),
        request.detail
    );
    eprint!("Allow without sandbox? [y/N] ");
    std::io::stderr().flush()?;
    Ok(())
}

fn show_next(pending: &VecDeque<(Request, UnixStream)>) -> Result<()> {
    if let Some((request, _)) = pending.front() {
        show_request(request)?;
    }
    Ok(())
}
