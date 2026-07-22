use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use uuid::Uuid;

use crate::config;

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub id: Uuid,
    pub operation: String,
    pub detail: String,
    pub cwd: PathBuf,
}

fn socket_path() -> Result<PathBuf> {
    Ok(config::state_dir()?.join("approvals.sock"))
}

pub async fn request(operation: &str, detail: String, cwd: PathBuf) -> Result<bool> {
    let request = Request {
        id: Uuid::new_v4(),
        operation: operation.to_owned(),
        detail,
        cwd,
    };
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "approval service is unavailable; run `local-mcp approvals` (socket: {})",
            path.display()
        )
    })?;
    stream.write_all(&serde_json::to_vec(&request)?).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    match response.trim() {
        "allow" => Ok(true),
        "deny" => Ok(false),
        value => anyhow::bail!("invalid response from approval service: {value:?}"),
    }
}

pub async fn run_ui() -> Result<()> {
    let path = socket_path()?;
    let state_dir = path.parent().context("approval socket has no parent")?;
    tokio::fs::create_dir_all(state_dir).await?;
    tokio::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).await?;

    if UnixStream::connect(&path).await.is_ok() {
        anyhow::bail!(
            "another approval service is already listening at {}",
            path.display()
        );
    }
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to remove stale approval socket"),
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to listen at {}", path.display()))?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    eprintln!(
        "Waiting for local-mcp approval requests at {}. Press Ctrl-C to stop.",
        path.display()
    );
    let mut input = BufReader::new(tokio::io::stdin()).lines();

    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut line = String::new();
        BufReader::new(&mut stream).read_line(&mut line).await?;
        let request: Request = serde_json::from_str(&line).context("invalid approval request")?;
        eprintln!(
            "\n[{}] {}\ncwd: {}\n{}",
            request.id,
            request.operation,
            request.cwd.display(),
            request.detail
        );
        eprint!("Allow once? [y/N] ");
        std::io::stderr().flush()?;
        let allowed = input.next_line().await?.is_some_and(|line| {
            line.trim().eq_ignore_ascii_case("y") || line.trim().eq_ignore_ascii_case("yes")
        });
        stream
            .write_all(if allowed { b"allow\n" } else { b"deny\n" })
            .await?;
    }
}
