use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::{approvals, config, evidence};

const SUPPORTED_APP_SERVER_VERSION: &str = "0.153.4";
const TASK_SCHEMA_VERSION: u64 = 1;
const TASK_RETENTION_SECONDS: u64 = 24 * 60 * 60;
const MAX_TASK_RECORD_BYTES: usize = 64 * 1024;
const MAX_TASK_DIRECTORY_ENTRIES: usize = 4096;
const MAX_TASKS_PER_SCOPE: usize = 128;
const MAX_OPERATION_HISTORY: usize = 128;
const MAX_TASK_INPUT_BYTES: usize = 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 256;
const MAX_RPC_LINE_BYTES: usize = 4 * 1024 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);
const SESSION_STOP_POLL: Duration = Duration::from_secs(1);
const TOKEN_USAGE_FIELDS: &[&str] = &[
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
    "total_tokens",
];

const TASK_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x35, 0x70, 0xb9, 0xde, 0x9e, 0x41, 0x47, 0x89, 0xb8, 0x23, 0x28, 0x07, 0x54, 0x96, 0x11, 0xa4,
]);
const REQUEST_FINGERPRINT_NAMESPACE: Uuid = Uuid::from_bytes([
    0xa5, 0x5f, 0x89, 0x38, 0x32, 0x5c, 0x44, 0xc7, 0x87, 0x9e, 0x24, 0x7a, 0x88, 0x65, 0xa5, 0x5c,
]);

const CODEX_CHILD_ENV_ALLOWLIST: &[&str] = &[
    "ALL_PROXY",
    "CODEX_HOME",
    "HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "LANG",
    "LOGNAME",
    "NO_PROXY",
    "OPENAI_API_KEY",
    "PATH",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "TEMP",
    "TERM",
    "TMP",
    "TMPDIR",
    "USER",
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct SessionInstance {
    id: String,
    started_at: u64,
    process_id: u32,
}

impl SessionInstance {
    fn from_session(session: &config::Session) -> Self {
        Self {
            id: session.id.clone(),
            started_at: session.started_at,
            process_id: session.process_id,
        }
    }

    fn matches(&self, session: &config::Session) -> bool {
        self.id == session.id
            && self.started_at == session.started_at
            && self.process_id == session.process_id
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TaskStatus {
    Accepted,
    Running,
    WaitingApproval,
    Completed,
    Interrupted,
    Failed,
    ReconciliationRequired,
    Unknown,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::ReconciliationRequired => "reconciliation_required",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OperationPhase {
    Accepted,
    Applied,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct OperationOutcome {
    status: TaskStatus,
    revision: u64,
    generation: u64,
    thread_id: Option<String>,
    turn_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct OperationReceipt {
    operation_id: Uuid,
    request_fingerprint: Uuid,
    action: String,
    phase: OperationPhase,
    outcome: OperationOutcome,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct TaskRecord {
    schema_version: u64,
    task_id: Uuid,
    owner: SessionInstance,
    scope_cwd: PathBuf,
    model: String,
    effort: String,
    status: TaskStatus,
    revision: u64,
    generation: u64,
    thread_id: Option<String>,
    turn_id: Option<String>,
    #[serde(default)]
    usage: Option<BTreeMap<String, u64>>,
    created_at: u64,
    updated_at: u64,
    operations: Vec<OperationReceipt>,
}

impl TaskRecord {
    fn outcome(&self) -> OperationOutcome {
        OperationOutcome {
            status: self.status,
            revision: self.revision,
            generation: self.generation,
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct TaskStore {
    directory: PathBuf,
}

fn store_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

impl TaskStore {
    fn default_store() -> Result<Self> {
        Ok(Self {
            directory: config::state_dir()?.join("codex-tasks"),
        })
    }

    #[cfg(test)]
    fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn ensure_directory(&self) -> Result<()> {
        if let Some(parent) = self.directory.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::symlink_metadata(&self.directory) {
            Ok(metadata) => validate_store_directory(&self.directory, &metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&self.directory)?;
                #[cfg(unix)]
                std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))?;
                let metadata = std::fs::symlink_metadata(&self.directory)?;
                validate_store_directory(&self.directory, &metadata)
            }
            Err(error) => Err(error).context("cannot inspect Codex task store"),
        }
    }

    fn path(&self, task_id: Uuid) -> PathBuf {
        self.directory.join(format!("{task_id}.json"))
    }

    fn load(&self, session: &config::Session, task_id: Uuid) -> Result<TaskRecord> {
        let _guard = store_lock().lock().unwrap();
        self.load_locked(session, task_id)
    }

    fn load_locked(&self, session: &config::Session, task_id: Uuid) -> Result<TaskRecord> {
        let record = self.read_record(task_id).map_err(|error| {
            if is_not_found(&error) {
                anyhow::anyhow!("CODEX_TASK_NOT_FOUND: task was not found")
            } else {
                error
            }
        })?;
        ensure_task_owner(&record, session)?;
        Ok(record)
    }

    fn save(&self, record: &TaskRecord) -> Result<()> {
        let _guard = store_lock().lock().unwrap();
        self.save_locked(record)
    }

    fn save_locked(&self, record: &TaskRecord) -> Result<()> {
        self.ensure_directory()?;
        validate_record(record)?;
        self.prune_locked(record)?;
        let bytes = serde_json::to_vec_pretty(record)?;
        anyhow::ensure!(
            bytes.len() <= MAX_TASK_RECORD_BYTES,
            "Codex task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let path = self.path(record.task_id);
        reject_symlink_target(&path)?;
        let temporary = self
            .directory
            .join(format!(".{}.{}.tmp", record.task_id, Uuid::new_v4()));
        let mut cleanup = TemporaryFile::new(temporary.clone());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let mut file = options.open(&temporary)?;
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        reject_symlink_target(&path)?;
        std::fs::rename(&temporary, &path)?;
        cleanup.disarm();
        if let Ok(directory) = File::open(&self.directory) {
            let _ = directory.sync_all();
        }
        Ok(())
    }

    fn update<F>(&self, session: &config::Session, task_id: Uuid, f: F) -> Result<TaskRecord>
    where
        F: FnOnce(&mut TaskRecord) -> Result<()>,
    {
        let _guard = store_lock().lock().unwrap();
        let mut record = self.load_locked(session, task_id)?;
        f(&mut record)?;
        record.updated_at = config::unix_time();
        self.save_locked(&record)?;
        Ok(record)
    }

    fn read_record(&self, task_id: Uuid) -> Result<TaskRecord> {
        let path = self.path(task_id);
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let file = options.open(&path)?;
        let metadata = file.metadata()?;
        validate_private_regular_file(&path, &metadata)?;
        anyhow::ensure!(
            metadata.len() <= MAX_TASK_RECORD_BYTES as u64,
            "Codex task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_TASK_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= MAX_TASK_RECORD_BYTES,
            "Codex task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let record: TaskRecord =
            serde_json::from_slice(&bytes).context("invalid Codex task record")?;
        validate_record(&record)?;
        anyhow::ensure!(record.task_id == task_id, "Codex task record ID mismatch");
        Ok(record)
    }

    fn prune_locked(&self, current: &TaskRecord) -> Result<()> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("cannot list Codex task store"),
        };
        let now = config::unix_time();
        let mut scoped = Vec::new();
        let mut count = 0usize;
        for entry in entries {
            count += 1;
            anyhow::ensure!(
                count <= MAX_TASK_DIRECTORY_ENTRIES,
                "Codex task store contains more than {MAX_TASK_DIRECTORY_ENTRIES} entries"
            );
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Ok(id) = Uuid::parse_str(stem) else {
                continue;
            };
            let Ok(record) = self.read_record(id) else {
                continue;
            };
            let expired = now.saturating_sub(record.updated_at) >= TASK_RETENTION_SECONDS;
            if expired && id != current.task_id {
                let _ = std::fs::remove_file(entry.path());
                continue;
            }
            if record.owner == current.owner && record.scope_cwd == current.scope_cwd {
                scoped.push((record.updated_at, id, entry.path()));
            }
        }
        if scoped.len() >= MAX_TASKS_PER_SCOPE {
            scoped.sort_by_key(|(updated, id, _)| (*updated, *id));
            let excess = scoped.len() + 1 - MAX_TASKS_PER_SCOPE;
            for (_, id, path) in scoped.into_iter().take(excess) {
                if id != current.task_id {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        Ok(())
    }
}

struct TemporaryFile {
    path: PathBuf,
    armed: bool,
}

impl TemporaryFile {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn validate_store_directory(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "Codex task store must be a real directory: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "Codex task store must be owner-only (mode {mode:04o})"
        );
    }
    Ok(())
}

fn validate_private_regular_file(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "Codex task path is not a regular file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(mode & 0o077 == 0, "Codex task file must be owner-only");
    }
    Ok(())
}

fn reject_symlink_target(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "Codex task path may not be a symlink"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect Codex task path"),
    }
    Ok(())
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

fn is_missing_task(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().starts_with("CODEX_TASK_NOT_FOUND:"))
}

fn ensure_task_owner(record: &TaskRecord, session: &config::Session) -> Result<()> {
    let scope = config::canonical_directory(&session.cwd)?;
    anyhow::ensure!(
        record.owner.matches(session) && record.scope_cwd == scope,
        "CODEX_TASK_NOT_FOUND: task was not found"
    );
    Ok(())
}

fn validate_record(record: &TaskRecord) -> Result<()> {
    anyhow::ensure!(
        record.schema_version == TASK_SCHEMA_VERSION,
        "unsupported Codex task schema version"
    );
    config::validate_session_id(&record.owner.id)?;
    let canonical = config::canonical_directory(&record.scope_cwd)?;
    anyhow::ensure!(
        canonical == record.scope_cwd,
        "Codex task scope is not canonical"
    );
    validate_argument(&record.model, "model")?;
    validate_argument(&record.effort, "effort")?;
    anyhow::ensure!(record.revision > 0, "Codex task revision must be positive");
    anyhow::ensure!(
        record.operations.len() <= MAX_OPERATION_HISTORY,
        "Codex task operation history exceeds limit"
    );
    if let Some(usage) = &record.usage {
        anyhow::ensure!(
            usage.len() <= TOKEN_USAGE_FIELDS.len()
                && usage
                    .keys()
                    .all(|key| TOKEN_USAGE_FIELDS.contains(&key.as_str())),
            "Codex task usage has unsupported fields"
        );
    }
    Ok(())
}

fn validate_argument(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= MAX_ARGUMENT_BYTES && !value.contains('\0'),
        "{label} must contain 1..={MAX_ARGUMENT_BYTES} NUL-free UTF-8 bytes"
    );
    Ok(())
}

fn validate_task_input(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= MAX_TASK_INPUT_BYTES && !value.contains('\0'),
        "{label} must contain 1..={MAX_TASK_INPUT_BYTES} NUL-free UTF-8 bytes"
    );
    Ok(())
}

#[cfg(unix)]
fn append_scope_identity(bytes: &mut Vec<u8>, scope: &Path) {
    bytes.extend_from_slice(scope.as_os_str().as_bytes());
}

#[cfg(not(unix))]
fn append_scope_identity(bytes: &mut Vec<u8>, scope: &Path) {
    bytes.extend_from_slice(scope.to_string_lossy().as_bytes());
}

fn task_id_for_operation(session: &config::Session, operation_id: Uuid) -> Result<Uuid> {
    let scope = config::canonical_directory(&session.cwd)?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(session.id.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&session.started_at.to_le_bytes());
    bytes.extend_from_slice(&session.process_id.to_le_bytes());
    append_scope_identity(&mut bytes, &scope);
    bytes.push(0);
    bytes.extend_from_slice(operation_id.as_bytes());
    Ok(Uuid::new_v5(&TASK_ID_NAMESPACE, &bytes))
}

fn fingerprint(value: &Value) -> Result<Uuid> {
    let bytes = serde_json::to_vec(value)?;
    Ok(Uuid::new_v5(&REQUEST_FINGERPRINT_NAMESPACE, &bytes))
}

fn task_view(record: &TaskRecord, evidence_ref: Option<&evidence::EvidenceRef>) -> Value {
    json!({
        "task_id": record.task_id,
        "status": record.status.as_str(),
        "revision": record.revision,
        "generation": record.generation,
        "model": record.model,
        "effort": record.effort,
        "thread_id": record.thread_id,
        "turn_id": record.turn_id,
        "usage": record.usage,
        "reconciliation_required": record.status == TaskStatus::ReconciliationRequired,
        "evidence": evidence_ref,
        "retention_seconds": TASK_RETENTION_SECONDS,
    })
}

fn operation_view(task_id: Uuid, outcome: &OperationOutcome) -> Value {
    json!({
        "task_id": task_id,
        "status": outcome.status.as_str(),
        "revision": outcome.revision,
        "generation": outcome.generation,
        "thread_id": outcome.thread_id,
        "turn_id": outcome.turn_id,
        "reconciliation_required": outcome.status == TaskStatus::ReconciliationRequired,
    })
}

#[derive(Clone)]
struct RpcClient {
    tx: mpsc::Sender<ClientCommand>,
}

enum ClientCommand {
    Request {
        method: &'static str,
        params: Value,
        reply: oneshot::Sender<std::result::Result<Value, String>>,
    },
    Notify {
        method: &'static str,
        params: Option<Value>,
    },
    Shutdown,
}

struct ServerResponse {
    id: Value,
    payload: std::result::Result<Value, (i64, String)>,
}

impl RpcClient {
    async fn request(&self, method: &'static str, params: Value) -> Result<Value> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ClientCommand::Request {
                method,
                params,
                reply: reply_tx,
            })
            .await
            .context("Codex app-server actor stopped")?;
        let result = tokio::time::timeout(RPC_TIMEOUT, reply_rx)
            .await
            .with_context(|| format!("Codex app-server request timed out: {method}"))?
            .context("Codex app-server actor dropped request")?;
        result.map_err(anyhow::Error::msg)
    }

    async fn notify(&self, method: &'static str, params: Option<Value>) -> Result<()> {
        self.tx
            .send(ClientCommand::Notify { method, params })
            .await
            .context("Codex app-server actor stopped")
    }

    async fn shutdown(&self) {
        let _ = self.tx.send(ClientCommand::Shutdown).await;
    }
}

#[derive(Clone)]
struct RuntimeHandle {
    client: RpcClient,
    owner: SessionInstance,
    scope: PathBuf,
    started_at: Instant,
}

fn runtimes() -> &'static Mutex<HashMap<Uuid, RuntimeHandle>> {
    static RUNTIMES: OnceLock<Mutex<HashMap<Uuid, RuntimeHandle>>> = OnceLock::new();
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runtime_for(session: &config::Session, task_id: Uuid) -> Option<RuntimeHandle> {
    let mut state = runtimes().lock().unwrap();
    let now = Instant::now();
    state.retain(|_, runtime| now.saturating_duration_since(runtime.started_at) < CHILD_LIFETIME);
    let runtime = state.get(&task_id)?.clone();
    let scope = config::canonical_directory(&session.cwd).ok()?;
    (runtime.owner.matches(session) && runtime.scope == scope).then_some(runtime)
}

fn insert_runtime(session: &config::Session, task_id: Uuid, client: RpcClient) -> Result<()> {
    let scope = config::canonical_directory(&session.cwd)?;
    let runtime = RuntimeHandle {
        client: client.clone(),
        owner: SessionInstance::from_session(session),
        scope,
        started_at: Instant::now(),
    };
    runtimes().lock().unwrap().insert(task_id, runtime);
    let session_id = session.id.clone();
    tokio::spawn(async move {
        let session_stopped = tokio::select! {
            _ = tokio::time::sleep(CHILD_LIFETIME) => false,
            _ = wait_for_session_stop(session_id.clone()) => true,
        };
        let runtime = { runtimes().lock().unwrap().remove(&task_id) };
        if let Some(runtime) = runtime {
            runtime.client.shutdown().await;
        }
        if session_stopped {
            evidence::remove_session(&session_id);
        }
    });
    Ok(())
}

async fn wait_for_session_stop(session_id: String) {
    loop {
        if let Ok(false) = config::session_is_active(&session_id).await {
            return;
        }
        tokio::time::sleep(SESSION_STOP_POLL).await;
    }
}

pub(crate) async fn remove_session(session_id: &str) {
    let clients = {
        let mut state = runtimes().lock().unwrap();
        let ids = state
            .iter()
            .filter_map(|(id, runtime)| (runtime.owner.id == session_id).then_some(*id))
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|id| state.remove(&id).map(|runtime| runtime.client))
            .collect::<Vec<_>>()
    };
    for client in clients {
        client.shutdown().await;
    }
    evidence::remove_session(session_id);
}

async fn spawn_initialized_client(
    session: &config::Session,
    task_id: Option<Uuid>,
) -> Result<(RpcClient, Value)> {
    spawn_initialized_client_with_binary(session, task_id, Path::new("codex")).await
}

async fn spawn_initialized_client_with_binary(
    session: &config::Session,
    task_id: Option<Uuid>,
    binary: &Path,
) -> Result<(RpcClient, Value)> {
    let client = spawn_client_with_binary(session.clone(), task_id, binary).await?;
    let initialized = async {
        let initialized = client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "temote-mcp",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "experimentalApi": true
                    }
                }),
            )
            .await?;
        validate_initialize_response(&initialized)?;
        client.notify("initialized", None).await?;
        Result::<Value>::Ok(initialized)
    }
    .await;
    match initialized {
        Ok(initialized) => Ok((client, initialized)),
        Err(error) => {
            client.shutdown().await;
            Err(error)
        }
    }
}

fn validate_initialize_response(value: &Value) -> Result<()> {
    let object = value
        .as_object()
        .context("Codex app-server initialize result must be an object")?;
    let user_agent = object
        .get("userAgent")
        .and_then(Value::as_str)
        .context("Codex app-server initialize result is missing userAgent")?;
    anyhow::ensure!(
        user_agent.split_whitespace().next().is_some_and(|prefix| {
            prefix == format!("codex_cli_rs/{SUPPORTED_APP_SERVER_VERSION}")
        }),
        "CODEX_APP_SERVER_INCOMPATIBLE: expected {SUPPORTED_APP_SERVER_VERSION}, got {user_agent}"
    );
    anyhow::ensure!(
        object.get("codexHome").and_then(Value::as_str).is_some(),
        "Codex app-server initialize result is missing codexHome"
    );
    Ok(())
}

async fn spawn_client_with_binary(
    session: config::Session,
    task_id: Option<Uuid>,
    binary: &Path,
) -> Result<RpcClient> {
    let mut command = Command::new(binary);
    command
        .arg("app-server")
        .arg("--stdio")
        .arg("-c")
        .arg("mcp_servers={}")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(&session.cwd)
        .kill_on_drop(true)
        .env_clear();
    for (key, value) in filtered_codex_environment(std::env::vars_os()) {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start Codex app-server from {}", binary.display()))?;
    let stdin = child
        .stdin
        .take()
        .context("Codex app-server stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("Codex app-server stdout unavailable")?;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(run_actor(child, stdin, stdout, session, task_id, rx));
    Ok(RpcClient { tx })
}

fn filtered_codex_environment<I>(environment: I) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    environment
        .into_iter()
        .filter(|(key, _)| codex_environment_key_allowed(key))
        .collect()
}

fn codex_environment_key_allowed(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    key.starts_with("LC_") || CODEX_CHILD_ENV_ALLOWLIST.contains(&key)
}

async fn run_actor(
    mut child: tokio::process::Child,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    session: config::Session,
    task_id: Option<Uuid>,
    mut commands: mpsc::Receiver<ClientCommand>,
) {
    let mut reader = BufReader::new(stdout);
    let (server_tx, mut server_rx) = mpsc::channel::<ServerResponse>(16);
    let mut next_id = 1u64;
    let mut pending = HashMap::<u64, oneshot::Sender<std::result::Result<Value, String>>>::new();
    let terminal_error = loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    break "client channel closed".to_owned();
                };
                match command {
                    ClientCommand::Request { method, params, reply } => {
                        let id = next_id;
                        next_id = next_id.saturating_add(1);
                        let message = json!({"id": id, "method": method, "params": params});
                        if let Err(error) = write_json_line(&mut stdin, &message).await {
                            let _ = reply.send(Err(format!("Codex app-server write failed: {error:#}")));
                            break format!("Codex app-server write failed: {error:#}");
                        }
                        pending.insert(id, reply);
                    }
                    ClientCommand::Notify { method, params } => {
                        let message = match params {
                            Some(params) => json!({"method": method, "params": params}),
                            None => json!({"method": method}),
                        };
                        if let Err(error) = write_json_line(&mut stdin, &message).await {
                            break format!("Codex app-server notification write failed: {error:#}");
                        }
                    }
                    ClientCommand::Shutdown => break "shutdown requested".to_owned(),
                }
            }
            response = server_rx.recv() => {
                if let Some(response) = response {
                    let message = match response.payload {
                        Ok(result) => json!({"id": response.id, "result": result}),
                        Err((code, message)) => json!({"id": response.id, "error": {"code": code, "message": message}}),
                    };
                    if let Err(error) = write_json_line(&mut stdin, &message).await {
                        break format!("Codex app-server server-response write failed: {error:#}");
                    }
                }
            }
            line = read_bounded_json_line(&mut reader) => {
                match line {
                    Ok(Some(value)) => {
                        if let Some(method) = value.get("method").and_then(Value::as_str) {
                            let method = method.to_owned();
                            if let Some(id) = value.get("id").cloned() {
                                let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
                                let tx = server_tx.clone();
                                let session = session.clone();
                                tokio::spawn(async move {
                                    let payload = handle_server_request(&session, task_id, &method, params).await;
                                    let _ = tx.send(ServerResponse { id, payload }).await;
                                });
                            } else {
                                handle_notification(&session, task_id, &method, value.get("params"));
                            }
                        } else if let Some(id) = value.get("id").and_then(Value::as_u64)
                            && let Some(reply) = pending.remove(&id)
                        {
                            if let Some(error) = value.get("error") {
                                let _ = reply.send(Err(format!("Codex app-server RPC error: {error}")));
                            } else if let Some(result) = value.get("result") {
                                let _ = reply.send(Ok(result.clone()));
                            } else {
                                let _ = reply.send(Err("Codex app-server response has neither result nor error".to_owned()));
                            }
                        }
                    }
                    Ok(None) => break "Codex app-server stdout closed".to_owned(),
                    Err(error) => break format!("Codex app-server protocol error: {error:#}"),
                }
            }
            status = child.wait() => {
                break match status {
                    Ok(status) => format!("Codex app-server exited: {status}"),
                    Err(error) => format!("Codex app-server wait failed: {error}"),
                };
            }
        }
    };

    for (_, reply) in pending {
        let _ = reply.send(Err(terminal_error.clone()));
    }
    let _ = child.kill().await;
}

async fn write_json_line(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        bytes.len() <= MAX_RPC_LINE_BYTES,
        "outbound Codex app-server message exceeds {MAX_RPC_LINE_BYTES} bytes"
    );
    stdin.write_all(&bytes).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_bounded_json_line<R>(reader: &mut R) -> Result<Option<Value>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
            anyhow::ensure!(
                line.len().saturating_add(newline) <= MAX_RPC_LINE_BYTES,
                "Codex app-server message exceeds {MAX_RPC_LINE_BYTES} bytes"
            );
            line.extend_from_slice(&chunk[..newline]);
            reader.consume(newline + 1);
            break;
        }
        anyhow::ensure!(
            line.len().saturating_add(chunk.len()) <= MAX_RPC_LINE_BYTES,
            "Codex app-server message exceeds {MAX_RPC_LINE_BYTES} bytes"
        );
        line.extend_from_slice(chunk);
        let len = chunk.len();
        reader.consume(len);
    }
    let value = serde_json::from_slice(&line).context("invalid Codex app-server JSON line")?;
    Ok(Some(value))
}

fn handle_notification(
    session: &config::Session,
    task_id: Option<Uuid>,
    method: &str,
    params: Option<&Value>,
) {
    let Some(task_id) = task_id else {
        return;
    };
    let Some(params) = params else {
        return;
    };
    let Some(thread_id) = notification_thread_id(params) else {
        return;
    };
    let turn_id = notification_turn_id(params);
    let usage = notification_usage(params);
    let status = match method {
        "turn/started" => Some(TaskStatus::Running),
        "turn/completed" => params
            .get("turn")
            .and_then(|turn| turn.get("status"))
            .or_else(|| params.get("status"))
            .and_then(Value::as_str)
            .and_then(task_status_from_str),
        "thread/tokenUsage/updated" => None,
        _ => return,
    };
    let Ok(store) = TaskStore::default_store() else {
        return;
    };
    let _ = store.update(session, task_id, |record| {
        anyhow::ensure!(
            record.thread_id.as_deref() == Some(thread_id),
            "Codex notification does not match task thread"
        );
        let mut changed = false;
        if let Some(turn_id) = turn_id.as_deref()
            && record.turn_id.as_deref() != Some(turn_id)
        {
            record.turn_id = Some(turn_id.to_owned());
            record.generation = record.generation.max(1);
            changed = true;
        }
        if let Some(status) = status
            && record.status != status
        {
            record.status = status;
            changed = true;
        }
        if usage.is_some() && record.usage != usage {
            record.usage = usage.clone();
            changed = true;
        }
        if changed {
            record.revision = record.revision.saturating_add(1);
        }
        Ok(())
    });
}

fn notification_thread_id(params: &Value) -> Option<&str> {
    params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("threadId"))
                .and_then(Value::as_str)
        })
}

fn notification_turn_id(params: &Value) -> Option<String> {
    params
        .get("turnId")
        .or_else(|| params.get("turn_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

fn notification_usage(params: &Value) -> Option<BTreeMap<String, u64>> {
    params
        .get("usage")
        .or_else(|| params.get("tokenUsage"))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("usage")))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("tokenUsage")))
        .and_then(extract_usage)
}

fn extract_usage(value: &Value) -> Option<BTreeMap<String, u64>> {
    let object = value.as_object()?;
    let mut usage = BTreeMap::new();
    for (name, aliases) in [
        ("input_tokens", ["input_tokens", "inputTokens"]),
        (
            "cached_input_tokens",
            ["cached_input_tokens", "cachedInputTokens"],
        ),
        ("output_tokens", ["output_tokens", "outputTokens"]),
        (
            "reasoning_output_tokens",
            ["reasoning_output_tokens", "reasoningOutputTokens"],
        ),
        ("total_tokens", ["total_tokens", "totalTokens"]),
    ] {
        if let Some(value) = aliases
            .iter()
            .find_map(|alias| object.get(*alias).and_then(Value::as_u64))
        {
            usage.insert(name.to_owned(), value);
        }
    }
    (!usage.is_empty()).then_some(usage)
}

async fn handle_server_request(
    session: &config::Session,
    task_id: Option<Uuid>,
    method: &str,
    params: Value,
) -> std::result::Result<Value, (i64, String)> {
    match method {
        "item/commandExecution/requestApproval" => {
            let task_id = task_id.ok_or_else(|| {
                (
                    -32601,
                    "approval request is unavailable outside a Codex task".to_owned(),
                )
            })?;
            validate_approval_task(session, task_id, &params)?;
            mark_waiting_approval(session, task_id, true);
            let allowed = if session.yolo {
                true
            } else {
                approvals::request(
                    &session.id,
                    "Codex command approval",
                    format!("Codex task {task_id} requested command execution"),
                    session.cwd.clone(),
                )
                .await
                .unwrap_or(false)
            };
            mark_waiting_approval(session, task_id, false);
            Ok(json!({"decision": if allowed { "accept" } else { "decline" }}))
        }
        "item/fileChange/requestApproval" => {
            let task_id = task_id.ok_or_else(|| {
                (
                    -32601,
                    "approval request is unavailable outside a Codex task".to_owned(),
                )
            })?;
            validate_approval_task(session, task_id, &params)?;
            mark_waiting_approval(session, task_id, true);
            let allowed = if session.yolo {
                true
            } else {
                approvals::request(
                    &session.id,
                    "Codex file-change approval",
                    format!("Codex task {task_id} requested file changes"),
                    session.cwd.clone(),
                )
                .await
                .unwrap_or(false)
            };
            mark_waiting_approval(session, task_id, false);
            Ok(json!({"decision": if allowed { "accept" } else { "decline" }}))
        }
        _ => Err((
            -32601,
            format!("unsupported Codex app-server request method: {method}"),
        )),
    }
}

fn validate_approval_task(
    session: &config::Session,
    task_id: Uuid,
    params: &Value,
) -> std::result::Result<(), (i64, String)> {
    let store = TaskStore::default_store().map_err(internal_server_error)?;
    bind_approval_turn(&store, session, task_id, params)
        .map_err(|error| (-32602, error.to_string()))
}

fn bind_approval_turn(
    store: &TaskStore,
    session: &config::Session,
    task_id: Uuid,
    params: &Value,
) -> Result<()> {
    let thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .context("approval request is missing threadId")?;
    let turn_id = params
        .get("turnId")
        .and_then(Value::as_str)
        .context("approval request is missing turnId")?;
    store.update(session, task_id, |record| {
        anyhow::ensure!(
            record.thread_id.as_deref() == Some(thread_id),
            "approval request does not match task thread"
        );
        match record.turn_id.as_deref() {
            Some(existing) => anyhow::ensure!(
                existing == turn_id,
                "approval request does not match task turn"
            ),
            None => {
                record.turn_id = Some(turn_id.to_owned());
                record.revision = record.revision.saturating_add(1);
            }
        }
        Ok(())
    })?;
    Ok(())
}

fn internal_server_error(error: anyhow::Error) -> (i64, String) {
    (-32603, error.to_string())
}

fn mark_waiting_approval(session: &config::Session, task_id: Uuid, waiting: bool) {
    if let Ok(store) = TaskStore::default_store() {
        let _ = store.update(session, task_id, |record| {
            record.status = if waiting {
                TaskStatus::WaitingApproval
            } else if matches!(record.status, TaskStatus::WaitingApproval) {
                TaskStatus::Running
            } else {
                record.status
            };
            record.revision = record.revision.saturating_add(1);
            Ok(())
        });
    }
}

fn validate_model_request(models: &Value, model: &str, effort: &str) -> Result<()> {
    let data = models
        .get("data")
        .and_then(Value::as_array)
        .context("Codex model/list response is missing data")?;
    let selected = data
        .iter()
        .find(|entry| entry.get("model").and_then(Value::as_str) == Some(model))
        .with_context(|| format!("Codex model is not advertised: {model}"))?;
    let efforts = selected
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .context("Codex model is missing supportedReasoningEfforts")?;
    anyhow::ensure!(
        efforts.iter().any(|entry| {
            entry.get("effort").and_then(Value::as_str) == Some(effort)
                || entry.as_str() == Some(effort)
        }),
        "Codex effort {effort} is not advertised for model {model}"
    );
    Ok(())
}

pub(crate) async fn status(session: &config::Session) -> Result<Value> {
    let (client, initialized) = spawn_initialized_client(session, None).await?;
    let models = match client
        .request("model/list", json!({"includeHidden": true}))
        .await
    {
        Ok(models) => models,
        Err(error) => {
            client.shutdown().await;
            return Err(error);
        }
    };
    client.shutdown().await;
    let data = models
        .get("data")
        .and_then(Value::as_array)
        .context("Codex model/list response is missing data")?;
    let advertised = data
        .iter()
        .filter_map(|entry| {
            let model = entry.get("model")?.as_str()?;
            let efforts = entry
                .get("supportedReasoningEfforts")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            item.get("effort")
                                .and_then(Value::as_str)
                                .or_else(|| item.as_str())
                                .map(str::to_owned)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(json!({"model": model, "efforts": efforts}))
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "compatible": true,
        "app_server_version": SUPPORTED_APP_SERVER_VERSION,
        "platform_family": initialized.get("platformFamily"),
        "platform_os": initialized.get("platformOs"),
        "models": advertised,
    }))
}

pub(crate) async fn task_start(args: &Value, session: &config::Session) -> Result<Value> {
    let operation_id = required_uuid(args, "operation_id")?;
    let task = required_string(args, "task")?;
    let model = required_string(args, "model")?;
    let effort = required_string(args, "effort")?;
    validate_task_input(task, "task")?;
    validate_argument(model, "model")?;
    validate_argument(effort, "effort")?;

    let task_id = task_id_for_operation(session, operation_id)?;
    let request_fingerprint = fingerprint(&json!({
        "kind": "start",
        "task_id": task_id,
        "task": task,
        "model": model,
        "effort": effort,
    }))?;
    let store = TaskStore::default_store()?;

    match store.load(session, task_id) {
        Ok(existing) => return replay_operation(&existing, operation_id, request_fingerprint),
        Err(error) if is_missing_task(&error) => {}
        Err(error) => return Err(error),
    }

    let now = config::unix_time();
    let mut record = TaskRecord {
        schema_version: TASK_SCHEMA_VERSION,
        task_id,
        owner: SessionInstance::from_session(session),
        scope_cwd: config::canonical_directory(&session.cwd)?,
        model: model.to_owned(),
        effort: effort.to_owned(),
        status: TaskStatus::Accepted,
        revision: 1,
        generation: 0,
        thread_id: None,
        turn_id: None,
        usage: None,
        created_at: now,
        updated_at: now,
        operations: Vec::new(),
    };
    record.operations.push(OperationReceipt {
        operation_id,
        request_fingerprint,
        action: "start".to_owned(),
        phase: OperationPhase::Accepted,
        outcome: record.outcome(),
    });
    store.save(&record)?;

    let (client, _) = match spawn_initialized_client(session, Some(task_id)).await {
        Ok(client) => client,
        Err(_) => {
            let record = store.update(session, task_id, |record| {
                record.status = TaskStatus::ReconciliationRequired;
                record.revision = record.revision.saturating_add(1);
                Ok(())
            })?;
            return Ok(task_view(&record, None));
        }
    };
    let models = match client
        .request("model/list", json!({"includeHidden": true}))
        .await
    {
        Ok(models) => models,
        Err(_) => {
            client.shutdown().await;
            let record = store.update(session, task_id, |record| {
                record.status = TaskStatus::Failed;
                record.revision = record.revision.saturating_add(1);
                let outcome = record.outcome();
                if let Some(receipt) = record
                    .operations
                    .iter_mut()
                    .find(|receipt| receipt.operation_id == operation_id)
                {
                    receipt.phase = OperationPhase::Applied;
                    receipt.outcome = outcome;
                }
                Ok(())
            })?;
            return Ok(task_view(&record, None));
        }
    };
    if validate_model_request(&models, model, effort).is_err() {
        client.shutdown().await;
        let record = store.update(session, task_id, |record| {
            record.status = TaskStatus::Failed;
            record.revision = record.revision.saturating_add(1);
            let outcome = record.outcome();
            if let Some(receipt) = record
                .operations
                .iter_mut()
                .find(|receipt| receipt.operation_id == operation_id)
            {
                receipt.phase = OperationPhase::Applied;
                receipt.outcome = outcome;
            }
            Ok(())
        })?;
        return Ok(task_view(&record, None));
    }

    let start_result = async {
        let cwd = record.scope_cwd.to_string_lossy().into_owned();
        let thread = client
            .request(
                "thread/start",
                json!({
                    "cwd": cwd,
                    "model": model,
                    "approvalPolicy": if session.yolo { "never" } else { "on-request" },
                    "approvalsReviewer": "user",
                    "sandbox": "workspaceWrite",
                    "runtimeWorkspaceRoots": [record.scope_cwd],
                    "ephemeral": false,
                    "threadSource": "temote-mcp",
                }),
            )
            .await?;
        let thread_id = thread
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .context("thread/start response is missing thread.id")?
            .to_owned();
        record = store.update(session, task_id, |record| {
            anyhow::ensure!(record.thread_id.is_none(), "task thread was already bound");
            record.thread_id = Some(thread_id.clone());
            record.revision = record.revision.saturating_add(1);
            Ok(())
        })?;

        let turn = client
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": task}],
                    "model": model,
                    "effort": effort,
                    "cwd": cwd,
                    "approvalPolicy": if session.yolo { "never" } else { "on-request" },
                    "sandboxPolicy": {
                        "type": "workspaceWrite",
                        "writableRoots": [record.scope_cwd],
                        "networkAccess": false,
                    },
                }),
            )
            .await?;
        let turn_id = turn
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .context("turn/start response is missing turn.id")?
            .to_owned();
        record = store.update(session, task_id, |record| {
            anyhow::ensure!(
                record.thread_id.as_deref() == Some(thread_id.as_str()),
                "turn/start returned for an unexpected task thread"
            );
            if let Some(existing) = record.turn_id.as_deref() {
                anyhow::ensure!(
                    existing == turn_id,
                    "turn/start returned an unexpected turn id"
                );
            } else {
                record.turn_id = Some(turn_id.clone());
            }
            if matches!(
                record.status,
                TaskStatus::Accepted | TaskStatus::Unknown | TaskStatus::ReconciliationRequired
            ) {
                record.status = TaskStatus::Running;
            }
            record.generation = 1;
            record.revision = record.revision.saturating_add(1);
            let outcome = record.outcome();
            if let Some(receipt) = record.operations.last_mut() {
                receipt.phase = OperationPhase::Applied;
                receipt.outcome = outcome;
            }
            Ok(())
        })?;
        Result::<()>::Ok(())
    }
    .await;

    if let Err(_error) = start_result {
        client.shutdown().await;
        let record = store.update(session, task_id, |record| {
            record.status = TaskStatus::ReconciliationRequired;
            record.revision = record.revision.saturating_add(1);
            Ok(())
        })?;
        return Ok(task_view(&record, None));
    }

    insert_runtime(session, task_id, client)?;
    Ok(task_view(&record, None))
}

fn replay_operation(record: &TaskRecord, operation_id: Uuid, fingerprint: Uuid) -> Result<Value> {
    let receipt = record
        .operations
        .iter()
        .find(|receipt| receipt.operation_id == operation_id)
        .context("OPERATION_CONFLICT: task exists without the requested operation receipt")?;
    anyhow::ensure!(
        receipt.request_fingerprint == fingerprint,
        "OPERATION_CONFLICT: operation_id was already accepted with a different request"
    );
    if receipt.phase == OperationPhase::Accepted {
        let mut outcome = receipt.outcome.clone();
        outcome.status = TaskStatus::ReconciliationRequired;
        return Ok(operation_view(record.task_id, &outcome));
    }
    Ok(operation_view(record.task_id, &receipt.outcome))
}

pub(crate) async fn task_get(args: &Value, session: &config::Session) -> Result<Value> {
    let task_id = required_uuid(args, "task_id")?;
    let after_revision = optional_u64(args, "after_revision")?;
    let store = TaskStore::default_store()?;
    let mut record = store.load(session, task_id)?;
    if after_revision == Some(record.revision) && record.status != TaskStatus::Accepted {
        return Ok(json!({
            "task_id": task_id,
            "status": "not_modified",
            "revision": record.revision,
        }));
    }
    if record.thread_id.is_none() {
        if record.status == TaskStatus::Accepted {
            record = store.update(session, task_id, |record| {
                record.status = TaskStatus::ReconciliationRequired;
                record.revision = record.revision.saturating_add(1);
                Ok(())
            })?;
        }
        return Ok(task_view(&record, None));
    }

    let client = match ensure_runtime(session, &record).await {
        Ok(client) => client,
        Err(_) => {
            record = store.update(session, task_id, |record| {
                record.status = TaskStatus::Unknown;
                record.revision = record.revision.saturating_add(1);
                Ok(())
            })?;
            return Ok(task_view(&record, None));
        }
    };
    let thread_id = record.thread_id.clone().unwrap();
    let response = client
        .request(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        )
        .await;
    let response = match response {
        Ok(response) => response,
        Err(_) => {
            let record = store.update(session, task_id, |record| {
                record.status = TaskStatus::Unknown;
                record.revision = record.revision.saturating_add(1);
                Ok(())
            })?;
            return Ok(task_view(&record, None));
        }
    };
    let evidence_ref =
        evidence::store(&session.id, &session.cwd, serde_json::to_string(&response)?)
            .ok()
            .flatten();
    let derived = match derive_thread_state(&response, record.turn_id.as_deref()) {
        Ok(derived) => derived,
        Err(_) => {
            record = store.update(session, task_id, |record| {
                record.status = TaskStatus::Unknown;
                record.revision = record.revision.saturating_add(1);
                Ok(())
            })?;
            return Ok(task_view(&record, evidence_ref.as_ref()));
        }
    };
    let changed = record.status != derived.status
        || record.turn_id != derived.turn_id
        || (derived.usage.is_some() && record.usage != derived.usage);
    if changed || (record.generation == 0 && derived.turn_id.is_some()) {
        record = store.update(session, task_id, |record| {
            record.status = derived.status;
            if derived.turn_id.is_some() {
                record.turn_id = derived.turn_id.clone();
                if record.generation == 0 {
                    record.generation = 1;
                }
            }
            if derived.usage.is_some() {
                record.usage = derived.usage.clone();
            }
            record.revision = record.revision.saturating_add(1);
            let outcome = record.outcome();
            if derived.turn_id.is_some()
                && let Some(receipt) = record.operations.iter_mut().find(|receipt| {
                    receipt.action == "start" && receipt.phase == OperationPhase::Accepted
                })
            {
                receipt.phase = OperationPhase::Applied;
                receipt.outcome = outcome;
            }
            Ok(())
        })?;
    }
    if after_revision == Some(record.revision) {
        return Ok(json!({
            "task_id": task_id,
            "status": "not_modified",
            "revision": record.revision,
        }));
    }
    Ok(task_view(&record, evidence_ref.as_ref()))
}

struct DerivedThreadState {
    status: TaskStatus,
    turn_id: Option<String>,
    usage: Option<BTreeMap<String, u64>>,
}

fn derive_thread_state(
    response: &Value,
    expected_turn_id: Option<&str>,
) -> Result<DerivedThreadState> {
    let thread = response
        .get("thread")
        .and_then(Value::as_object)
        .context("thread/read response is missing thread")?;
    let turns = thread
        .get("turns")
        .and_then(Value::as_array)
        .context("thread/read response is missing turns")?;
    let turn = expected_turn_id
        .and_then(|id| {
            turns
                .iter()
                .find(|turn| turn.get("id").and_then(Value::as_str) == Some(id))
        })
        .or_else(|| turns.last());
    let usage = thread
        .get("tokenUsage")
        .or_else(|| thread.get("usage"))
        .and_then(extract_usage)
        .or_else(|| turn.and_then(extract_usage_from_turn));
    let Some(turn) = turn else {
        let thread_status = thread
            .get("status")
            .and_then(|status| status.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Ok(DerivedThreadState {
            status: match thread_status {
                "idle" => TaskStatus::Unknown,
                "active" => TaskStatus::Running,
                "systemError" => TaskStatus::Failed,
                _ => TaskStatus::Unknown,
            },
            turn_id: None,
            usage,
        });
    };
    let turn_id = turn.get("id").and_then(Value::as_str).map(str::to_owned);
    let status = turn
        .get("status")
        .and_then(Value::as_str)
        .and_then(task_status_from_str)
        .unwrap_or(TaskStatus::Unknown);
    Ok(DerivedThreadState {
        status,
        turn_id,
        usage,
    })
}

fn task_status_from_str(status: &str) -> Option<TaskStatus> {
    match status {
        "completed" => Some(TaskStatus::Completed),
        "interrupted" => Some(TaskStatus::Interrupted),
        "failed" => Some(TaskStatus::Failed),
        "inProgress" | "active" => Some(TaskStatus::Running),
        "waitingApproval" => Some(TaskStatus::WaitingApproval),
        _ => None,
    }
}

fn extract_usage_from_turn(turn: &Value) -> Option<BTreeMap<String, u64>> {
    turn.get("usage")
        .or_else(|| turn.get("tokenUsage"))
        .and_then(extract_usage)
}

async fn ensure_runtime(session: &config::Session, record: &TaskRecord) -> Result<RpcClient> {
    if let Some(runtime) = runtime_for(session, record.task_id) {
        return Ok(runtime.client);
    }
    let (client, _) = spawn_initialized_client(session, Some(record.task_id)).await?;
    let thread_id = record
        .thread_id
        .as_deref()
        .context("cannot resume Codex task without thread_id")?;
    let resume = client
        .request(
            "thread/resume",
            json!({
                "threadId": thread_id,
                "cwd": record.scope_cwd,
                "model": record.model,
                "approvalPolicy": if session.yolo { "never" } else { "on-request" },
                "approvalsReviewer": "user",
                "sandbox": "workspaceWrite",
                "runtimeWorkspaceRoots": [record.scope_cwd],
                "excludeTurns": true,
            }),
        )
        .await;
    if let Err(error) = resume {
        client.shutdown().await;
        return Err(error).context("Codex task could not resume its retained thread");
    }
    insert_runtime(session, record.task_id, client.clone())?;
    Ok(client)
}

pub(crate) async fn task_control(args: &Value, session: &config::Session) -> Result<Value> {
    let task_id = required_uuid(args, "task_id")?;
    let operation_id = required_uuid(args, "operation_id")?;
    let action = required_string(args, "action")?;
    anyhow::ensure!(
        matches!(action, "steer" | "resume" | "interrupt"),
        "unsupported Codex task action"
    );
    let input = args.get("input").and_then(Value::as_str);
    match action {
        "steer" => validate_task_input(input.context("steer requires input")?, "input")?,
        "resume" | "interrupt" => {
            anyhow::ensure!(input.is_none(), "{action} does not accept input")
        }
        _ => unreachable!(),
    }

    let store = TaskStore::default_store()?;
    let mut record = store.load(session, task_id)?;
    let request_fingerprint = fingerprint(&json!({
        "kind": "control",
        "task_id": task_id,
        "action": action,
        "input": input,
    }))?;
    if let Some(receipt) = record
        .operations
        .iter()
        .find(|receipt| receipt.operation_id == operation_id)
    {
        anyhow::ensure!(
            receipt.request_fingerprint == request_fingerprint,
            "OPERATION_CONFLICT: operation_id was already accepted with a different request"
        );
        if receipt.phase == OperationPhase::Accepted {
            let mut outcome = receipt.outcome.clone();
            outcome.status = TaskStatus::ReconciliationRequired;
            return Ok(operation_view(task_id, &outcome));
        }
        return Ok(operation_view(task_id, &receipt.outcome));
    }
    if action == "resume" {
        anyhow::ensure!(
            matches!(
                record.status,
                TaskStatus::Unknown | TaskStatus::ReconciliationRequired
            ),
            "Codex task does not require resume reconciliation"
        );
    } else {
        anyhow::ensure!(
            matches!(
                record.status,
                TaskStatus::Running | TaskStatus::WaitingApproval
            ),
            "Codex task is not active and cannot be controlled"
        );
    }
    let thread_id = record
        .thread_id
        .clone()
        .context("Codex task requires reconciliation before control")?;
    let turn_id = record.turn_id.clone();
    if action != "resume" {
        anyhow::ensure!(
            turn_id.is_some(),
            "Codex task requires reconciliation before control"
        );
    }

    record.revision = record.revision.saturating_add(1);
    let accepted_outcome = record.outcome();
    record.operations.push(OperationReceipt {
        operation_id,
        request_fingerprint,
        action: action.to_owned(),
        phase: OperationPhase::Accepted,
        outcome: accepted_outcome,
    });
    if record.operations.len() > MAX_OPERATION_HISTORY {
        let excess = record.operations.len() - MAX_OPERATION_HISTORY;
        record.operations.drain(..excess);
    }
    store.save(&record)?;

    let client = match ensure_runtime(session, &record).await {
        Ok(client) => client,
        Err(_) => {
            let record = store.update(session, task_id, |record| {
                record.status = TaskStatus::ReconciliationRequired;
                record.revision = record.revision.saturating_add(1);
                Ok(())
            })?;
            return Ok(task_view(&record, None));
        }
    };
    let result = match action {
        "steer" => {
            client
                .request(
                    "turn/steer",
                    json!({
                    "threadId": thread_id,
                        "expectedTurnId": turn_id.as_deref().unwrap_or_default(),
                        "input": [{"type": "text", "text": input.unwrap()}],
                    }),
                )
                .await
        }
        "interrupt" => client
            .request(
                "turn/interrupt",
                json!({"threadId": thread_id, "turnId": turn_id.as_deref().unwrap_or_default()}),
            )
            .await,
        "resume" => {
            client
                .request(
                    "thread/read",
                    json!({"threadId": thread_id, "includeTurns": true}),
                )
                .await
        }
        _ => unreachable!(),
    };
    if result.is_err() {
        let record = store.update(session, task_id, |record| {
            record.status = TaskStatus::ReconciliationRequired;
            record.revision = record.revision.saturating_add(1);
            Ok(())
        })?;
        return Ok(task_view(&record, None));
    }

    let resumed = if action == "resume" {
        let response = match &result {
            Ok(response) => response,
            Err(_) => unreachable!("resume errors return above"),
        };
        match derive_thread_state(response, record.turn_id.as_deref()) {
            Ok(state) => Some(state),
            Err(_) => {
                let record = store.update(session, task_id, |record| {
                    record.status = TaskStatus::ReconciliationRequired;
                    record.revision = record.revision.saturating_add(1);
                    Ok(())
                })?;
                return Ok(task_view(&record, None));
            }
        }
    } else {
        None
    };
    record = store.update(session, task_id, |record| {
        if let Some(resumed) = resumed.as_ref() {
            record.status = resumed.status;
            if resumed.turn_id.is_some() {
                record.turn_id = resumed.turn_id.clone();
                record.generation = record.generation.max(1);
            }
            if resumed.usage.is_some() {
                record.usage = resumed.usage.clone();
            }
        } else {
            record.generation = record.generation.saturating_add(1);
            record.status = if action == "interrupt" {
                TaskStatus::Interrupted
            } else {
                TaskStatus::Running
            };
        }
        record.revision = record.revision.saturating_add(1);
        let outcome = record.outcome();
        if let Some(receipt) = record
            .operations
            .iter_mut()
            .find(|receipt| receipt.operation_id == operation_id)
        {
            receipt.phase = OperationPhase::Applied;
            receipt.outcome = outcome;
        }
        Ok(())
    })?;
    Ok(task_view(&record, None))
}

fn required_uuid(args: &Value, key: &str) -> Result<Uuid> {
    let value = required_string(args, key)?;
    Uuid::parse_str(value).with_context(|| format!("{key} must be a UUID"))
}

fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing or invalid {key}"))
}

fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>> {
    args.get(key)
        .map(|value| {
            value
                .as_u64()
                .with_context(|| format!("{key} must be a non-negative integer"))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn session(root: &Path, id: &str, yolo: bool) -> config::Session {
        let cwd = config::canonical_directory(root).unwrap();
        config::Session {
            id: id.to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 1234,
            process_id: 5678,
            yolo,
        }
    }

    fn fake_app_server(root: &Path, mode: &str) -> PathBuf {
        let path = root.join(format!("fake-app-server-{mode}"));
        let script = r##"#!/usr/bin/env python3
import json, os, sys
mode = os.path.basename(sys.argv[0]).split('fake-app-server-', 1)[-1]
thread_id = '0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa'
turn_id = '0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb'
for raw in sys.stdin:
    req = json.loads(raw)
    if req.get('method') == 'initialized':
        continue
    i = req.get('id')
    method = req.get('method')
    if method == 'initialize':
        result = {'userAgent':'codex_cli_rs/0.153.4 (temote-mcp; test)','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'macos'}
    elif method == 'model/list':
        result = {'data':[{'model':'gpt-5.6-luna','id':'luna','displayName':'Luna','description':'test','hidden':False,'isDefault':True,'defaultReasoningEffort':'high','supportedReasoningEfforts':[{'effort':'low'},{'effort':'medium'},{'effort':'high'},{'effort':'max'},{'effort':'xhigh'}]}]}
    elif method == 'thread/start':
        result = {'thread':{'id':thread_id}}
    elif method == 'turn/start':
        if mode == 'approval':
            print(json.dumps({'id':'approval-1','method':'item/commandExecution/requestApproval','params':{'itemId':'item','startedAtMs':1,'threadId':thread_id,'turnId':turn_id}}), flush=True)
            approval = json.loads(sys.stdin.readline())
            if approval.get('result',{}).get('decision') != 'accept':
                print(json.dumps({'id':i,'error':{'code':-1,'message':'denied'}}), flush=True)
                continue
        result = {'turn':{'id':turn_id}}
    elif method == 'thread/resume':
        result = {'thread':{'id':thread_id}}
    elif method == 'thread/read':
        result = {'thread':{'id':thread_id,'status':{'type':'idle'},'tokenUsage':{'inputTokens':4,'cachedInputTokens':1,'outputTokens':2,'reasoningOutputTokens':1,'totalTokens':6},'turns':[{'id':turn_id,'status':'completed','items':[{'type':'agentMessage','id':'m','text':'secret transcript marker'}]}]}}
    elif method == 'turn/steer':
        result = {'turnId':turn_id}
    elif method == 'turn/interrupt':
        result = {}
    else:
        print(json.dumps({'id':i,'error':{'code':-32601,'message':'unsupported'}}), flush=True)
        continue
    print(json.dumps({'id':i,'result':result}), flush=True)
"##;
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn task_store_is_scope_and_full_session_instance_bound_and_prompt_free() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner = session(root.path(), "owner", true);
        let operation_id = Uuid::new_v4();
        let task_id = task_id_for_operation(&owner, operation_id).unwrap();
        let now = config::unix_time();
        let record = TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(&owner),
            scope_cwd: owner.cwd.clone(),
            model: "gpt-5.6-luna".to_owned(),
            effort: "max".to_owned(),
            status: TaskStatus::Accepted,
            revision: 1,
            generation: 0,
            thread_id: None,
            turn_id: None,
            usage: None,
            created_at: now,
            updated_at: now,
            operations: vec![OperationReceipt {
                operation_id,
                request_fingerprint: fingerprint(&json!({"task":"prompt-secret-marker"})).unwrap(),
                action: "start".to_owned(),
                phase: OperationPhase::Accepted,
                outcome: OperationOutcome {
                    status: TaskStatus::Accepted,
                    revision: 1,
                    generation: 0,
                    thread_id: None,
                    turn_id: None,
                },
            }],
        };
        store.save(&record).unwrap();
        let bytes = std::fs::read(store.path(task_id)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("prompt-secret-marker"));
        assert_eq!(store.load(&owner, task_id).unwrap(), record);

        let mut restarted = owner.clone();
        restarted.process_id += 1;
        assert!(store.load(&restarted, task_id).is_err());
        let other_root = tempfile::tempdir().unwrap();
        let other = session(other_root.path(), "owner", true);
        assert!(store.load(&other, task_id).is_err());
    }

    #[test]
    fn accepted_operation_replay_requires_reconciliation_and_conflicts_on_change() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "replay", true);
        let operation_id = Uuid::new_v4();
        let task_id = task_id_for_operation(&session, operation_id).unwrap();
        let fp = fingerprint(&json!({"same":true})).unwrap();
        let now = config::unix_time();
        let record = TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(&session),
            scope_cwd: session.cwd.clone(),
            model: "gpt-5.6-luna".to_owned(),
            effort: "max".to_owned(),
            status: TaskStatus::Accepted,
            revision: 1,
            generation: 0,
            thread_id: None,
            turn_id: None,
            usage: None,
            created_at: now,
            updated_at: now,
            operations: vec![OperationReceipt {
                operation_id,
                request_fingerprint: fp,
                action: "start".to_owned(),
                phase: OperationPhase::Accepted,
                outcome: OperationOutcome {
                    status: TaskStatus::Accepted,
                    revision: 1,
                    generation: 0,
                    thread_id: None,
                    turn_id: None,
                },
            }],
        };
        let replay = replay_operation(&record, operation_id, fp).unwrap();
        assert_eq!(replay["status"], "reconciliation_required");
        assert!(replay_operation(&record, operation_id, Uuid::new_v4()).is_err());
    }

    #[test]
    fn task_start_only_treats_the_explicit_missing_marker_as_absence() {
        assert!(is_missing_task(&anyhow::anyhow!(
            "CODEX_TASK_NOT_FOUND: task was not found"
        )));
        assert!(!is_missing_task(&anyhow::anyhow!(
            "invalid Codex task record"
        )));
    }

    #[tokio::test]
    async fn fake_app_server_handshake_start_read_steer_interrupt_and_evidence() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "fake", true);
        let binary = fake_app_server(root.path(), "ok");
        let task_id = Uuid::new_v4();
        let (client, initialized) =
            spawn_initialized_client_with_binary(&session, Some(task_id), &binary)
                .await
                .unwrap();
        assert_eq!(initialized["platformOs"], "macos");
        let models = client
            .request("model/list", json!({"includeHidden":true}))
            .await
            .unwrap();
        validate_model_request(&models, "gpt-5.6-luna", "max").unwrap();
        let thread = client
            .request(
                "thread/start",
                json!({"cwd":session.cwd,"sandbox":"workspaceWrite"}),
            )
            .await
            .unwrap();
        let thread_id = thread["thread"]["id"].as_str().unwrap().to_owned();
        let turn = client
            .request(
                "turn/start",
                json!({"threadId":thread_id,"input":[{"type":"text","text":"x"}]}),
            )
            .await
            .unwrap();
        let turn_id = turn["turn"]["id"].as_str().unwrap().to_owned();
        assert!(client.request("turn/steer", json!({"threadId":thread_id,"expectedTurnId":turn_id,"input":[{"type":"text","text":"y"}]})).await.is_ok());
        assert!(
            client
                .request(
                    "turn/interrupt",
                    json!({"threadId":thread_id,"turnId":turn_id})
                )
                .await
                .is_ok()
        );
        let read = client
            .request(
                "thread/read",
                json!({"threadId":thread_id,"includeTurns":true}),
            )
            .await
            .unwrap();
        let derived = derive_thread_state(&read, Some(&turn_id)).unwrap();
        assert_eq!(derived.status, TaskStatus::Completed);
        assert_eq!(derived.usage.as_ref().unwrap()["input_tokens"], 4);
        assert_eq!(derived.usage.as_ref().unwrap()["total_tokens"], 6);
        let reference = evidence::store(
            &session.id,
            &session.cwd,
            serde_json::to_string(&read).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(Uuid::parse_str(&reference.evidence_id).is_ok());
        client.shutdown().await;
    }

    #[tokio::test]
    async fn app_server_rejects_incompatible_and_oversized_protocol() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "protocol", true);
        let incompatible = root.path().join("fake-app-server-incompatible");
        std::fs::write(
            &incompatible,
            "#!/usr/bin/env python3\nimport json,sys\nfor line in sys.stdin:\n r=json.loads(line)\n if r.get('method')=='initialize': print(json.dumps({'id':r['id'],'result':{'userAgent':'temote-mcp/9.9.9','codexHome':'/tmp','platformFamily':'unix','platformOs':'macos'}}),flush=True)\n",
        )
        .unwrap();
        std::fs::set_permissions(&incompatible, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = spawn_initialized_client_with_binary(&session, None, &incompatible)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("CODEX_APP_SERVER_INCOMPATIBLE"));

        let oversized = root.path().join("fake-app-server-oversized");
        std::fs::write(
            &oversized,
            format!("#!/usr/bin/env python3\nimport sys,json\nfor line in sys.stdin:\n sys.stdout.write('x'*{}+'\\n');sys.stdout.flush()\n", MAX_RPC_LINE_BYTES + 1),
        )
        .unwrap();
        std::fs::set_permissions(&oversized, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = spawn_initialized_client_with_binary(&session, None, &oversized)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("message exceeds"));
    }

    #[test]
    fn codex_app_server_environment_filter_is_deliberate() {
        let filtered = filtered_codex_environment([
            (OsString::from("HOME"), OsString::from("/home/test")),
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (OsString::from("OPENAI_API_KEY"), OsString::from("secret")),
            (OsString::from("TEMOTE_SECRET"), OsString::from("no")),
            (
                OsString::from("AWS_SECRET_ACCESS_KEY"),
                OsString::from("no"),
            ),
        ]);
        let keys = filtered
            .into_iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(keys.contains(&"HOME".to_owned()));
        assert!(keys.contains(&"PATH".to_owned()));
        assert!(keys.contains(&"LC_ALL".to_owned()));
        assert!(keys.contains(&"OPENAI_API_KEY".to_owned()));
        assert!(!keys.contains(&"TEMOTE_SECRET".to_owned()));
        assert!(!keys.contains(&"AWS_SECRET_ACCESS_KEY".to_owned()));
    }
}
