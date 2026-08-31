use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const SESSION_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_SESSION_PROBE_RESPONSE_BYTES: usize = 64;
const MAX_SESSION_METADATA_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    Starting,
    Active,
    Stopping,
    Stopped,
    Crashed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionLifecycle {
    pub status: LifecycleStatus,
    pub started_at: u64,
    pub stopped_at: Option<u64>,
    pub exit_reason: Option<String>,
    pub last_error: Option<String>,
    pub logical_path: Option<String>,
    pub restart_policy: String,
    #[serde(default)]
    pub restart_count: u32,
    #[serde(default)]
    pub last_restart_at: Option<u64>,
    #[serde(default)]
    pub next_restart_at: Option<u64>,
    #[serde(default)]
    pub restart_limit_reason: Option<String>,
}

impl SessionLifecycle {
    pub fn starting(started_at: u64, logical_path: Option<String>) -> Self {
        Self {
            status: LifecycleStatus::Starting,
            started_at,
            stopped_at: None,
            exit_reason: None,
            last_error: None,
            logical_path,
            restart_policy: "never".to_owned(),
            restart_count: 0,
            last_restart_at: None,
            next_restart_at: None,
            restart_limit_reason: None,
        }
    }
}

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
    #[serde(default)]
    pub yolo: bool,
}

pub fn state_dir() -> Result<PathBuf> {
    crate::platform_paths::state_dir()
        .or_else(crate::platform_paths::data_local_dir)
        .map(|path| path.join("temote-mcp"))
        .context("could not determine a local state directory")
}

pub fn session_path(id: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    Ok(sessions_dir()?.join(format!("{id}.json")))
}

pub fn session_lifecycle_path(id: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    Ok(sessions_dir()?.join(format!("{id}.state")))
}

pub fn supervisor_socket_path() -> Result<PathBuf> {
    Ok(socket_dir()?.join("supervisor.sock"))
}

pub fn sessions_dir() -> Result<PathBuf> {
    Ok(state_dir()?.join("sessions"))
}

pub fn socket_path(id: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    Ok(socket_dir()?.join(format!("{id}.sock")))
}

/// Returns a short, per-user directory for Unix-domain session sockets.
///
/// Socket paths have a platform-specific length limit (104 bytes on macOS),
/// so they cannot live below the regular state directory, which may include
/// a long home-directory path. Session metadata remains in `state_dir()`.
fn socket_dir() -> Result<PathBuf> {
    // `TMPDIR` on macOS can itself be long, so use the conventional short
    // system temporary directory rather than `std::env::temp_dir()`. A short
    // optional namespace exists for isolated process-boundary tests or
    // deliberately parallel supervisors owned by the same user.
    let uid = unsafe { libc::geteuid() };
    let namespace = std::env::var("TEMOTE_MCP_SOCKET_NAMESPACE")
        .ok()
        .filter(|value| !value.is_empty());
    socket_dir_for(uid, namespace.as_deref())
}

fn socket_dir_for(uid: libc::uid_t, namespace: Option<&str>) -> Result<PathBuf> {
    match namespace {
        None => Ok(PathBuf::from("/tmp").join(format!("temote-mcp-{uid}"))),
        Some(namespace) => {
            validate_socket_namespace(namespace)?;
            Ok(PathBuf::from("/tmp").join(format!("tmcp-{uid}-{namespace}")))
        }
    }
}

fn validate_socket_namespace(namespace: &str) -> Result<()> {
    anyhow::ensure!(
        !namespace.is_empty() && namespace.len() <= 12,
        "TEMOTE_MCP_SOCKET_NAMESPACE must contain 1..=12 ASCII characters"
    );
    anyhow::ensure!(
        namespace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "TEMOTE_MCP_SOCKET_NAMESPACE accepts only ASCII letters, digits, '-' and '_'"
    );
    Ok(())
}

pub fn session_id(id: Option<&str>) -> Result<String> {
    let id = id
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    validate_session_id(&id)?;
    Ok(id)
}

pub fn new_session(cwd: &Path, id: Option<&str>, yolo: bool) -> Result<Session> {
    let cwd = canonical_directory(cwd)?;
    let id = session_id(id)?;
    let session = Session {
        id,
        cwd: cwd.clone(),
        permitted_directories: vec![cwd],
        started_at: unix_time(),
        process_id: 0,
        yolo,
    };
    Ok(session)
}

pub async fn load_session(id: &str) -> Result<Session> {
    anyhow::ensure!(
        session_is_active(id).await?,
        "session {id} is not running; run temote-mcp start {id} first"
    );
    read_session_metadata(id).await
}

pub async fn read_session_metadata(id: &str) -> Result<Session> {
    let path = session_path(id)?;
    let file = open_session_metadata_nofollow(&path)
        .with_context(|| format!("session {id} was not found or could not be opened safely"))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect session metadata {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "session metadata is not a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_SESSION_METADATA_BYTES as u64,
        "session metadata exceeds {MAX_SESSION_METADATA_BYTES} bytes: {}",
        path.display()
    );
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::from_std(file);
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_SESSION_METADATA_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("cannot read session metadata {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_SESSION_METADATA_BYTES,
        "session metadata exceeds {MAX_SESSION_METADATA_BYTES} bytes: {}",
        path.display()
    );
    let session: Session = serde_json::from_slice(&bytes).context("invalid temote-mcp session")?;
    validate_loaded_session(id, &session)?;
    Ok(session)
}

fn open_session_metadata_nofollow(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .with_context(|| format!("cannot open session metadata {}", path.display()))
}

pub async fn session_is_active(id: &str) -> Result<bool> {
    let path = socket_path(id)?;
    tokio::time::timeout(SESSION_PROBE_TIMEOUT, probe_session_socket(&path))
        .await
        .with_context(|| format!("timed out inspecting session socket for {id}"))?
}

async fn probe_session_socket(path: &Path) -> Result<bool> {
    let mut stream = match tokio::net::UnixStream::connect(path).await {
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
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    stream.write_all(br#"{"type":"probe"}"#).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    let mut response = String::new();
    let read = BufReader::new(stream)
        .take((MAX_SESSION_PROBE_RESPONSE_BYTES + 1) as u64)
        .read_line(&mut response)
        .await?;
    anyhow::ensure!(
        read <= MAX_SESSION_PROBE_RESPONSE_BYTES,
        "session probe response exceeds {MAX_SESSION_PROBE_RESPONSE_BYTES} bytes"
    );
    validate_session_probe_response(&response)?;
    Ok(true)
}

fn validate_session_probe_response(response: &str) -> Result<()> {
    anyhow::ensure!(
        response.trim() == "active",
        "unexpected session probe response: {:?}",
        response.trim()
    );
    Ok(())
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

pub async fn read_session_lifecycle(id: &str) -> Result<Option<SessionLifecycle>> {
    let path = session_lifecycle_path(id)?;
    let file = match open_session_metadata_nofollow(&path) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot read session lifecycle {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect session lifecycle {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "session lifecycle is not a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_SESSION_METADATA_BYTES as u64,
        "session lifecycle exceeds {MAX_SESSION_METADATA_BYTES} bytes: {}",
        path.display()
    );
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::from_std(file);
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_SESSION_METADATA_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("cannot read session lifecycle {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_SESSION_METADATA_BYTES,
        "session lifecycle exceeds {MAX_SESSION_METADATA_BYTES} bytes: {}",
        path.display()
    );
    let lifecycle =
        serde_json::from_slice(&bytes).context("invalid temote-mcp session lifecycle")?;
    Ok(Some(lifecycle))
}

pub async fn save_session_lifecycle(id: &str, lifecycle: &SessionLifecycle) -> Result<()> {
    validate_session_id(id)?;
    let path = session_lifecycle_path(id)?;
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let temporary = path.with_extension(format!(
        "state.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bytes = serde_json::to_vec_pretty(lifecycle)?;
    anyhow::ensure!(
        bytes.len() <= MAX_SESSION_METADATA_BYTES,
        "session lifecycle exceeds {MAX_SESSION_METADATA_BYTES} bytes"
    );
    let result = async {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .await?;
        }
        file.write_all(&bytes).await?;
        file.flush().await?;
        drop(file);
        tokio::fs::rename(&temporary, &path).await?;
        Result::<()>::Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

pub async fn save_session(session: &Session) -> Result<()> {
    validate_session_id(&session.id)?;
    let path = session_path(&session.id)?;
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let temporary = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bytes = serde_json::to_vec_pretty(session)?;
    let result = async {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .with_context(|| {
                format!(
                    "cannot create session temporary file {}",
                    temporary.display()
                )
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .await?;
        }
        file.write_all(&bytes).await?;
        file.flush().await?;
        drop(file);
        tokio::fs::rename(&temporary, &path).await?;
        Result::<()>::Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

fn validate_loaded_session(requested_id: &str, session: &Session) -> Result<()> {
    validate_session_id(requested_id)?;
    anyhow::ensure!(
        session.id == requested_id,
        "session metadata ID mismatch: requested {requested_id}, found {}",
        session.id
    );
    validate_session_id(&session.id)?;

    let canonical_cwd = canonical_directory(&session.cwd)?;
    anyhow::ensure!(
        canonical_cwd == session.cwd,
        "session cwd is not canonical: {}",
        session.cwd.display()
    );
    anyhow::ensure!(
        !session.permitted_directories.is_empty(),
        "session metadata has no permitted directories"
    );

    let mut seen = BTreeSet::new();
    for root in &session.permitted_directories {
        let canonical = canonical_directory(root)?;
        anyhow::ensure!(
            canonical == *root,
            "session permitted root is not canonical: {}",
            root.display()
        );
        anyhow::ensure!(
            seen.insert(root.clone()),
            "duplicate session permitted root: {}",
            root.display()
        );
    }
    anyhow::ensure!(
        seen.contains(&session.cwd),
        "session cwd is missing from permitted directories"
    );
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
    if session.yolo {
        return Ok(());
    }
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

pub fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn validates_custom_session_ids() {
        assert!(validate_session_id("my-project_1.dev").is_ok());
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("../escape").is_err());
        assert!(validate_session_id("contains spaces").is_err());
        assert!(validate_session_id(&"x".repeat(65)).is_err());
    }

    #[test]
    fn socket_namespace_is_short_and_fail_closed() {
        let path = socket_dir_for(1000, Some("e2e123abc")).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/tmcp-1000-e2e123abc"));
        for invalid in ["", "contains/slash", "contains space", "1234567890123"] {
            assert!(validate_socket_namespace(invalid).is_err(), "{invalid:?}");
        }
        assert!(validate_socket_namespace("abc-DEF_12").is_ok());
        let longest_socket = path.join(format!("{}.sock", "x".repeat(64)));
        assert!(longest_socket.as_os_str().len() < 104);
    }

    #[test]
    fn puts_sockets_in_a_short_per_user_directory() {
        let path = socket_path("7418eda5-fd07-4e00-ace5-c1ece2f68a02").unwrap();
        assert_eq!(path.parent(), Some(socket_dir().unwrap().as_path()));
        assert!(path.as_os_str().len() < 104);
    }

    #[test]
    fn generated_session_paths_are_deterministic_distinct_and_short() -> noprop::TestResult {
        test_support::run(0x534f_434b_4554_0001, 512, |ctx| {
            let nonce = noprop::sample_u64(ctx);
            let id_a = format!("{}-{nonce:x}-a", test_support::safe_component(ctx));
            let id_b = format!("{}-{nonce:x}-b", test_support::safe_component(ctx));
            let socket_a = socket_path(&id_a).unwrap();
            let socket_b = socket_path(&id_b).unwrap();
            let state_a = session_path(&id_a).unwrap();
            let state_b = session_path(&id_b).unwrap();

            assert_eq!(socket_path(&id_a).unwrap(), socket_a);
            assert_eq!(session_path(&id_a).unwrap(), state_a);
            assert_ne!(socket_a, socket_b);
            assert_ne!(state_a, state_b);
            assert_eq!(socket_a.parent(), Some(socket_dir().unwrap().as_path()));
            assert!(
                socket_a.as_os_str().len() < 104,
                "socket path too long: {socket_a:?}"
            );
            assert!(
                socket_b.as_os_str().len() < 104,
                "socket path too long: {socket_b:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_session_probe_responses_accept_only_active() -> noprop::TestResult {
        test_support::run(0x5052_4f42_4552_4553, test_support::DEFAULT_CASES, |ctx| {
            let response = if noprop::sample_bool(ctx) {
                let left = " ".repeat(noprop::sample_usize_in(ctx, 0..=3));
                let right = "\n".repeat(noprop::sample_usize_in(ctx, 0..=3));
                format!("{left}active{right}")
            } else {
                test_support::ascii_string(ctx, 24)
            };
            let expected = response.trim() == "active";
            assert_eq!(
                validate_session_probe_response(&response).is_ok(),
                expected,
                "response={response:?}"
            );
            Ok(())
        })
    }

    #[tokio::test]
    async fn unexpected_session_probe_response_fails_closed() {
        let id = format!("unexpected-probe-{}", Uuid::new_v4());
        let path = socket_path(&id).unwrap();
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let _ = tokio::fs::remove_file(&path).await;
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"not-active\n").await.unwrap();
            let _ = stream.shutdown().await;
        });

        let error = session_is_active(&id).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unexpected session probe response")
        );

        server.await.unwrap();
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn stalled_session_probe_times_out_without_becoming_inactive() {
        let id = format!("stalled-probe-{}", Uuid::new_v4());
        let path = socket_path(&id).unwrap();
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let _ = tokio::fs::remove_file(&path).await;
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let error = session_is_active(&id).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("timed out inspecting session socket")
        );

        server.abort();
        let _ = server.await;
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn oversized_session_probe_response_fails_closed() {
        let id = format!("oversized-probe-{}", Uuid::new_v4());
        let path = socket_path(&id).unwrap();
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let _ = tokio::fs::remove_file(&path).await;
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(&[b'x'; MAX_SESSION_PROBE_RESPONSE_BYTES + 1])
                .await
                .unwrap();
            let _ = stream.shutdown().await;
        });

        let error = session_is_active(&id).await.unwrap_err();
        assert!(error.to_string().contains("probe response exceeds"));

        server.await.unwrap();
        tokio::fs::remove_file(path).await.unwrap();
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
            yolo: false,
        };
        std::fs::write(root.join("inside.txt"), "ok").unwrap();

        assert!(resolve_existing_path(&session, Path::new("inside.txt")).is_ok());
        assert!(resolve_existing_path(&session, &outside).is_err());
        assert!(resolve_write_path(&session, Path::new("new.txt")).is_ok());
        assert!(resolve_write_path(&session, &outside.join("new.txt")).is_err());
    }

    #[test]
    fn yolo_session_allows_paths_outside_configured_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = canonical_directory(root.path()).unwrap();
        let outside = canonical_directory(outside.path()).unwrap();
        let session = Session {
            id: "test-yolo".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root],
            started_at: 0,
            process_id: 0,
            yolo: true,
        };
        std::fs::write(outside.join("outside.txt"), "ok").unwrap();

        assert!(resolve_existing_path(&session, &outside.join("outside.txt")).is_ok());
        assert!(resolve_write_path(&session, &outside.join("new.txt")).is_ok());
        assert_eq!(resolve_cwd(&session, Some(&outside)).unwrap(), outside);
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
            yolo: false,
        };

        assert!(resolve_existing_path(&session, Path::new("outside/secret.txt")).is_err());
        assert!(resolve_write_path(&session, Path::new("outside/new.txt")).is_err());
    }

    #[test]
    fn generated_loaded_session_structure_matches_reference_invariants() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let cwd = fixture.path().join("cwd");
        let extra_a = fixture.path().join("extra-a");
        let extra_b = fixture.path().join("extra-b");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(&extra_a).unwrap();
        std::fs::create_dir(&extra_b).unwrap();
        let cwd = canonical_directory(&cwd).unwrap();
        let extra_a = canonical_directory(&extra_a).unwrap();
        let extra_b = canonical_directory(&extra_b).unwrap();

        test_support::run(0x5345_5353_4d45_5441, 512, |ctx| {
            let requested = format!("meta-{:x}", noprop::sample_u64(ctx));
            let mut session = Session {
                id: requested.clone(),
                cwd: cwd.clone(),
                permitted_directories: vec![cwd.clone(), extra_a.clone(), extra_b.clone()],
                started_at: noprop::sample_u64(ctx),
                process_id: noprop::sample_u32(ctx),
                yolo: noprop::sample_bool(ctx),
            };
            let mutation = noprop::sample_usize_in(ctx, 0..=5);
            let expected = mutation == 0;
            match mutation {
                0 => {}
                1 => session.id = format!("other-{:x}", noprop::sample_u64(ctx)),
                2 => session.permitted_directories.clear(),
                3 => session.permitted_directories.retain(|root| root != &cwd),
                4 => session.permitted_directories.push(extra_a.clone()),
                _ => session.cwd = cwd.join(".."),
            }
            assert_eq!(
                validate_loaded_session(&requested, &session).is_ok(),
                expected,
                "mutation={mutation} session={:?}",
                session.id
            );
            Ok(())
        })
    }

    #[tokio::test]
    async fn session_metadata_reader_rejects_oversized_files() {
        let id = format!("oversized-meta-{}", Uuid::new_v4());
        let path = session_path(&id).unwrap();
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_SESSION_METADATA_BYTES as u64 + 1).unwrap();

        let error = read_session_metadata(&id).await.err().unwrap();
        assert!(error.to_string().contains("session metadata exceeds"));
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_metadata_reader_rejects_symlink_files() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.json");
        std::fs::write(&target, b"{}").unwrap();
        let id = format!("symlink-meta-{}", Uuid::new_v4());
        let path = session_path(&id).unwrap();
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        symlink(&target, &path).unwrap();

        let error = read_session_metadata(&id).await.err().unwrap();
        assert!(error.to_string().contains("could not be opened safely"));
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_session_saves_remain_atomic_and_leave_no_temporary_files() {
        let root = tempfile::tempdir().unwrap();
        let cwd = canonical_directory(root.path()).unwrap();
        let id = format!("concurrent-save-{}", Uuid::new_v4());
        let mut tasks = Vec::new();
        for revision in 0..16_u64 {
            let session = Session {
                id: id.clone(),
                cwd: cwd.clone(),
                permitted_directories: vec![cwd.clone()],
                started_at: revision,
                process_id: revision as u32,
                yolo: revision % 2 == 0,
            };
            tasks.push(tokio::spawn(async move { save_session(&session).await }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        let path = session_path(&id).unwrap();
        let bytes = tokio::fs::read(&path).await.unwrap();
        let saved: Session = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved.id, id);
        validate_loaded_session(&id, &saved).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let prefix = format!("{id}.json.");
        let mut entries = tokio::fs::read_dir(sessions_dir().unwrap()).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.starts_with(&prefix) || !name.ends_with(".tmp"),
                "orphan session temporary file: {name}"
            );
        }
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[test]
    fn session_id_validation_matches_reference_grammar() -> noprop::TestResult {
        test_support::run(0x5345_5353_494f_4e01, test_support::DEFAULT_CASES, |ctx| {
            let id = test_support::ascii_string(ctx, 80);
            let expected = !id.is_empty()
                && id.len() <= 64
                && id.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "-_.".contains(character)
                })
                && id != "."
                && id != "..";
            assert_eq!(
                validate_session_id(&id).is_ok(),
                expected,
                "session ID mismatch for {id:?}"
            );
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn generated_read_and_write_paths_never_follow_symlinks_outside_sandbox() -> noprop::TestResult
    {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let canonical_root = canonical_directory(&root).unwrap();
        let session = Session {
            id: "pbt".to_owned(),
            cwd: canonical_root.clone(),
            permitted_directories: vec![canonical_root],
            started_at: 0,
            process_id: 0,
            yolo: false,
        };

        test_support::run(0x5041_5448_4553_4301, 512, |ctx| {
            let leaf = test_support::safe_component(ctx);
            std::fs::write(outside.join(&leaf), b"secret").unwrap();
            let escaped = PathBuf::from(format!("escape/{leaf}"));
            assert!(
                resolve_existing_path(&session, &escaped).is_err(),
                "read escape unexpectedly allowed: {escaped:?}"
            );
            let write = PathBuf::from(format!("escape/{leaf}.new"));
            assert!(
                resolve_write_path(&session, &write).is_err(),
                "write escape unexpectedly allowed: {write:?}"
            );
            Ok(())
        })
    }
}
