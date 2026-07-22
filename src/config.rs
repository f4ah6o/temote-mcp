use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub cwd: PathBuf,
    #[serde(default)]
    pub permitted_directories: Vec<PathBuf>,
}

pub fn state_dir() -> Result<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|path| path.join("local-mcp"))
        .context("could not determine a local state directory")
}

pub fn session_path(id: Uuid) -> Result<PathBuf> {
    Ok(state_dir()?.join("sessions").join(format!("{id}.json")))
}

pub fn socket_path(id: Uuid) -> Result<PathBuf> {
    Ok(state_dir()?.join("sessions").join(format!("{id}.sock")))
}

pub async fn create_session(cwd: &Path) -> Result<Session> {
    let cwd = canonical_directory(cwd)?;
    let session = Session {
        id: Uuid::new_v4(),
        cwd: cwd.clone(),
        permitted_directories: vec![cwd],
    };
    save_session(&session).await?;
    Ok(session)
}

pub async fn load_session(id: Uuid) -> Result<Session> {
    let path = session_path(id)?;
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("session {id} was not found; run `local-mcp start` first"))?;
    serde_json::from_slice(&bytes).context("invalid local-mcp session")
}

pub async fn save_session(session: &Session) -> Result<()> {
    let path = session_path(session.id)?;
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(session)?).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

pub fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    anyhow::ensure!(path.is_dir(), "{} is not a directory", path.display());
    Ok(path)
}
