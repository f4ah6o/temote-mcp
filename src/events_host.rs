//! Durable, credential-free Host transition handoff to the active Fabric Link.
//! The supervisor activity stream and the job owner enqueue facts; only the
//! connected Link supplies its current generation and authenticates delivery.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use temote_mcp::activity::contract::{ActivityEvent, ActivityOperation, ActivityState};
use uuid::Uuid;

use crate::{config, gateway::GatewayClient, session_control};

const MAX_PENDING: usize = 256;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const PENDING_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
const EVENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_DELIVERY_BATCH: usize = 2;
static NEXT_DELIVERY_OFFSET: AtomicUsize = AtomicUsize::new(0);

struct QueueStore {
    root: PathBuf,
}

impl QueueStore {
    fn operator() -> Result<Self> {
        let state = config::state_dir()?;
        check_private_dir(&state)?;
        Ok(Self {
            root: state.join("fabric-events"),
        })
    }

    fn host_dir(&self, host_id: &str) -> Result<PathBuf> {
        crate::host_identity::validate(host_id)?;
        check_private_dir(self.root.parent().context("event queue has no parent")?)?;
        ensure_private_dir(&self.root)?;
        let dir = self.root.join(host_id);
        ensure_private_dir(&dir)?;
        Ok(dir)
    }
}

#[derive(Serialize, Deserialize)]
struct Pending {
    created_ms: u64,
    body: Value,
}

fn now_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

fn timestamp(ms: u64) -> Result<String> {
    let seconds: libc::time_t = (ms / 1000).try_into()?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    anyhow::ensure!(
        !unsafe { libc::gmtime_r(&seconds, tm.as_mut_ptr()) }.is_null(),
        "invalid event time"
    );
    let tm = unsafe { tm.assume_init() };
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        ms % 1000
    ))
}

fn check_private_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.file_type().is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "Fabric event queue is not private"
    );
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    anyhow::ensure!(
        directory.metadata()?.uid() == unsafe { libc::geteuid() },
        "Fabric event queue owner changed"
    );
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_dir() && metadata.uid() == unsafe { libc::geteuid() },
                "Fabric event queue path is not an owner directory"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    anyhow::ensure!(
        directory.metadata()?.uid() == unsafe { libc::geteuid() },
        "Fabric event queue owner changed"
    );
    directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    check_private_dir(path)
}

fn check_private_file(file: &fs::File) -> Result<()> {
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.file_type().is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "Fabric event queue file is not private"
    );
    Ok(())
}

fn with_queue_lock<T>(
    store: &QueueStore,
    host_id: &str,
    operation: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    let dir = store.host_dir(host_id)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(".lock"))?;
    check_private_file(&lock)?;
    anyhow::ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
        "Fabric event queue lock unavailable"
    );
    let result = operation(&dir);
    let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
    result
}

fn pending_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut total = 0usize;
    for entry in fs::read_dir(dir)? {
        total += 1;
        anyhow::ensure!(
            total <= MAX_PENDING + 1,
            "Fabric event queue directory exceeds entry limit"
        );
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn enqueue(host_id: &str, body: Value) -> Result<()> {
    enqueue_in(&QueueStore::operator()?, host_id, body)
}

fn enqueue_in(store: &QueueStore, host_id: &str, body: Value) -> Result<()> {
    let pending = Pending {
        created_ms: now_ms()?,
        body,
    };
    let bytes = serde_json::to_vec(&pending)?;
    anyhow::ensure!(
        bytes.len() <= MAX_RECORD_BYTES,
        "Fabric transition exceeds queue record limit"
    );
    with_queue_lock(store, host_id, |dir| {
        anyhow::ensure!(
            pending_paths(dir)?.len() < MAX_PENDING,
            "Fabric transition queue is full"
        );
        let path = dir.join(format!(
            "{:020}-{}.json",
            pending.created_ms,
            Uuid::new_v4()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    })
}

fn transition_body(
    view: &session_control::SessionView,
    name: &str,
    state: &str,
    job_id: Option<&str>,
    at_ms: u64,
) -> Result<Option<Value>> {
    if view.yolo
        || view.permission_mode.is_yolo()
        || view.started_at == 0
        || view.process_id == 0
        || !matches!(
            view.permission_mode,
            config::PermissionMode::Ask | config::PermissionMode::Agent
        )
        || view.permitted_directories.is_empty()
        || view.permitted_directories.len() > 32
    {
        return Ok(None);
    }
    let cwd = view.cwd.to_str().context("non-UTF8 event session cwd")?;
    let directories = view
        .permitted_directories
        .iter()
        .map(|path| {
            path.to_str()
                .map(str::to_owned)
                .context("non-UTF8 event session scope")
        })
        .collect::<Result<Vec<_>>>()?;
    if cwd.len() > 4096 || directories.iter().any(|path| path.len() > 4096) {
        return Ok(None);
    }
    let mut body = json!({
        "name":name, "session_id":view.session_id, "state":state,
        "timestamp":timestamp(at_ms)?, "session_started_at":view.started_at,
        "session_process_id":view.process_id, "session_restart_count":view.restart_count,
        "session_permission_mode":view.permission_mode.as_str(),
        "session_cwd":cwd, "session_permitted_directories":directories,
    });
    if let Some(job_id) = job_id {
        body["job_id"] = json!(job_id);
    }
    // The Worker enforces a 4096-byte ingress body limit. Leave room for
    // generation and instance fields added by the connected Link.
    Ok((serde_json::to_vec(&body)?.len() <= 3500).then_some(body))
}

pub(crate) async fn record_lifecycle(event: &ActivityEvent, host_id: &str) -> Result<()> {
    let expected = match (event.operation(), event.state()) {
        (
            ActivityOperation::SessionStart
            | ActivityOperation::SessionRestart
            | ActivityOperation::SessionAutoRestart,
            ActivityState::Completed,
        ) => "active",
        (ActivityOperation::SessionStop, ActivityState::Completed) => "stopped",
        (ActivityOperation::SessionCrash, ActivityState::Failed) => "crashed",
        _ => return Ok(()),
    };
    let Some(session_id) = event.session_id() else {
        return Ok(());
    };
    let view = session_control::event_session_view(session_id).await?;
    if view.host_id != host_id || view.status != expected {
        return Ok(());
    }
    if let Some(body) = transition_body(
        &view,
        "session.state.changed",
        &view.status,
        None,
        event.timestamp_ms(),
    )? {
        enqueue(host_id, body)?;
    }
    Ok(())
}

pub(crate) async fn record_stopped_job(session_id: &str, job_id: Uuid) -> Result<()> {
    let view = session_control::event_session_view(session_id).await?;
    if view.status != "active" {
        return Ok(());
    }
    if let Some(body) = transition_body(
        &view,
        "job.state.changed",
        "stopped",
        Some(&job_id.to_string()),
        now_ms()?,
    )? {
        enqueue(&view.host_id, body)?;
    }
    Ok(())
}

pub(crate) async fn deliver_pending(
    gateway: &GatewayClient,
    host_id: &str,
    instance_id: &str,
    generation: u64,
) -> Result<()> {
    deliver_pending_in(
        &QueueStore::operator()?,
        gateway,
        host_id,
        instance_id,
        generation,
    )
    .await
}

async fn deliver_pending_in(
    store: &QueueStore,
    gateway: &GatewayClient,
    host_id: &str,
    instance_id: &str,
    generation: u64,
) -> Result<()> {
    let paths = with_queue_lock(store, host_id, pending_paths)?;
    if paths.is_empty() {
        return Ok(());
    }
    let offset =
        NEXT_DELIVERY_OFFSET.fetch_add(MAX_DELIVERY_BATCH, Ordering::Relaxed) % paths.len();
    for path in paths
        .iter()
        .cycle()
        .skip(offset)
        .take(MAX_DELIVERY_BATCH.min(paths.len()))
    {
        let pending = with_queue_lock(store, host_id, |_| {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            check_private_file(&file)?;
            let mut bytes = Vec::new();
            file.take((MAX_RECORD_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(
                bytes.len() <= MAX_RECORD_BYTES,
                "Fabric event record is oversized"
            );
            Ok(serde_json::from_slice::<Pending>(&bytes)?)
        })?;
        if now_ms()?.saturating_sub(pending.created_ms) >= PENDING_LIFETIME.as_millis() as u64 {
            with_queue_lock(store, host_id, |_| {
                fs::remove_file(path)?;
                Ok(())
            })?;
            continue;
        }
        let mut body = pending.body;
        body["generation"] = json!(generation);
        body["host_instance_id"] = json!(instance_id);
        let response = gateway
            .request_with_client(
                &gateway.sync_client,
                Method::POST,
                &format!("/v1/hosts/{host_id}/events/transition"),
                Some(host_id),
            )
            .timeout(EVENT_REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await;
        let Ok(response) = response else {
            continue;
        };
        if response.status() == StatusCode::CONFLICT {
            if let Some(session_id) = body.get("session_id").and_then(Value::as_str)
                && let Ok(view) = session_control::event_session_view(session_id).await
                && view.host_id == host_id
                && view.session_id == session_id
                && same_scope(&view, &body) == Some(false)
            {
                with_queue_lock(store, host_id, |_| {
                    fs::remove_file(path)?;
                    Ok(())
                })?;
            }
            continue;
        }
        let accepted = if response.status().is_success() {
            match crate::gateway::read_bounded_body(response, 1024, "Fabric event ack").await {
                Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .is_some_and(|value| value.get("accepted") == Some(&Value::Bool(true))),
                Err(_) => false,
            }
        } else {
            response.status() == StatusCode::BAD_REQUEST
        };
        if accepted {
            with_queue_lock(store, host_id, |_| {
                fs::remove_file(path)?;
                Ok(())
            })?;
        }
    }
    Ok(())
}

fn same_scope(view: &session_control::SessionView, body: &Value) -> Option<bool> {
    if view.yolo
        || !matches!(
            view.status.as_str(),
            "starting" | "active" | "stopping" | "stopped" | "crashed" | "failed"
        )
    {
        return None;
    }
    let started = body.get("session_started_at")?.as_u64()?;
    let process = body.get("session_process_id")?.as_u64()?;
    let restart = body.get("session_restart_count")?.as_u64()?;
    let mode = body.get("session_permission_mode")?.as_str()?;
    let cwd = body.get("session_cwd")?.as_str()?;
    let dirs = body.get("session_permitted_directories")?.as_array()?;
    if dirs.is_empty() || dirs.iter().any(|path| path.as_str().is_none()) {
        return None;
    }
    Some(
        started == view.started_at
            && process == u64::from(view.process_id)
            && restart == u64::from(view.restart_count)
            && mode == view.permission_mode.as_str()
            && Some(cwd) == view.cwd.to_str()
            && Value::Array(dirs.clone())
                == serde_json::to_value(&view.permitted_directories).ok()?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn private_fixture() -> TempDir {
        let temp = TempDir::new().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        temp
    }

    #[test]
    fn queue_is_private_durable_and_bounded() {
        let temp = private_fixture();
        let store = QueueStore {
            root: temp.path().join("queue"),
        };
        let host = format!("events-{}", Uuid::new_v4());
        enqueue_in(
            &store,
            &host,
            json!({"name":"session.state.changed","state":"active"}),
        )
        .unwrap();
        let dir = store.host_dir(&host).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let paths = pending_paths(&dir).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(
            fs::metadata(&paths[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let pending: Pending = serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
        assert_eq!(pending.body["state"], "active");
        assert!(pending.body.get("generation").is_none());
        assert!(pending.body.get("host_instance_id").is_none());
        for _ in 1..MAX_PENDING {
            enqueue_in(&store, &host, json!({"state":"active"})).unwrap();
        }
        assert!(enqueue_in(&store, &host, json!({"state":"active"})).is_err());
        assert_eq!(pending_paths(&dir).unwrap().len(), MAX_PENDING);
        fs::write(dir.join("extra.tmp"), b"x").unwrap();
        assert!(pending_paths(&dir).is_err());
    }

    #[test]
    fn queue_rejects_link_and_non_directory_before_chmod() {
        let temp = private_fixture();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o750)).unwrap();
        let linked = temp.path().join("queue");
        symlink(&target, &linked).unwrap();
        let store = QueueStore { root: linked };
        assert!(store.host_dir("events-host").is_err());
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o750
        );
        fs::write(temp.path().join("ordinary-file"), b"x").unwrap();
        let store = QueueStore {
            root: temp.path().join("ordinary-file"),
        };
        assert!(store.host_dir("events-host").is_err());
    }

    #[tokio::test]
    async fn current_link_authenticates_and_flushes_pending_transition() {
        let temp = private_fixture();
        let store = QueueStore {
            root: temp.path().join("queue"),
        };
        let host = format!("events-{}", Uuid::new_v4());
        enqueue_in(
            &store,
            &host,
            json!({"name":"session.state.changed","session_id":"session-a",
            "state":"active","timestamp":timestamp(now_ms().unwrap()).unwrap()}),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for accepted in [false, true] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&bytes[..header_end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap()
                            .parse::<usize>()
                            .unwrap();
                        if bytes.len() >= header_end + 4 + length {
                            assert!(headers.starts_with("post /v1/hosts/"));
                            assert!(headers.contains("authorization: bearer host-token"));
                            assert!(headers.contains("x-temote-host-id: events-"));
                            let body: Value = serde_json::from_slice(
                                &bytes[header_end + 4..header_end + 4 + length],
                            )
                            .unwrap();
                            assert_eq!(body["generation"], 17);
                            assert_eq!(body["host_instance_id"], "current-instance");
                            assert_eq!(body["state"], "active");
                            let payload = if accepted {
                                b"{\"accepted\":true}".as_slice()
                            } else {
                                b"{\"accepted\":false}".as_slice()
                            };
                            let header = format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\n\r\n",
                                payload.len()
                            );
                            socket.write_all(header.as_bytes()).await.unwrap();
                            socket.write_all(payload).await.unwrap();
                            break;
                        }
                    }
                }
            }
        });
        let gateway = GatewayClient {
            client: reqwest::Client::new(),
            sync_client: reqwest::Client::new(),
            base_url: format!("http://{address}"),
            host_token: "host-token".to_owned(),
            access_client_id: None,
            access_client_secret: None,
        };
        deliver_pending_in(&store, &gateway, &host, "current-instance", 17)
            .await
            .unwrap();
        assert_eq!(
            pending_paths(&store.host_dir(&host).unwrap())
                .unwrap()
                .len(),
            1
        );
        deliver_pending_in(&store, &gateway, &host, "current-instance", 17)
            .await
            .unwrap();
        server.await.unwrap();
        assert!(
            pending_paths(&store.host_dir(&host).unwrap())
                .unwrap()
                .is_empty()
        );
    }
}
