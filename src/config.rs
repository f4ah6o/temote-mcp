use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub permitted_directories: Vec<PathBuf>,
    #[serde(default)]
    pub started_at: u64,
    #[serde(default)]
    pub process_id: u32,
}

pub fn state_dir() -> Result<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|path| path.join("local-mcp"))
        .context("could not determine a local state directory")
}

pub fn session_path(id: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    Ok(sessions_dir()?.join(format!("{id}.json")))
}

pub fn sessions_dir() -> Result<PathBuf> {
    Ok(state_dir()?.join("sessions"))
}

pub fn socket_path(id: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    Ok(socket_dir().join(format!("{id}.sock")))
}

/// Returns a short, per-user directory for Unix-domain session sockets.
///
/// Socket paths have a platform-specific length limit (104 bytes on macOS),
/// so they cannot live below the regular state directory, which may include
/// a long home-directory path. Session metadata remains in `state_dir()`.
fn socket_dir() -> PathBuf {
    // `TMPDIR` on macOS can itself be long, so use the conventional short
    // system temporary directory rather than `std::env::temp_dir()`.
    let uid = unsafe { libc::geteuid() };
    PathBuf::from("/tmp").join(format!("local-mcp-{uid}"))
}

pub fn session_id(id: Option<&str>) -> Result<String> {
    let id = id
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    validate_session_id(&id)?;
    Ok(id)
}

pub fn new_session(cwd: &Path, id: Option<&str>) -> Result<Session> {
    let cwd = canonical_directory(cwd)?;
    let id = session_id(id)?;
    let session = Session {
        id,
        cwd: cwd.clone(),
        permitted_directories: vec![cwd],
        started_at: unix_time(),
        process_id: 0,
    };
    Ok(session)
}

pub async fn load_session(id: &str) -> Result<Session> {
    let path = session_path(id)?;
    anyhow::ensure!(
        session_is_active(id).await?,
        "session {id} is not running; run local-mcp start {id} first"
    );
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("session {id} was not found; run `local-mcp start` first"))?;
    serde_json::from_slice(&bytes).context("invalid local-mcp session")
}

pub async fn session_is_active(id: &str) -> Result<bool> {
    let path = socket_path(id)?;
    let mut stream = match tokio::net::UnixStream::connect(&path).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionAborted
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error).context("failed to inspect session socket"),
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    stream.write_all(br#"{"type":"probe"}"#).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    Ok(response.trim() == "active")
}

pub async fn remove_inactive_socket(id: &str) -> Result<()> {
    if session_is_active(id).await? {
        anyhow::bail!("session {id} is already running");
    }
    let path = socket_path(id)?;
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to remove stale session socket"),
    }
}

pub async fn save_session(session: &Session) -> Result<()> {
    let path = session_path(&session.id)?;
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(session)?).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

pub fn validate_session_id(id: &str) -> Result<()> {
    anyhow::ensure!(!id.is_empty(), "session ID must not be empty");
    anyhow::ensure!(id.len() <= 64, "session ID must be at most 64 bytes");
    anyhow::ensure!(
        id.chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character)),
        "session ID may contain only ASCII letters, numbers, '-', '_', and '.'"
    );
    anyhow::ensure!(id != "." && id != "..", "invalid session ID");
    Ok(())
}

pub fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    anyhow::ensure!(path.is_dir(), "{} is not a directory", path.display());
    Ok(path)
}

pub fn resolve_existing_path(session: &Session, path: &Path) -> Result<PathBuf> {
    let candidate = resolve_from_cwd(&session.cwd, path);
    let resolved = std::fs::canonicalize(&candidate)
        .with_context(|| format!("cannot resolve {}", candidate.display()))?;
    ensure_permitted(session, &resolved)?;
    Ok(resolved)
}

pub fn resolve_write_path(session: &Session, path: &Path) -> Result<PathBuf> {
    let candidate = resolve_from_cwd(&session.cwd, path);
    let parent = candidate.parent().context("file has no parent directory")?;
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("parent does not exist: {}", parent.display()))?;
    ensure_permitted(session, &parent)?;
    let name = candidate.file_name().context("file name is missing")?;
    anyhow::ensure!(
        name != "." && name != "..",
        "file name must not be '.' or '..'"
    );
    let target = parent.join(name);
    if target.exists() || std::fs::symlink_metadata(&target).is_ok() {
        let resolved = std::fs::canonicalize(&target)
            .with_context(|| format!("cannot resolve {}", target.display()))?;
        ensure_permitted(session, &resolved)?;
    }
    Ok(target)
}

pub fn resolve_cwd(session: &Session, path: Option<&Path>) -> Result<PathBuf> {
    let candidate = path
        .map(|path| resolve_from_cwd(&session.cwd, path))
        .unwrap_or_else(|| session.cwd.clone());
    let resolved = std::fs::canonicalize(&candidate)
        .with_context(|| format!("cannot resolve cwd {}", candidate.display()))?;
    ensure_permitted(session, &resolved)?;
    anyhow::ensure!(
        resolved.is_dir(),
        "cwd is not a directory: {}",
        resolved.display()
    );
    Ok(resolved)
}

pub fn ensure_permitted(session: &Session, path: &Path) -> Result<()> {
    anyhow::ensure!(
        session
            .permitted_directories
            .iter()
            .any(|root| path == root || path.starts_with(root)),
        "path is outside the permitted sandbox roots: {}",
        path.display()
    );
    Ok(())
}

fn resolve_from_cwd(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_custom_session_ids() {
        assert!(validate_session_id("my-project_1.dev").is_ok());
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("../escape").is_err());
        assert!(validate_session_id("contains spaces").is_err());
        assert!(validate_session_id(&"x".repeat(65)).is_err());
    }

    #[test]
    fn puts_sockets_in_a_short_per_user_directory() {
        let path = socket_path("7418eda5-fd07-4e00-ace5-c1ece2f68a02").unwrap();
        assert_eq!(path.parent(), Some(socket_dir().as_path()));
        assert!(path.as_os_str().len() < 104);
    }

    #[test]
    fn path_resolution_stays_inside_permitted_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = canonical_directory(root.path()).unwrap();
        let outside = canonical_directory(outside.path()).unwrap();
        let session = Session {
            id: "test".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root.clone()],
            started_at: 0,
            process_id: 0,
        };
        std::fs::write(root.join("inside.txt"), "ok").unwrap();

        assert!(resolve_existing_path(&session, Path::new("inside.txt")).is_ok());
        assert!(resolve_existing_path(&session, &outside).is_err());
        assert!(resolve_write_path(&session, Path::new("new.txt")).is_ok());
        assert!(resolve_write_path(&session, &outside.join("new.txt")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn path_resolution_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(outside.path(), root.path().join("outside")).unwrap();
        let canonical_root = canonical_directory(root.path()).unwrap();
        let session = Session {
            id: "test".to_owned(),
            cwd: canonical_root.clone(),
            permitted_directories: vec![canonical_root],
            started_at: 0,
            process_id: 0,
        };

        assert!(resolve_existing_path(&session, Path::new("outside/secret.txt")).is_err());
        assert!(resolve_write_path(&session, Path::new("outside/new.txt")).is_err());
    }
}
