//! Devin Cloud (API v3) task backend.
//!
//! Drives hosted Devin sessions over the public REST API instead of a local
//! `devin acp` child. The durable contract mirrors the other delegation
//! backends: scope-bound task records with idempotent operation receipts,
//! typed controls, bounded reports and scoped evidence. There is no runtime
//! lease because nothing runs locally; the remote session is the source of
//! truth and every get reconciles against it.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{config, evidence};

const TASK_SCHEMA_VERSION: u64 = 1;
const TASK_RETENTION_SECONDS: u64 = 24 * 60 * 60;
const MAX_TASK_RECORD_BYTES: usize = 64 * 1024;
const MAX_TASK_DIRECTORY_ENTRIES: usize = 4096;
const MAX_TASKS_PER_SCOPE: usize = 128;
const MAX_OPERATION_HISTORY: usize = 32;
const MAX_ARGUMENT_BYTES: usize = 256;
const MAX_REPOS: usize = 16;
const MAX_TASK_INPUT_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 1024;
const MAX_REPORT_BYTES: usize = 8 * 1024;
const MAX_REPORT_ARRAY_ITEMS: usize = 64;
const MAX_SUMMARY_CHARS: usize = 1200;
const MAX_PULL_REQUESTS: usize = 16;
const MAX_MESSAGE_TEXT_BYTES: usize = MAX_REPORT_BYTES * 4;
const MAX_EVIDENCE_MESSAGES: usize = 8;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_API_KEY_BYTES: usize = 512;
const MAX_ORG_ID_BYTES: usize = 128;
const MAX_BASE_URL_BYTES: usize = 512;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DEVIN_CATALOG_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DEVIN_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_DEVIN_CATALOG_UIDS: usize = 4096;
const MAX_DEVIN_CATALOG_DEPTH: usize = 16;

pub(crate) const DEFAULT_API_BASE_URL: &str = "https://api.devin.ai";
pub(crate) const API_KEY_ENV: &str = "TEMOTE_MCP_DEVIN_API_KEY";
pub(crate) const API_KEY_FALLBACK_ENV: &str = "DEVIN_API_KEY";
pub(crate) const ORG_ID_ENV: &str = "TEMOTE_MCP_DEVIN_ORG_ID";
pub(crate) const BASE_URL_ENV: &str = "TEMOTE_MCP_DEVIN_API_BASE_URL";
pub(crate) const CREATE_AS_USER_ID_ENV: &str = "TEMOTE_MCP_DEVIN_CREATE_AS_USER_ID";
const SESSION_TAG: &str = "temote-mcp";

const TASK_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x71, 0x2e, 0xb5, 0x0d, 0x4f, 0x63, 0x4a, 0x8c, 0x9d, 0x54, 0x1a, 0xe7, 0x36, 0xc2, 0x08, 0xb9,
]);
const REQUEST_FINGERPRINT_NAMESPACE: Uuid = Uuid::from_bytes([
    0xa4, 0x9c, 0x2f, 0x58, 0x7b, 0x11, 0x4e, 0x0a, 0x8f, 0x63, 0xd2, 0x95, 0x1c, 0x7e, 0x40, 0x36,
]);

const REPORT_INSTRUCTIONS: &str = r#"You are a delegated implementation worker running non-interactively. You must not ask interactive questions, and you must finish the task below before answering.

When finished, publish your final result as structured output matching the provided schema and also send it as your final message: ONLY one JSON object and nothing else, no markdown, no code fences, no text before or after the JSON.
The JSON object must contain exactly these fields:
{"status":"completed|failed|blocked|needs_decision","summary":"short summary, at most 1200 characters","base_commit":"","changed_files":[],"checks":[],"unresolved":[]}
Rules:
- All string values are plain strings; changed_files, checks, and unresolved are arrays of strings (use [] when empty).
- Do not include any other fields.

Task:
"#;

const RESUME_INSTRUCTIONS: &str = "Continue the task. When finished, publish the required JSON report object as structured output and as your final message, and nothing else.";

fn report_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": {"type": "string", "enum": ["completed", "failed", "blocked", "needs_decision"]},
            "summary": {"type": "string", "maxLength": MAX_SUMMARY_CHARS},
            "base_commit": {"type": "string"},
            "changed_files": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_REPORT_ARRAY_ITEMS},
            "checks": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_REPORT_ARRAY_ITEMS},
            "unresolved": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_REPORT_ARRAY_ITEMS},
        },
        "required": ["status", "summary"],
        "additionalProperties": false,
    })
}

// ---------- configuration ----------

/// Resolved Devin Cloud API configuration. The key is held only for the
/// duration of one tool call and is never serialized or rendered.
pub(crate) struct CloudConfig {
    api_key: String,
    api_key_source: &'static str,
    org_id: Option<String>,
    base_url: String,
    create_as_user_id: Option<String>,
}

/// Secret-free readiness view for `doctor` and `devin_cloud_status`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct CloudReadiness {
    pub api_key_source: &'static str,
    pub org_id_configured: bool,
    pub base_url: String,
    pub create_as_user_id_configured: bool,
}

impl CloudConfig {
    fn readiness(&self) -> CloudReadiness {
        CloudReadiness {
            api_key_source: self.api_key_source,
            org_id_configured: self.org_id.is_some(),
            base_url: self.base_url.clone(),
            create_as_user_id_configured: self.create_as_user_id.is_some(),
        }
    }
}

fn env_value(name: &str) -> Result<Option<String>, String> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid UTF-8")),
    }
}

pub(crate) fn resolve_config() -> Result<CloudConfig, String> {
    let (api_key, api_key_source) = match env_value(API_KEY_ENV)? {
        Some(value) => (value, API_KEY_ENV),
        None => match env_value(API_KEY_FALLBACK_ENV)? {
            Some(value) => (value, API_KEY_FALLBACK_ENV),
            None => {
                return Err(format!(
                    "Devin Cloud API key is not configured; set {API_KEY_ENV} (or {API_KEY_FALLBACK_ENV}) to a Devin service-user or personal API key"
                ));
            }
        },
    };
    let api_key = api_key.trim().to_owned();
    if api_key.is_empty() {
        return Err(format!("{api_key_source} is set but empty"));
    }
    if api_key.len() > MAX_API_KEY_BYTES || !api_key.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(format!(
            "{api_key_source} must be a printable ASCII token of at most {MAX_API_KEY_BYTES} bytes"
        ));
    }
    let org_id = match env_value(ORG_ID_ENV)? {
        None => None,
        Some(value) => {
            let value = value.trim().to_owned();
            if value.is_empty() {
                None
            } else {
                validate_org_id(&value).map_err(|error| format!("{ORG_ID_ENV}: {error}"))?;
                Some(value)
            }
        }
    };
    let base_url = match env_value(BASE_URL_ENV)? {
        None => DEFAULT_API_BASE_URL.to_owned(),
        Some(value) => validate_base_url(value.trim())?,
    };
    let create_as_user_id = match env_value(CREATE_AS_USER_ID_ENV)? {
        None => None,
        Some(value) => {
            let value = value.trim().to_owned();
            if value.is_empty() {
                None
            } else {
                validate_org_id(&value)
                    .map_err(|error| format!("{CREATE_AS_USER_ID_ENV}: {error}"))?;
                Some(value)
            }
        }
    };
    Ok(CloudConfig {
        api_key,
        api_key_source,
        org_id,
        base_url,
        create_as_user_id,
    })
}

pub(crate) fn readiness() -> Result<CloudReadiness, String> {
    resolve_config().map(|config| config.readiness())
}

fn validate_org_id(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= MAX_ORG_ID_BYTES
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "organization ID must be 1..={MAX_ORG_ID_BYTES} bytes of [A-Za-z0-9_-]"
    );
    Ok(())
}

fn validate_devin_session_id(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= MAX_ORG_ID_BYTES
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "Devin session ID must be 1..={MAX_ORG_ID_BYTES} bytes of [A-Za-z0-9_-]"
    );
    Ok(())
}

fn validate_base_url(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("{BASE_URL_ENV} is set but empty"));
    }
    if value.len() > MAX_BASE_URL_BYTES {
        return Err(format!("{BASE_URL_ENV} exceeds {MAX_BASE_URL_BYTES} bytes"));
    }
    let Some(rest) = value.strip_prefix("https://") else {
        return Err(format!("{BASE_URL_ENV} must start with https://"));
    };
    if rest.is_empty()
        || rest.contains(['?', '#', '@', ' '])
        || !rest.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(format!(
            "{BASE_URL_ENV} must be an https origin without credentials, query, or fragment"
        ));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

// ---------- API transport ----------

#[derive(Clone, Debug, PartialEq, Eq)]
enum ApiError {
    /// The request definitely did not reach the server (connect failure) or
    /// the server rejected it. Retrying is safe.
    Rejected(String),
    /// The request may have reached the server (timeout, read error): the
    /// remote side effect is unknown.
    Uncertain(String),
}

impl ApiError {
    fn message(&self) -> &str {
        match self {
            Self::Rejected(message) | Self::Uncertain(message) => message,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ApiError {}

type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Clone)]
enum CloudApi {
    Http(Arc<HttpApi>),
    #[cfg(test)]
    Fake(Arc<Mutex<FakeApi>>),
}

struct HttpApi {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl HttpApi {
    fn new(config: &CloudConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("temote-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("cannot build Devin Cloud HTTP client")?;
        Ok(Self {
            client,
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
        })
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> ApiResult<Value> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key)
            .header(reqwest::header::ACCEPT, "application/json");
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|error| {
            let message = bound_text(
                &format!("Devin Cloud request failed: {error}"),
                MAX_ERROR_BYTES,
            );
            if error.is_connect() || error.is_builder() || error.is_request() {
                ApiError::Rejected(message)
            } else {
                ApiError::Uncertain(message)
            }
        })?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|error| {
            ApiError::Uncertain(bound_text(
                &format!("Devin Cloud response read failed: {error}"),
                MAX_ERROR_BYTES,
            ))
        })?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(ApiError::Uncertain(format!(
                "Devin Cloud response exceeds {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&bytes);
            return Err(ApiError::Rejected(bound_text(
                &format!("Devin Cloud API returned HTTP {status}: {}", detail.trim()),
                MAX_ERROR_BYTES,
            )));
        }
        if bytes.is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            ApiError::Uncertain(format!("Devin Cloud {path} returned a non-JSON response"))
        })
    }
}

impl CloudApi {
    fn http(config: &CloudConfig) -> Result<Self> {
        Ok(Self::Http(Arc::new(HttpApi::new(config)?)))
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> ApiResult<Value> {
        match self {
            Self::Http(http) => http.send(method, path, query, body).await,
            #[cfg(test)]
            Self::Fake(fake) => fake.lock().unwrap().call(method, path, query, body),
        }
    }

    async fn get_self(&self) -> ApiResult<Value> {
        self.call(reqwest::Method::GET, "/v3/self", &[], None).await
    }

    async fn create_session(&self, org_id: &str, body: &Value) -> ApiResult<Value> {
        self.call(
            reqwest::Method::POST,
            &format!("/v3/organizations/{org_id}/sessions"),
            &[],
            Some(body),
        )
        .await
    }

    async fn get_session(&self, org_id: &str, devin_id: &str) -> ApiResult<Value> {
        self.call(
            reqwest::Method::GET,
            &format!("/v3/organizations/{org_id}/sessions/{devin_id}"),
            &[],
            None,
        )
        .await
    }

    async fn list_messages(&self, org_id: &str, devin_id: &str) -> ApiResult<Value> {
        self.call(
            reqwest::Method::GET,
            &format!("/v3/organizations/{org_id}/sessions/{devin_id}/messages"),
            &[("first", "100".to_owned())],
            None,
        )
        .await
    }

    async fn send_message(&self, org_id: &str, devin_id: &str, message: &str) -> ApiResult<Value> {
        self.call(
            reqwest::Method::POST,
            &format!("/v3/organizations/{org_id}/sessions/{devin_id}/messages"),
            &[],
            Some(&json!({"message": message})),
        )
        .await
    }

    async fn terminate_session(&self, org_id: &str, devin_id: &str) -> ApiResult<Value> {
        self.call(
            reqwest::Method::DELETE,
            &format!("/v3/organizations/{org_id}/sessions/{devin_id}"),
            &[("archive", "false".to_owned())],
            None,
        )
        .await
    }
}

async fn resolve_org_id(config: &CloudConfig, api: &CloudApi) -> Result<String> {
    if let Some(org_id) = &config.org_id {
        return Ok(org_id.clone());
    }
    let me = api
        .get_self()
        .await
        .context("cannot resolve Devin organization from /v3/self")?;
    let org_id = me.get("org_id").and_then(Value::as_str).with_context(|| {
        format!("Devin Cloud principal has no organization; set {ORG_ID_ENV} explicitly")
    })?;
    validate_org_id(org_id)?;
    Ok(org_id.to_owned())
}

// ---------- session-instance identity ----------

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
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

async fn ensure_current_active_instance(
    owner: &SessionInstance,
    session: &config::Session,
) -> Result<()> {
    anyhow::ensure!(
        owner.matches(session),
        "Devin Cloud operation session snapshot does not match its owner instance"
    );
    let current = config::read_session_metadata(&owner.id)
        .await
        .with_context(|| {
            format!(
                "cannot verify current Devin Cloud session instance {}",
                owner.id
            )
        })?;
    anyhow::ensure!(
        owner.matches(&current),
        "Devin Cloud session instance is no longer current"
    );
    anyhow::ensure!(
        config::session_is_active(&owner.id).await?,
        "Devin Cloud session instance is not active"
    );
    Ok(())
}

// ---------- durable task contract ----------

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TaskStatus {
    Accepted,
    Running,
    WaitingInput,
    WaitingApproval,
    RetryableFailed,
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
            Self::WaitingInput => "waiting_input",
            Self::WaitingApproval => "waiting_approval",
            Self::RetryableFailed => "retryable_failed",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::ReconciliationRequired => "reconciliation_required",
            Self::Unknown => "unknown",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OperationPhase {
    Accepted,
    Applied,
    RetryableFailed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct OperationOutcome {
    status: TaskStatus,
    revision: u64,
    generation: u64,
    devin_session_id: Option<String>,
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
struct OperationTombstone {
    operation_id: Uuid,
    request_fingerprint: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct TaskRecord {
    schema_version: u64,
    task_id: Uuid,
    owner: SessionInstance,
    scope_cwd: PathBuf,
    org_id: String,
    title: Option<String>,
    devin_mode: Option<String>,
    #[serde(default)]
    swe_tier: Option<String>,
    #[serde(default)]
    effective_devin_mode: Option<String>,
    #[serde(default)]
    repos: Vec<String>,
    status: TaskStatus,
    revision: u64,
    generation: u64,
    devin_session_id: Option<String>,
    #[serde(default)]
    session_url: Option<String>,
    #[serde(default)]
    remote_status: Option<String>,
    #[serde(default)]
    remote_status_detail: Option<String>,
    #[serde(default)]
    acus_consumed_milli: Option<u64>,
    #[serde(default)]
    pull_requests: Vec<String>,
    #[serde(default)]
    report: Option<Value>,
    #[serde(default)]
    last_error: Option<String>,
    created_at: u64,
    updated_at: u64,
    operations: Vec<OperationReceipt>,
    #[serde(default)]
    operation_tombstones: Vec<OperationTombstone>,
}

impl TaskRecord {
    fn outcome(&self) -> OperationOutcome {
        OperationOutcome {
            status: self.status,
            revision: self.revision,
            generation: self.generation,
            devin_session_id: self.devin_session_id.clone(),
        }
    }
}

fn update_operation_receipt(record: &mut TaskRecord, operation_id: Uuid, phase: OperationPhase) {
    let outcome = record.outcome();
    if let Some(receipt) = record
        .operations
        .iter_mut()
        .find(|receipt| receipt.operation_id == operation_id)
    {
        receipt.phase = phase;
        receipt.outcome = outcome;
    }
}

#[derive(Clone, Debug)]
struct TaskStore {
    directory: PathBuf,
}

enum StartAcceptance {
    Existing(TaskRecord),
    Accepted(TaskRecord),
}

enum ControlAcceptance {
    Replay(Value),
    Accepted(Box<TaskRecord>),
}

struct TaskStoreGuard {
    _process: MutexGuard<'static, ()>,
    file: File,
}

impl Drop for TaskStoreGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: file remains open for the lifetime of the lock.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn store_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

impl TaskStore {
    fn default_store() -> Result<Self> {
        Ok(Self {
            directory: config::state_dir()?.join("devin-cloud-tasks"),
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
                create_private_directory(&self.directory)?;
                let metadata = std::fs::symlink_metadata(&self.directory)?;
                validate_store_directory(&self.directory, &metadata)
            }
            Err(error) => Err(error).context("cannot inspect Devin Cloud task store"),
        }
    }

    fn lock(&self) -> Result<TaskStoreGuard> {
        let process = store_lock().lock().unwrap();
        self.ensure_directory()?;
        let path = self.directory.join(".store.lock");
        let file = open_private_lock_file(&path)?;
        #[cfg(unix)]
        {
            // SAFETY: flock is called with a valid open descriptor. The guard
            // keeps it open until the critical section ends.
            let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if status != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("cannot lock Devin Cloud task store");
            }
        }
        Ok(TaskStoreGuard {
            _process: process,
            file,
        })
    }

    fn path(&self, task_id: Uuid) -> PathBuf {
        self.directory.join(format!("{task_id}.json"))
    }

    fn load(&self, session: &config::Session, task_id: Uuid) -> Result<TaskRecord> {
        let _guard = self.lock()?;
        self.load_locked(session, task_id)
    }

    fn existing_start(
        &self,
        session: &config::Session,
        task_id: Uuid,
    ) -> Result<Option<TaskRecord>> {
        let _guard = self.lock()?;
        match self.read_record(task_id) {
            Ok(record) => {
                ensure_task_owner(&record, session)?;
                Ok(Some(record))
            }
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn load_locked(&self, session: &config::Session, task_id: Uuid) -> Result<TaskRecord> {
        let record = self.read_record(task_id).map_err(|error| {
            if is_not_found(&error) {
                anyhow::anyhow!("DEVIN_TASK_NOT_FOUND: task was not found")
            } else {
                error
            }
        })?;
        ensure_task_owner(&record, session)?;
        Ok(record)
    }

    #[cfg(test)]
    fn save(&self, record: &TaskRecord) -> Result<()> {
        let _guard = self.lock()?;
        self.save_locked(record)
    }

    fn save_locked(&self, record: &TaskRecord) -> Result<()> {
        self.ensure_directory()?;
        validate_record(record)?;
        self.prune_locked(record)?;
        let bytes = serde_json::to_vec_pretty(record)?;
        anyhow::ensure!(
            bytes.len() <= MAX_TASK_RECORD_BYTES,
            "Devin Cloud task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
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
        let _guard = self.lock()?;
        let mut record = self.load_locked(session, task_id)?;
        f(&mut record)?;
        record.updated_at = config::unix_time();
        self.save_locked(&record)?;
        Ok(record)
    }

    fn accept_start(
        &self,
        session: &config::Session,
        record: TaskRecord,
    ) -> Result<StartAcceptance> {
        let _guard = self.lock()?;
        match self.read_record(record.task_id) {
            Ok(existing) => {
                ensure_task_owner(&existing, session)?;
                Ok(StartAcceptance::Existing(existing))
            }
            Err(error) if is_not_found(&error) => {
                self.save_locked(&record)?;
                Ok(StartAcceptance::Accepted(record))
            }
            Err(error) => Err(error),
        }
    }

    fn accept_control(
        &self,
        session: &config::Session,
        task_id: Uuid,
        operation_id: Uuid,
        request_fingerprint: Uuid,
        action: &str,
    ) -> Result<ControlAcceptance> {
        let _guard = self.lock()?;
        let mut record = self.load_locked(session, task_id)?;
        if record
            .operations
            .iter()
            .any(|receipt| receipt.operation_id == operation_id)
            || record
                .operation_tombstones
                .iter()
                .any(|tombstone| tombstone.operation_id == operation_id)
        {
            return Ok(ControlAcceptance::Replay(replay_operation(
                &record,
                operation_id,
                request_fingerprint,
            )?));
        }
        anyhow::ensure!(
            !record.status.is_terminal(),
            "TASK_TERMINAL: task {task_id} is already {}",
            record.status.as_str()
        );
        record.revision = record.revision.saturating_add(1);
        record.updated_at = config::unix_time();
        compact_operations(&mut record);
        let outcome = record.outcome();
        record.operations.push(OperationReceipt {
            operation_id,
            request_fingerprint,
            action: action.to_owned(),
            phase: OperationPhase::Accepted,
            outcome,
        });
        self.save_locked(&record)?;
        Ok(ControlAcceptance::Accepted(Box::new(record)))
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
            "Devin Cloud task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_TASK_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= MAX_TASK_RECORD_BYTES,
            "Devin Cloud task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let record: TaskRecord =
            serde_json::from_slice(&bytes).context("invalid Devin Cloud task record")?;
        validate_record(&record)?;
        anyhow::ensure!(
            record.task_id == task_id,
            "Devin Cloud task record ID mismatch"
        );
        Ok(record)
    }

    fn prune_locked(&self, current: &TaskRecord) -> Result<()> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("cannot list Devin Cloud task store"),
        };
        let now = config::unix_time();
        let mut scoped = Vec::new();
        let mut count = 0usize;
        for entry in entries {
            count += 1;
            anyhow::ensure!(
                count <= MAX_TASK_DIRECTORY_ENTRIES,
                "Devin Cloud task store contains more than {MAX_TASK_DIRECTORY_ENTRIES} entries"
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
                std::fs::remove_file(entry.path())
                    .with_context(|| format!("cannot prune expired Devin Cloud task {id}"))?;
                continue;
            }
            if record.owner == current.owner && record.scope_cwd == current.scope_cwd {
                scoped.push(id);
            }
        }
        let current_is_persisted = scoped.contains(&current.task_id);
        let projected = scoped.len() + usize::from(!current_is_persisted);
        if projected > MAX_TASKS_PER_SCOPE {
            anyhow::bail!(
                "Devin Cloud task scope has reached its retention limit; refusing to accept another task"
            );
        }
        Ok(())
    }

    /// Read-only projection of every record owned by `session`'s full
    /// instance and canonical scope. Unreadable record files count as
    /// `skipped` so a partial projection stays visible; a missing store
    /// directory is an empty list, not an error.
    fn list_owned(&self, session: &config::Session) -> Result<(Vec<TaskRecord>, usize)> {
        let _guard = self.lock()?;
        let scope = config::canonical_directory(&session.cwd)?;
        let metadata = match std::fs::symlink_metadata(&self.directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), 0));
            }
            Err(error) => return Err(error).context("cannot inspect Devin Cloud task store"),
        };
        validate_store_directory(&self.directory, &metadata)?;
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), 0));
            }
            Err(error) => return Err(error).context("cannot list Devin Cloud task store"),
        };
        let mut records = Vec::new();
        let mut skipped = 0usize;
        let mut count = 0usize;
        for entry in entries {
            count += 1;
            anyhow::ensure!(
                count <= MAX_TASK_DIRECTORY_ENTRIES,
                "Devin Cloud task store contains more than {MAX_TASK_DIRECTORY_ENTRIES} entries"
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
            match self.read_record(id) {
                Ok(record) => {
                    if record.owner.matches(session) && record.scope_cwd == scope {
                        records.push(record);
                    }
                }
                Err(error) if is_not_found(&error) => continue,
                Err(_) => skipped += 1,
            }
        }
        Ok((records, skipped))
    }
}

fn compact_operations(record: &mut TaskRecord) {
    while record.operations.len() >= MAX_OPERATION_HISTORY {
        let removed = record.operations.remove(0);
        record.operation_tombstones.push(OperationTombstone {
            operation_id: removed.operation_id,
            request_fingerprint: removed.request_fingerprint,
        });
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
        "Devin Cloud task store must be a real directory: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "Devin Cloud task store must be owner-only (mode {mode:04o})"
        );
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("cannot create private directory {}", path.display())),
    }
}

fn open_private_lock_file(path: &Path) -> Result<File> {
    reject_symlink_target(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(path)
        .with_context(|| format!("cannot open private lock file {}", path.display()))?;
    #[cfg(unix)]
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let metadata = file.metadata()?;
    validate_private_regular_file(path, &metadata)?;
    Ok(file)
}

fn validate_private_regular_file(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "Devin Cloud task path is not a regular file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "Devin Cloud task file must be owner-only"
        );
    }
    Ok(())
}

fn reject_symlink_target(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "Devin Cloud task path may not be a symlink"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect Devin Cloud task path"),
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

fn ensure_task_owner(record: &TaskRecord, session: &config::Session) -> Result<()> {
    let scope = config::canonical_directory(&session.cwd)?;
    anyhow::ensure!(
        record.owner.matches(session) && record.scope_cwd == scope,
        "DEVIN_TASK_NOT_FOUND: task was not found"
    );
    Ok(())
}

fn validate_record(record: &TaskRecord) -> Result<()> {
    anyhow::ensure!(
        record.schema_version == TASK_SCHEMA_VERSION,
        "unsupported Devin Cloud task schema version"
    );
    config::validate_session_id(&record.owner.id)?;
    let canonical = config::canonical_directory(&record.scope_cwd)?;
    anyhow::ensure!(
        canonical == record.scope_cwd,
        "Devin Cloud task scope is not canonical"
    );
    validate_org_id(&record.org_id)?;
    if let Some(devin_session_id) = &record.devin_session_id {
        validate_devin_session_id(devin_session_id)?;
    }
    if let Some(title) = &record.title {
        validate_argument(title, "title")?;
    }
    if let Some(mode) = &record.devin_mode {
        validate_devin_mode(mode)?;
    }
    validate_swe_tier(record.swe_tier.as_deref(), record.devin_mode.as_deref())?;
    if let Some(effective) = &record.effective_devin_mode {
        validate_argument(effective, "effective_devin_mode")?;
        if record.swe_tier.as_deref() == Some("priority") {
            let requested = record
                .devin_mode
                .as_deref()
                .context("priority SWE-2 record is missing devin_mode")?;
            anyhow::ensure!(
                is_swe2_priority_uid(effective, requested),
                "priority SWE-2 record has an invalid effective Devin mode"
            );
        } else if let Some(requested) = record.devin_mode.as_deref() {
            anyhow::ensure!(
                effective == requested,
                "non-priority Devin Cloud record changed its effective mode"
            );
        }
    }
    anyhow::ensure!(record.repos.len() <= MAX_REPOS, "too many repos");
    for repo in &record.repos {
        validate_argument(repo, "repos")?;
    }
    if let Some(url) = &record.session_url {
        anyhow::ensure!(url.len() <= MAX_BASE_URL_BYTES, "session_url too long");
    }
    if let Some(error) = &record.last_error {
        anyhow::ensure!(
            error.len() <= MAX_ERROR_BYTES,
            "Devin Cloud task error field exceeds {MAX_ERROR_BYTES} bytes"
        );
    }
    if let Some(report) = &record.report {
        let bytes = serde_json::to_vec(report)?;
        anyhow::ensure!(
            bytes.len() <= MAX_REPORT_BYTES,
            "Devin Cloud task report exceeds {MAX_REPORT_BYTES} bytes"
        );
    }
    anyhow::ensure!(
        record.pull_requests.len() <= MAX_PULL_REQUESTS
            && record
                .pull_requests
                .iter()
                .all(|url| url.len() <= MAX_BASE_URL_BYTES),
        "Devin Cloud task pull request list exceeds limits"
    );
    anyhow::ensure!(
        record.revision > 0,
        "Devin Cloud task revision must be positive"
    );
    anyhow::ensure!(
        record.operations.len() <= MAX_OPERATION_HISTORY,
        "Devin Cloud task operation history exceeds limit"
    );
    let mut operation_ids = record
        .operations
        .iter()
        .map(|receipt| receipt.operation_id)
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        operation_ids.len() == record.operations.len(),
        "Devin Cloud task operation history contains a duplicate operation_id"
    );
    for tombstone in &record.operation_tombstones {
        anyhow::ensure!(
            operation_ids.insert(tombstone.operation_id),
            "Devin Cloud task operation history contains a duplicate operation_id"
        );
    }
    Ok(())
}

fn validate_argument(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= MAX_ARGUMENT_BYTES
            && !value.chars().any(char::is_control),
        "{label} must contain 1..={MAX_ARGUMENT_BYTES} control-free UTF-8 bytes"
    );
    Ok(())
}

fn validate_devin_mode(value: &str) -> Result<()> {
    anyhow::ensure!(
        matches!(
            value,
            "normal"
                | "fast"
                | "lite"
                | "ultra"
                | "fusion"
                | "swe-2-medium"
                | "swe-2-high"
                | "swe-2-max"
        ),
        "devin_mode must be one of normal, fast, lite, ultra, fusion, swe-2-medium, swe-2-high, swe-2-max"
    );
    Ok(())
}

fn is_swe2_mode(value: &str) -> bool {
    matches!(value, "swe-2-medium" | "swe-2-high" | "swe-2-max")
}

fn validate_swe_tier(tier: Option<&str>, devin_mode: Option<&str>) -> Result<()> {
    let Some(tier) = tier else {
        return Ok(());
    };
    anyhow::ensure!(
        matches!(tier, "promo" | "priority"),
        "swe_tier must be one of promo, priority"
    );
    anyhow::ensure!(
        devin_mode.is_some_and(is_swe2_mode),
        "swe_tier requires devin_mode to be one of swe-2-medium, swe-2-high, swe-2-max"
    );
    Ok(())
}

fn is_swe2_priority_uid(candidate: &str, requested_mode: &str) -> bool {
    let Some(effort) = requested_mode.strip_prefix("swe-2-") else {
        return false;
    };
    let normalized = candidate.to_ascii_lowercase().replace('_', "-");
    let tokens = normalized.split('-').collect::<Vec<_>>();
    tokens.starts_with(&["swe", "2"])
        && tokens.contains(&effort)
        && (tokens.contains(&"priority") || tokens.contains(&"fast"))
}

fn collect_model_uids(value: &Value, depth: usize, out: &mut BTreeSet<String>) -> Result<()> {
    anyhow::ensure!(
        depth <= MAX_DEVIN_CATALOG_DEPTH,
        "Devin model catalog exceeds nesting limit"
    );
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key == "model_uid" {
                    let uid = child
                        .as_str()
                        .context("Devin model catalog model_uid must be a string")?;
                    validate_argument(uid, "model_uid")?;
                    out.insert(uid.to_owned());
                    anyhow::ensure!(
                        out.len() <= MAX_DEVIN_CATALOG_UIDS,
                        "Devin model catalog contains too many model UIDs"
                    );
                }
                collect_model_uids(child, depth + 1, out)?;
            }
        }
        Value::Array(items) => {
            anyhow::ensure!(
                items.len() <= MAX_DEVIN_CATALOG_UIDS,
                "Devin model catalog array exceeds item limit"
            );
            for item in items {
                collect_model_uids(item, depth + 1, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn resolve_swe2_priority_uid_from_catalog(catalog: &Value, requested_mode: &str) -> Result<String> {
    anyhow::ensure!(
        is_swe2_mode(requested_mode),
        "priority resolution requires a SWE-2 devin_mode"
    );
    let mut uids = BTreeSet::new();
    collect_model_uids(catalog, 0, &mut uids)?;
    let matches = uids
        .into_iter()
        .filter(|uid| is_swe2_priority_uid(uid, requested_mode))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [uid] => Ok(uid.clone()),
        [] => anyhow::bail!(
            "SWE-2 priority is unavailable: Devin model catalog exposes no selectable priority/fast UID for {requested_mode}"
        ),
        _ => anyhow::bail!(
            "SWE-2 priority is ambiguous: Devin model catalog exposes multiple priority/fast UIDs for {requested_mode}"
        ),
    }
}

async fn read_devin_model_catalog() -> Result<Value> {
    let binary = crate::devin_acp::resolve_devin_executable().map_err(anyhow::Error::msg)?;
    let future = async move {
        let mut child = Command::new(&binary)
            .args(["models", "list", "--format", "json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("could not start {} for Devin model discovery", binary.display()))?;
        let stdout = child
            .stdout
            .take()
            .context("Devin model discovery stdout unavailable")?;
        let read_stdout = async move {
            let mut bytes = Vec::new();
            stdout
                .take((MAX_DEVIN_CATALOG_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .await
                .context("could not read Devin model catalog")?;
            Ok::<Vec<u8>, anyhow::Error>(bytes)
        };
        let (bytes, status) = tokio::try_join!(read_stdout, child.wait())?;
        anyhow::ensure!(
            bytes.len() <= MAX_DEVIN_CATALOG_BYTES,
            "Devin model catalog exceeds {MAX_DEVIN_CATALOG_BYTES} bytes"
        );
        anyhow::ensure!(
            status.success(),
            "Devin model discovery failed; run devin models list --format json interactively to verify login and connectivity"
        );
        serde_json::from_slice(&bytes).context("Devin model catalog is not valid JSON")
    };
    timeout(DEVIN_CATALOG_TIMEOUT, future)
        .await
        .context("Devin model discovery timed out")?
}

#[cfg(test)]
static FAKE_SWE_PRIORITY_UID: OnceLock<Mutex<Option<Result<String, String>>>> = OnceLock::new();

#[cfg(test)]
fn set_fake_swe_priority_uid(value: Option<Result<String, String>>) {
    *FAKE_SWE_PRIORITY_UID
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap() = value;
}

async fn resolve_swe2_priority_uid(requested_mode: &str) -> Result<String> {
    #[cfg(test)]
    if let Some(result) = FAKE_SWE_PRIORITY_UID
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .clone()
    {
        return result.map_err(anyhow::Error::msg);
    }

    let catalog = read_devin_model_catalog().await?;
    resolve_swe2_priority_uid_from_catalog(&catalog, requested_mode)
}

async fn resolve_effective_devin_mode(
    devin_mode: Option<&str>,
    swe_tier: Option<&str>,
) -> Result<Option<String>> {
    match (devin_mode, swe_tier) {
        (Some(mode), Some("priority")) => resolve_swe2_priority_uid(mode).await.map(Some),
        (Some(mode), _) => Ok(Some(mode.to_owned())),
        (None, None) => Ok(None),
        (None, Some(_)) => anyhow::bail!(
            "swe_tier requires devin_mode to be one of swe-2-medium, swe-2-high, swe-2-max"
        ),
    }
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
    use std::os::unix::ffi::OsStrExt;
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

fn operation_view(task_id: Uuid, outcome: &OperationOutcome) -> Value {
    json!({
        "task_id": task_id,
        "status": outcome.status.as_str(),
        "revision": outcome.revision,
        "generation": outcome.generation,
        "devin_session_id": outcome.devin_session_id,
        "reconciliation_required": outcome.status == TaskStatus::ReconciliationRequired,
    })
}

fn task_view(record: &TaskRecord, evidence_ref: Option<&evidence::EvidenceRef>) -> Value {
    json!({
        "task_id": record.task_id,
        "backend": "devin_cloud",
        "status": record.status.as_str(),
        "revision": record.revision,
        "generation": record.generation,
        "org_id": record.org_id,
        "title": record.title,
        "devin_mode": record.devin_mode,
        "swe_tier": record.swe_tier,
        "effective_devin_mode": record.effective_devin_mode,
        "repos": record.repos,
        "devin_session_id": record.devin_session_id,
        "session_url": record.session_url,
        "remote_status": record.remote_status,
        "remote_status_detail": record.remote_status_detail,
        "acus_consumed_milli": record.acus_consumed_milli,
        "pull_requests": record.pull_requests,
        "report": record.report,
        "last_error": record.last_error,
        "reconciliation_required": record.status == TaskStatus::ReconciliationRequired,
        "evidence": evidence_ref,
        "retention_seconds": TASK_RETENTION_SECONDS,
    })
}

fn not_modified_view(record: &TaskRecord) -> Value {
    json!({
        "task_id": record.task_id,
        "status": "not_modified",
        "revision": record.revision,
    })
}

fn bound_text(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

// ---------- report extraction ----------

fn extract_report(text: &str) -> Option<Value> {
    let mut best: Option<Value> = None;
    for (index, _) in text.match_indices('{') {
        let Some(value) = parse_balanced_json(&text[index..]) else {
            continue;
        };
        if report_shape_valid(&value) {
            best = Some(value);
        }
    }
    best
}

fn parse_balanced_json(text: &str) -> Option<Value> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return serde_json::from_str(&text[..=offset]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

fn report_shape_valid(report: &Value) -> bool {
    let Some(object) = report.as_object() else {
        return false;
    };
    let status_ok = object
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| {
            matches!(
                status,
                "completed" | "failed" | "blocked" | "needs_decision"
            )
        });
    if !status_ok {
        return false;
    }
    if object
        .get("summary")
        .and_then(Value::as_str)
        .is_none_or(|summary| summary.chars().count() > MAX_SUMMARY_CHARS)
    {
        return false;
    }
    for key in ["changed_files", "checks", "unresolved"] {
        match object.get(key) {
            Some(Value::Array(items))
                if items.len() <= MAX_REPORT_ARRAY_ITEMS
                    && items.iter().all(|item| item.is_string()) => {}
            None => {}
            _ => return false,
        }
    }
    serde_json::to_vec(report)
        .map(|bytes| bytes.len() <= MAX_REPORT_BYTES)
        .unwrap_or(false)
}

// ---------- remote state mapping ----------

#[derive(Clone, Debug, PartialEq)]
struct RemoteSession {
    devin_session_id: String,
    url: Option<String>,
    status: String,
    status_detail: Option<String>,
    acus_consumed_milli: Option<u64>,
    pull_requests: Vec<String>,
    structured_output: Option<Value>,
}

fn parse_remote_session(value: &Value) -> Result<RemoteSession> {
    let devin_session_id = value
        .get("session_id")
        .and_then(Value::as_str)
        .context("Devin Cloud session response is missing session_id")?
        .to_owned();
    validate_devin_session_id(&devin_session_id)?;
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .context("Devin Cloud session response is missing status")?;
    Ok(RemoteSession {
        devin_session_id,
        url: value
            .get("url")
            .and_then(Value::as_str)
            .map(|url| bound_text(url, MAX_BASE_URL_BYTES - 4)),
        status: bound_text(status, 64),
        status_detail: value
            .get("status_detail")
            .and_then(Value::as_str)
            .map(|detail| bound_text(detail, 64)),
        acus_consumed_milli: value
            .get("acus_consumed")
            .and_then(Value::as_f64)
            .filter(|acus| acus.is_finite() && *acus >= 0.0)
            .map(|acus| (acus * 1000.0).round() as u64),
        pull_requests: value
            .get("pull_requests")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("pr_url").and_then(Value::as_str))
                    .take(MAX_PULL_REQUESTS)
                    .map(|url| bound_text(url, MAX_BASE_URL_BYTES - 4))
                    .collect()
            })
            .unwrap_or_default(),
        structured_output: value
            .get("structured_output")
            .filter(|output| output.is_object())
            .cloned(),
    })
}

#[derive(Clone, Debug, PartialEq)]
struct DerivedState {
    status: TaskStatus,
    report: Option<Value>,
    last_error: Option<String>,
    needs_messages: bool,
}

/// Map the remote session status onto the task contract. `needs_messages`
/// marks finished sessions whose structured output did not validate, so the
/// caller can fall back to the final Devin message text.
fn derive_state(remote: &RemoteSession, current: TaskStatus) -> DerivedState {
    let detail = remote.status_detail.as_deref();
    let structured_report = remote
        .structured_output
        .as_ref()
        .filter(|output| report_shape_valid(output))
        .cloned();
    let finished = |report: Option<Value>| DerivedState {
        status: TaskStatus::Completed,
        needs_messages: report.is_none(),
        last_error: report
            .is_none()
            .then(|| "Devin session finished without a valid report".to_owned()),
        report,
    };
    match remote.status.as_str() {
        "new" | "claimed" | "resuming" => DerivedState {
            status: TaskStatus::Running,
            report: None,
            last_error: None,
            needs_messages: false,
        },
        "running" => match detail {
            Some("waiting_for_user") => {
                // Devin idles after finishing a turn instead of exiting, so a
                // terminal structured report wins over the waiting state.
                let terminal = structured_report.as_ref().and_then(report_task_status);
                let needs_messages = structured_report.is_none();
                DerivedState {
                    status: terminal.unwrap_or(TaskStatus::WaitingInput),
                    report: structured_report,
                    last_error: match terminal {
                        None | Some(TaskStatus::WaitingInput) => {
                            Some("Devin session is waiting for user input".to_owned())
                        }
                        Some(TaskStatus::Completed) => None,
                        _ => Some("Devin session reported failure".to_owned()),
                    },
                    needs_messages,
                }
            }
            Some("waiting_for_approval") => DerivedState {
                status: TaskStatus::WaitingApproval,
                report: None,
                last_error: Some("Devin session is waiting for action approval".to_owned()),
                needs_messages: false,
            },
            Some("finished") => finished(structured_report),
            _ => DerivedState {
                status: TaskStatus::Running,
                report: None,
                last_error: None,
                needs_messages: false,
            },
        },
        "exit" => finished(structured_report),
        "error" => DerivedState {
            status: TaskStatus::Failed,
            report: structured_report,
            last_error: Some("Devin session ended with an error".to_owned()),
            needs_messages: false,
        },
        "suspended" => match detail {
            Some("user_request") if current == TaskStatus::Interrupted => DerivedState {
                status: TaskStatus::Interrupted,
                report: None,
                last_error: None,
                needs_messages: false,
            },
            Some("user_request") => DerivedState {
                status: TaskStatus::Interrupted,
                report: structured_report,
                last_error: Some("Devin session was stopped by user request".to_owned()),
                needs_messages: false,
            },
            Some("inactivity") | None => {
                if structured_report.is_some() {
                    finished(structured_report)
                } else {
                    DerivedState {
                        status: TaskStatus::WaitingInput,
                        report: None,
                        last_error: Some(
                            "Devin session is suspended for inactivity; resume or steer to continue"
                                .to_owned(),
                        ),
                        needs_messages: true,
                    }
                }
            }
            Some(reason) => DerivedState {
                status: TaskStatus::Failed,
                report: structured_report,
                last_error: Some(bound_text(
                    &format!("Devin session is suspended: {reason}"),
                    MAX_ERROR_BYTES,
                )),
                needs_messages: false,
            },
        },
        other => DerivedState {
            status: TaskStatus::Unknown,
            report: None,
            last_error: Some(bound_text(
                &format!("Devin session reported unknown status {other}"),
                MAX_ERROR_BYTES,
            )),
            needs_messages: false,
        },
    }
}

/// Terminal task outcome encoded in a validated report, if it declares one.
/// `blocked` and `needs_decision` stay steerable (waiting_input).
fn report_task_status(report: &Value) -> Option<TaskStatus> {
    match report.get("status").and_then(Value::as_str) {
        Some("completed") => Some(TaskStatus::Completed),
        Some("failed") => Some(TaskStatus::Failed),
        _ => None,
    }
}

fn devin_messages(value: &Value) -> Vec<String> {
    value
        .get("items")
        .or_else(|| value.get("messages"))
        .or_else(|| value.get("data"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("source").and_then(Value::as_str) == Some("devin"))
                .filter_map(|item| item.get("message").and_then(Value::as_str))
                .map(|text| bound_text(text, MAX_MESSAGE_TEXT_BYTES))
                .collect()
        })
        .unwrap_or_default()
}

async fn reconcile(
    session: &config::Session,
    owner: &SessionInstance,
    store: &TaskStore,
    api: &CloudApi,
    record: TaskRecord,
) -> Result<(TaskRecord, Option<evidence::EvidenceRef>)> {
    let Some(devin_session_id) = record.devin_session_id.clone() else {
        // Accepted but never bound: the create call failed or crashed before
        // the session ID was persisted. The task text is not retained, so this
        // cannot be re-driven safely.
        let record = store.update(session, record.task_id, |record| {
            if record.status.is_terminal() {
                return Ok(());
            }
            record.status = TaskStatus::ReconciliationRequired;
            record.revision = record.revision.saturating_add(1);
            let start_operation = record
                .operations
                .iter()
                .find(|receipt| receipt.action == "start")
                .map(|receipt| receipt.operation_id);
            if let Some(operation_id) = start_operation {
                update_operation_receipt(record, operation_id, OperationPhase::Accepted);
            }
            Ok(())
        })?;
        return Ok((record, None));
    };
    let remote = api
        .get_session(&record.org_id, &devin_session_id)
        .await
        .map_err(anyhow::Error::from)
        .and_then(|value| parse_remote_session(&value));
    let remote = match remote {
        Ok(remote) => remote,
        Err(error) => {
            let record = store.update(session, record.task_id, |record| {
                if record.status.is_terminal() {
                    return Ok(());
                }
                record.status = TaskStatus::Unknown;
                record.last_error = Some(bound_text(&format!("{error:#}"), MAX_ERROR_BYTES));
                record.revision = record.revision.saturating_add(1);
                Ok(())
            })?;
            return Ok((record, None));
        }
    };
    anyhow::ensure!(
        remote.devin_session_id == devin_session_id,
        "Devin Cloud returned a different session than requested"
    );
    let mut derived = derive_state(&remote, record.status);
    let mut final_messages = Vec::new();
    if (derived.needs_messages || derived.status.is_terminal())
        && let Ok(messages) = api.list_messages(&record.org_id, &devin_session_id).await
    {
        final_messages = devin_messages(&messages);
        if derived.report.is_none()
            && let Some(report) = final_messages
                .iter()
                .rev()
                .find_map(|text| extract_report(text))
        {
            derived.report = Some(report);
            if derived.status == TaskStatus::Completed {
                derived.last_error = None;
            }
        }
        if derived.status == TaskStatus::WaitingInput
            && let Some(status) = derived.report.as_ref().and_then(report_task_status)
        {
            derived.status = status;
            if status == TaskStatus::Completed {
                derived.last_error = None;
            }
        }
    }
    ensure_current_active_instance(owner, session).await?;
    let record = store.update(session, record.task_id, |record| {
        if record.status.is_terminal() {
            return Ok(());
        }
        record.status = derived.status;
        record.remote_status = Some(remote.status.clone());
        record.remote_status_detail = remote.status_detail.clone();
        if remote.url.is_some() {
            record.session_url = remote.url.clone();
        }
        if remote.acus_consumed_milli.is_some() {
            record.acus_consumed_milli = remote.acus_consumed_milli;
        }
        if !remote.pull_requests.is_empty() {
            record.pull_requests = remote.pull_requests.clone();
        }
        if derived.report.is_some() {
            record.report = derived.report.clone();
        }
        record.last_error = derived.last_error.clone();
        record.revision = record.revision.saturating_add(1);
        if derived.status.is_terminal() {
            let outcome = record.outcome();
            for receipt in record
                .operations
                .iter_mut()
                .filter(|receipt| receipt.phase == OperationPhase::Accepted)
            {
                receipt.phase = OperationPhase::Applied;
                receipt.outcome = outcome.clone();
            }
        }
        Ok(())
    })?;
    let mut evidence_ref = None;
    if record.status.is_terminal() {
        let payload = json!({
            "kind": "devin_cloud_task_final_state",
            "task_id": record.task_id,
            "devin_session_id": devin_session_id,
            "session_url": record.session_url,
            "status": record.status.as_str(),
            "remote_status": record.remote_status,
            "remote_status_detail": record.remote_status_detail,
            "report": record.report,
            "pull_requests": record.pull_requests,
            "acus_consumed_milli": record.acus_consumed_milli,
            "final_messages": final_messages
                .iter()
                .rev()
                .take(MAX_EVIDENCE_MESSAGES)
                .collect::<Vec<_>>(),
        });
        evidence_ref = evidence::store(
            &session.id,
            &session.cwd,
            serde_json::to_string(&payload).unwrap_or_default(),
        )
        .ok()
        .flatten();
    }
    Ok((record, evidence_ref))
}

// ---------- public entry points ----------

fn connect() -> Result<(CloudConfig, CloudApi)> {
    let config = resolve_config().map_err(anyhow::Error::msg)?;
    #[cfg(test)]
    if let Some(fake) = fake_api() {
        return Ok((config, fake));
    }
    let api = CloudApi::http(&config)?;
    Ok((config, api))
}

pub(crate) async fn status(session: &config::Session) -> Result<Value> {
    let owner = SessionInstance::from_session(session);
    ensure_current_active_instance(&owner, session).await?;
    let (config, api) = connect()?;
    let me = api.get_self().await?;
    let principal_type = me.get("principal_type").and_then(Value::as_str);
    let principal_name = me
        .get("service_user_name")
        .or_else(|| me.get("user_name"))
        .and_then(Value::as_str)
        .map(|name| bound_text(name, MAX_ARGUMENT_BYTES));
    let remote_org_id = me.get("org_id").and_then(Value::as_str);
    let readiness = config.readiness();
    Ok(json!({
        "compatible": true,
        "backend": "devin_cloud",
        "api_version": "v3",
        "base_url": readiness.base_url,
        "api_key_source": readiness.api_key_source,
        "principal_type": principal_type,
        "principal_name": principal_name,
        "org_id": config.org_id.as_deref().or(remote_org_id),
        "org_id_source": if config.org_id.is_some() { ORG_ID_ENV } else { "self" },
    }))
}

pub(crate) async fn task_start(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    task_start_with_store(args, session, &store).await
}

fn optional_string_list(args: &Value, key: &str, max: usize) -> Result<Vec<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            anyhow::ensure!(items.len() <= max, "{key} accepts at most {max} entries");
            items
                .iter()
                .map(|item| {
                    let value = item
                        .as_str()
                        .with_context(|| format!("{key} entries must be strings"))?;
                    validate_argument(value, key)?;
                    Ok(value.to_owned())
                })
                .collect()
        }
        Some(_) => anyhow::bail!("{key} must be an array of strings"),
    }
}

async fn task_start_with_store(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<Value> {
    let owner = SessionInstance::from_session(session);
    let operation_id = required_uuid(args, "operation_id")?;
    let task = required_string(args, "task")?;
    let title = optional_string(args, "title")?;
    let devin_mode = optional_string(args, "devin_mode")?;
    let swe_tier = optional_string(args, "swe_tier")?;
    let repos = optional_string_list(args, "repos", MAX_REPOS)?;
    let max_acu_limit = optional_u64(args, "max_acu_limit")?;
    validate_task_input(task, "task")?;
    if let Some(title) = title {
        validate_argument(title, "title")?;
    }
    if let Some(mode) = devin_mode {
        validate_devin_mode(mode)?;
    }
    validate_swe_tier(swe_tier, devin_mode)?;
    if let Some(limit) = max_acu_limit {
        anyhow::ensure!(
            (1..=100_000).contains(&limit),
            "max_acu_limit must be within 1..=100000"
        );
    }

    ensure_current_active_instance(&owner, session).await?;
    let task_id = task_id_for_operation(session, operation_id)?;
    let request_fingerprint = fingerprint(&json!({
        "kind": "start",
        "task_id": task_id,
        "task": task,
        "title": title,
        "devin_mode": devin_mode,
        "swe_tier": swe_tier,
        "repos": repos,
        "max_acu_limit": max_acu_limit,
    }))?;
    if let Some(existing) = store.existing_start(session, task_id)? {
        return replay_operation(&existing, operation_id, request_fingerprint);
    }

    let effective_devin_mode = resolve_effective_devin_mode(devin_mode, swe_tier).await?;

    let (config, api) = connect()?;
    let org_id = resolve_org_id(&config, &api).await?;
    let now = config::unix_time();
    let mut record = TaskRecord {
        schema_version: TASK_SCHEMA_VERSION,
        task_id,
        owner: owner.clone(),
        scope_cwd: config::canonical_directory(&session.cwd)?,
        org_id: org_id.clone(),
        title: title.map(str::to_owned),
        devin_mode: devin_mode.map(str::to_owned),
        swe_tier: swe_tier.map(str::to_owned),
        effective_devin_mode: effective_devin_mode.clone(),
        repos: repos.clone(),
        status: TaskStatus::Accepted,
        revision: 1,
        generation: 0,
        devin_session_id: None,
        session_url: None,
        remote_status: None,
        remote_status_detail: None,
        acus_consumed_milli: None,
        pull_requests: Vec::new(),
        report: None,
        last_error: None,
        created_at: now,
        updated_at: now,
        operations: Vec::new(),
        operation_tombstones: Vec::new(),
    };
    record.operations.push(OperationReceipt {
        operation_id,
        request_fingerprint,
        action: "start".to_owned(),
        phase: OperationPhase::Accepted,
        outcome: record.outcome(),
    });
    let record = match store.accept_start(session, record)? {
        StartAcceptance::Existing(existing) => {
            return replay_operation(&existing, operation_id, request_fingerprint);
        }
        StartAcceptance::Accepted(record) => record,
    };

    let mut body = json!({
        "prompt": format!("{REPORT_INSTRUCTIONS}{task}"),
        "title": record.title.clone().unwrap_or_else(|| format!("temote-mcp task {task_id}")),
        "tags": [SESSION_TAG],
        "structured_output_schema": report_schema(),
        "structured_output_required": true,
        "resumable": true,
    });
    if let Some(mode) = effective_devin_mode.as_deref() {
        body["devin_mode"] = json!(mode);
    }
    if !repos.is_empty() {
        body["repos"] = json!(repos);
    }
    if let Some(limit) = max_acu_limit {
        body["max_acu_limit"] = json!(limit);
    }
    if let Some(user_id) = config.create_as_user_id.as_deref() {
        body["create_as_user_id"] = json!(user_id);
    }

    let created = api.create_session(&org_id, &body).await.and_then(|value| {
        parse_remote_session(&value).map_err(|error| {
            ApiError::Uncertain(bound_text(&format!("{error:#}"), MAX_ERROR_BYTES))
        })
    });
    let remote = match created {
        Ok(remote) => remote,
        Err(error) => {
            let (status, phase) = match &error {
                ApiError::Rejected(_) => {
                    (TaskStatus::RetryableFailed, OperationPhase::RetryableFailed)
                }
                ApiError::Uncertain(_) => {
                    (TaskStatus::ReconciliationRequired, OperationPhase::Accepted)
                }
            };
            let record = store.update(session, task_id, |record| {
                if !record.status.is_terminal() {
                    record.status = status;
                    record.last_error = Some(bound_text(error.message(), MAX_ERROR_BYTES));
                    record.revision = record.revision.saturating_add(1);
                    update_operation_receipt(record, operation_id, phase);
                }
                Ok(())
            })?;
            return Ok(task_view(&record, None));
        }
    };

    let record = store.update(session, task_id, |record| {
        if record.status.is_terminal() {
            return Ok(());
        }
        anyhow::ensure!(
            record.devin_session_id.is_none()
                || record.devin_session_id.as_deref() == Some(remote.devin_session_id.as_str()),
            "Devin Cloud task session was already bound"
        );
        record.devin_session_id = Some(remote.devin_session_id.clone());
        record.session_url = remote.url.clone();
        record.remote_status = Some(remote.status.clone());
        record.remote_status_detail = remote.status_detail.clone();
        record.status = TaskStatus::Running;
        record.generation = 1;
        record.revision = record.revision.saturating_add(1);
        update_operation_receipt(record, operation_id, OperationPhase::Applied);
        Ok(())
    })?;
    Ok(task_view(&record, None))
}

fn replay_operation(record: &TaskRecord, operation_id: Uuid, fingerprint: Uuid) -> Result<Value> {
    let Some(receipt) = record
        .operations
        .iter()
        .find(|receipt| receipt.operation_id == operation_id)
    else {
        if let Some(tombstone) = record
            .operation_tombstones
            .iter()
            .find(|tombstone| tombstone.operation_id == operation_id)
        {
            anyhow::ensure!(
                tombstone.request_fingerprint == fingerprint,
                "OPERATION_CONFLICT: operation_id was already accepted with a different request"
            );
            anyhow::bail!(
                "OPERATION_REPLAY_COMPACTED: operation_id was accepted during task retention, but its detailed receipt was compacted; refusing to reapply the mutation"
            );
        }
        anyhow::bail!("OPERATION_CONFLICT: task exists without the requested operation receipt");
    };
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
    let store = TaskStore::default_store()?;
    task_get_with_store(args, session, &store).await
}

async fn task_get_with_store(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<Value> {
    let task_id = required_uuid(args, "task_id")?;
    let after_revision = optional_u64(args, "after_revision")?;
    let owner = SessionInstance::from_session(session);
    ensure_current_active_instance(&owner, session).await?;
    let record = store.load(session, task_id)?;
    if record.status.is_terminal() {
        if after_revision == Some(record.revision) {
            return Ok(not_modified_view(&record));
        }
        return Ok(task_view(&record, None));
    }
    let (_config, api) = connect()?;
    let (record, evidence_ref) = reconcile(session, &owner, store, &api, record).await?;
    if after_revision == Some(record.revision) {
        return Ok(not_modified_view(&record));
    }
    Ok(task_view(&record, evidence_ref.as_ref()))
}

/// Read-only projection of the tasks this session instance owns.
/// Reconciliation stays in `task_get`: the list reports retained state
/// only, so it never calls the remote API or mutates records.
pub(crate) async fn task_list(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    task_list_with_store(args, session, &store)
}

fn task_list_with_store(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<Value> {
    let limit = task_list_limit(args)?;
    let (mut records, skipped) = store.list_owned(session)?;
    records.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.task_id.cmp(&b.task_id))
    });
    let total = records.len();
    let tasks: Vec<Value> = records.iter().take(limit).map(task_list_item).collect();
    Ok(json!({
        "backend": "devin_cloud",
        "tasks": tasks,
        "total": total,
        "skipped": skipped,
        "truncated": total > tasks.len(),
        "limit": limit,
    }))
}

fn task_list_item(record: &TaskRecord) -> Value {
    let mut item = task_view(record, None);
    item["backend"] = json!("devin_cloud");
    item["last_updated_at"] = json!(record.updated_at);
    item
}

fn task_list_limit(args: &Value) -> Result<usize> {
    match args.get("limit") {
        None => Ok(50),
        Some(value) => {
            let limit = value
                .as_u64()
                .context("task_list limit must be an integer")?;
            anyhow::ensure!(
                (1..=128).contains(&limit),
                "task_list limit must be 1..=128"
            );
            Ok(limit as usize)
        }
    }
}

pub(crate) async fn task_control(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    task_control_with_store(args, session, &store).await
}

async fn task_control_with_store(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<Value> {
    let task_id = required_uuid(args, "task_id")?;
    let operation_id = required_uuid(args, "operation_id")?;
    let action = required_string(args, "action")?;
    anyhow::ensure!(
        matches!(action, "steer" | "resume" | "interrupt"),
        "unsupported Devin task action"
    );
    let input = args.get("input").and_then(Value::as_str);
    match action {
        "steer" => validate_task_input(input.context("steer requires input")?, "input")?,
        "resume" | "interrupt" => {
            anyhow::ensure!(input.is_none(), "{action} does not accept input")
        }
        _ => unreachable!(),
    }
    let request_fingerprint = fingerprint(&json!({
        "kind": "control",
        "task_id": task_id,
        "action": action,
        "input": input,
    }))?;
    let owner = SessionInstance::from_session(session);
    ensure_current_active_instance(&owner, session).await?;
    let mut record =
        match store.accept_control(session, task_id, operation_id, request_fingerprint, action)? {
            ControlAcceptance::Replay(result) => return Ok(result),
            ControlAcceptance::Accepted(record) => *record,
        };
    let (_config, api) = connect()?;

    if record.devin_session_id.is_none() {
        record = reconcile(session, &owner, store, &api, record).await?.0;
    }
    let Some(devin_session_id) = record.devin_session_id.clone() else {
        let record = store.update(session, task_id, |record| {
            if !record.status.is_terminal() {
                record.status = TaskStatus::ReconciliationRequired;
                record.revision = record.revision.saturating_add(1);
                update_operation_receipt(record, operation_id, OperationPhase::RetryableFailed);
            }
            Ok(())
        })?;
        return Ok(task_view(&record, None));
    };
    if record.status.is_terminal() {
        let record = store.update(session, task_id, |record| {
            update_operation_receipt(record, operation_id, OperationPhase::RetryableFailed);
            Ok(())
        })?;
        return Ok(task_view(&record, None));
    }

    let result = match action {
        "steer" => {
            api.send_message(&record.org_id, &devin_session_id, input.unwrap_or_default())
                .await
        }
        "resume" => {
            api.send_message(&record.org_id, &devin_session_id, RESUME_INSTRUCTIONS)
                .await
        }
        "interrupt" => {
            api.terminate_session(&record.org_id, &devin_session_id)
                .await
        }
        _ => unreachable!(),
    };
    let record = match result {
        Ok(_) => store.update(session, task_id, |record| {
            if record.status.is_terminal() {
                return Ok(());
            }
            record.generation = record.generation.saturating_add(1);
            record.status = if action == "interrupt" {
                TaskStatus::Interrupted
            } else {
                TaskStatus::Running
            };
            record.last_error = None;
            record.revision = record.revision.saturating_add(1);
            update_operation_receipt(record, operation_id, OperationPhase::Applied);
            Ok(())
        })?,
        Err(error) => {
            let (status, phase) = match &error {
                ApiError::Rejected(_) => (TaskStatus::Unknown, OperationPhase::RetryableFailed),
                ApiError::Uncertain(_) => {
                    (TaskStatus::ReconciliationRequired, OperationPhase::Accepted)
                }
            };
            store.update(session, task_id, |record| {
                if !record.status.is_terminal() {
                    record.status = status;
                    record.last_error = Some(bound_text(error.message(), MAX_ERROR_BYTES));
                    record.revision = record.revision.saturating_add(1);
                    update_operation_receipt(record, operation_id, phase);
                }
                Ok(())
            })?
        }
    };
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

fn optional_string<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .with_context(|| format!("{key} must be a string")),
    }
}

fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .with_context(|| format!("{key} must be a non-negative integer")),
    }
}

// ---------- tests ----------

#[cfg(test)]
static FAKE_API: OnceLock<Mutex<Option<Arc<Mutex<FakeApi>>>>> = OnceLock::new();

#[cfg(test)]
fn fake_api() -> Option<CloudApi> {
    FAKE_API
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .as_ref()
        .map(|fake| CloudApi::Fake(Arc::clone(fake)))
}

#[cfg(test)]
#[derive(Default)]
struct FakeApi {
    next_session: u64,
    sessions: std::collections::HashMap<String, Value>,
    messages: std::collections::HashMap<String, Vec<Value>>,
    create_fail: Option<ApiError>,
    message_fail: Option<ApiError>,
    calls: Vec<(String, String)>,
}

#[cfg(test)]
impl FakeApi {
    fn call(
        &mut self,
        method: reqwest::Method,
        path: &str,
        _query: &[(&str, String)],
        body: Option<&Value>,
    ) -> ApiResult<Value> {
        self.calls.push((method.to_string(), path.to_owned()));
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (method.as_str(), segments.as_slice()) {
            ("GET", ["v3", "self"]) => Ok(json!({
                "principal_type": "service_user",
                "service_user_id": "su-test",
                "service_user_name": "temote-test",
                "org_id": "org-test",
            })),
            ("POST", ["v3", "organizations", org, "sessions"]) => {
                if let Some(error) = &self.create_fail {
                    return Err(error.clone());
                }
                self.next_session += 1;
                let id = format!("devin-{:04}", self.next_session);
                let body = body.cloned().unwrap_or_default();
                let session = json!({
                    "session_id": id,
                    "url": format!("https://app.devin.ai/sessions/{id}"),
                    "status": "new",
                    "status_detail": null,
                    "org_id": org,
                    "title": body.get("title"),
                    "devin_mode": body.get("devin_mode"),
                    "acus_consumed": 0.0,
                    "pull_requests": [],
                    "structured_output": null,
                });
                self.sessions.insert(id.clone(), session.clone());
                self.messages.insert(
                    id.clone(),
                    vec![json!({
                        "event_id": "e1",
                        "source": "user",
                        "message": body.get("prompt"),
                        "created_at": 1,
                    })],
                );
                Ok(session)
            }
            ("GET", ["v3", "organizations", _, "sessions", id]) => self
                .sessions
                .get(*id)
                .cloned()
                .ok_or_else(|| ApiError::Rejected("HTTP 404".to_owned())),
            ("DELETE", ["v3", "organizations", _, "sessions", id]) => {
                let session = self
                    .sessions
                    .get_mut(*id)
                    .ok_or_else(|| ApiError::Rejected("HTTP 404".to_owned()))?;
                session["status"] = json!("suspended");
                session["status_detail"] = json!("user_request");
                Ok(json!({}))
            }
            ("GET", ["v3", "organizations", _, "sessions", id, "messages"]) => Ok(json!({
                "items": self.messages.get(*id).cloned().unwrap_or_default(),
                "has_more": false,
            })),
            ("POST", ["v3", "organizations", _, "sessions", id, "messages"]) => {
                if let Some(error) = &self.message_fail {
                    return Err(error.clone());
                }
                let session = self
                    .sessions
                    .get_mut(*id)
                    .ok_or_else(|| ApiError::Rejected("HTTP 404".to_owned()))?;
                session["status"] = json!("running");
                session["status_detail"] = json!("working");
                self.messages
                    .entry((*id).to_owned())
                    .or_default()
                    .push(json!({
                        "event_id": "e-user",
                        "source": "user",
                        "message": body.and_then(|body| body.get("message")).cloned(),
                        "created_at": 2,
                    }));
                Ok(json!({}))
            }
            _ => Err(ApiError::Rejected(format!("unsupported fake route {path}"))),
        }
    }

    fn finish(&mut self, id: &str, structured: Option<Value>, final_text: Option<&str>) {
        let session = self.sessions.get_mut(id).unwrap();
        session["status"] = json!("running");
        session["status_detail"] = json!("finished");
        session["structured_output"] = structured.unwrap_or(Value::Null);
        session["acus_consumed"] = json!(1.25);
        session["pull_requests"] =
            json!([{"pr_url": "https://example.test/pr/1", "pr_state": "open"}]);
        if let Some(text) = final_text {
            self.messages.entry(id.to_owned()).or_default().push(json!({
                "event_id": "e-final",
                "source": "devin",
                "message": text,
                "created_at": 3,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals;

    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    struct EnvGuard;

    impl EnvGuard {
        fn install() -> Self {
            // SAFETY: tests holding `serial()` are the only writers of these
            // variables in this process.
            unsafe {
                std::env::set_var(API_KEY_ENV, "cog_test_key");
                std::env::set_var(ORG_ID_ENV, "org-test");
                std::env::remove_var(BASE_URL_ENV);
                std::env::remove_var(CREATE_AS_USER_ID_ENV);
            }
            Self
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: see `install`.
            unsafe {
                std::env::remove_var(API_KEY_ENV);
                std::env::remove_var(ORG_ID_ENV);
            }
            clear_fake();
            set_fake_swe_priority_uid(None);
        }
    }

    fn install_fake() -> Arc<Mutex<FakeApi>> {
        let fake = Arc::new(Mutex::new(FakeApi::default()));
        *FAKE_API.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(Arc::clone(&fake));
        fake
    }

    fn clear_fake() {
        *FAKE_API.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;
    }

    async fn active_test_session(
        root: &Path,
        id: &str,
    ) -> (approvals::RuntimeHandle, config::Session) {
        let (approval_sender, _approval_receiver) = approvals::approval_channel();
        let handle = approvals::spawn_runtime(root, Some(id), false, approval_sender)
            .await
            .unwrap();
        let session = config::read_session_metadata(id).await.unwrap();
        (handle, session)
    }

    fn test_id() -> String {
        format!("devin-cloud-test-{}", Uuid::new_v4().simple())
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devin-cloud-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_store(root: &Path) -> TaskStore {
        TaskStore::new(root.join("devin-cloud-tasks"))
    }

    fn start_args(operation_id: Uuid, task: &str) -> Value {
        json!({
            "operation_id": operation_id,
            "task": task,
            "title": "test task",
            "devin_mode": "fast",
            "repos": ["f4ah6o/temote-mcp"],
        })
    }

    fn valid_report() -> Value {
        json!({
            "status": "completed",
            "summary": "done",
            "changed_files": ["src/lib.rs"],
            "checks": ["cargo test"],
            "unresolved": [],
        })
    }

    #[test]
    fn swe_priority_catalog_resolution_is_exact_and_fail_closed() {
        let catalog = json!({
            "families": [{
                "family_uid": "swe-2",
                "variants": [
                    {"model_uid": "swe-2-high"},
                    {"model_uid": "swe-2-high-priority"},
                    {"model_uid": "swe-2-max-fast"}
                ]
            }]
        });
        assert_eq!(
            resolve_swe2_priority_uid_from_catalog(&catalog, "swe-2-high").unwrap(),
            "swe-2-high-priority"
        );
        assert_eq!(
            resolve_swe2_priority_uid_from_catalog(&catalog, "swe-2-max").unwrap(),
            "swe-2-max-fast"
        );

        let promo_only = json!({"families": [{"variants": [{"model_uid": "swe-2-high"}]}]});
        assert!(
            resolve_swe2_priority_uid_from_catalog(&promo_only, "swe-2-high")
                .unwrap_err()
                .to_string()
                .contains("unavailable")
        );

        let ambiguous = json!({"families": [{"variants": [
            {"model_uid": "swe-2-high-priority"},
            {"model_uid": "swe-2-high-fast"}
        ]}]});
        assert!(
            resolve_swe2_priority_uid_from_catalog(&ambiguous, "swe-2-high")
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );

        assert!(
            resolve_swe2_priority_uid_from_catalog(
                &json!({"families": [{"variants": [{"model_uid": 7}]}]}),
                "swe-2-high"
            )
            .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn swe_priority_preflight_resolves_exact_uid_and_fails_before_remote_create() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();

        set_fake_swe_priority_uid(Some(Ok("swe-2-high-priority".to_owned())));
        let op = Uuid::new_v4();
        let args = json!({
            "operation_id": op,
            "task": "priority work",
            "devin_mode": "swe-2-high",
            "swe_tier": "priority"
        });
        let out = task_start_with_store(&args, &session, &store).await.unwrap();
        assert_eq!(out["swe_tier"], "priority");
        assert_eq!(out["effective_devin_mode"], "swe-2-high-priority");
        assert_eq!(
            fake.lock().unwrap().sessions["devin-0001"]["devin_mode"],
            "swe-2-high-priority"
        );

        set_fake_swe_priority_uid(Some(Err("catalog unavailable".to_owned())));
        let replay = task_start_with_store(&args, &session, &store).await.unwrap();
        assert_eq!(replay["status"], "running");
        assert_eq!(fake.lock().unwrap().sessions.len(), 1);

        let fresh_args = json!({
            "operation_id": Uuid::new_v4(),
            "task": "fresh priority work",
            "devin_mode": "swe-2-high",
            "swe_tier": "priority"
        });
        let calls_before = fake.lock().unwrap().calls.len();
        let error = task_start_with_store(&fresh_args, &session, &store)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("catalog unavailable"));
        assert_eq!(fake.lock().unwrap().calls.len(), calls_before);
        assert_eq!(fake.lock().unwrap().sessions.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn swe_promo_uses_requested_mode_without_priority_discovery() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();
        set_fake_swe_priority_uid(Some(Err("must not be called".to_owned())));

        let args = json!({
            "operation_id": Uuid::new_v4(),
            "task": "promo work",
            "devin_mode": "swe-2-max",
            "swe_tier": "promo"
        });
        let out = task_start_with_store(&args, &session, &store).await.unwrap();
        assert_eq!(out["swe_tier"], "promo");
        assert_eq!(out["effective_devin_mode"], "swe-2-max");
        assert_eq!(
            fake.lock().unwrap().sessions["devin-0001"]["devin_mode"],
            "swe-2-max"
        );
    }

    #[test]
    fn swe_tier_validation_requires_swe2_mode() {
        assert!(validate_swe_tier(None, None).is_ok());
        assert!(validate_swe_tier(Some("promo"), Some("swe-2-medium")).is_ok());
        assert!(validate_swe_tier(Some("priority"), Some("swe-2-max")).is_ok());
        assert!(validate_swe_tier(Some("priority"), Some("fast")).is_err());
        assert!(validate_swe_tier(Some("bogus"), Some("swe-2-high")).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_creates_remote_session_and_binds_it() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();

        let op = Uuid::new_v4();
        let out = task_start_with_store(&start_args(op, "implement the feature"), &session, &store)
            .await
            .unwrap();
        assert_eq!(out["status"], "running");
        assert_eq!(out["devin_session_id"], "devin-0001");
        assert_eq!(out["org_id"], "org-test");
        assert_eq!(
            out["session_url"],
            "https://app.devin.ai/sessions/devin-0001"
        );
        let task_id = Uuid::parse_str(out["task_id"].as_str().unwrap()).unwrap();
        assert_eq!(task_id, task_id_for_operation(&session, op).unwrap());

        let fake = fake.lock().unwrap();
        assert!(fake.calls.iter().any(
            |(method, path)| method == "POST" && path == "/v3/organizations/org-test/sessions"
        ));
        let prompt = fake.messages["devin-0001"][0]["message"].as_str().unwrap();
        assert!(prompt.contains("implement the feature"));
        assert!(prompt.starts_with(REPORT_INSTRUCTIONS.trim_end_matches("Task:\n")));
        assert_eq!(fake.sessions["devin-0001"]["title"], "test task");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_replays_same_operation_id_without_recreating() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();

        let op = Uuid::new_v4();
        let first = task_start_with_store(&start_args(op, "same task"), &session, &store)
            .await
            .unwrap();
        let second = task_start_with_store(&start_args(op, "same task"), &session, &store)
            .await
            .unwrap();
        assert_eq!(first["task_id"], second["task_id"]);
        assert_eq!(second["status"], "running");
        assert_eq!(fake.lock().unwrap().sessions.len(), 1);

        let conflict = task_start_with_store(&start_args(op, "different task"), &session, &store)
            .await
            .unwrap_err();
        assert!(conflict.to_string().contains("OPERATION_CONFLICT"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejected_create_is_retryable_and_uncertain_create_requires_reconciliation() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();

        fake.lock().unwrap().create_fail = Some(ApiError::Rejected("HTTP 403".to_owned()));
        let out = task_start_with_store(&start_args(Uuid::new_v4(), "a"), &session, &store)
            .await
            .unwrap();
        assert_eq!(out["status"], "retryable_failed");
        assert_eq!(out["last_error"], "HTTP 403");

        fake.lock().unwrap().create_fail = Some(ApiError::Uncertain("timeout".to_owned()));
        let out = task_start_with_store(&start_args(Uuid::new_v4(), "b"), &session, &store)
            .await
            .unwrap();
        assert_eq!(out["status"], "reconciliation_required");
        assert_eq!(out["reconciliation_required"], true);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_reconciles_remote_status_and_extracts_structured_report() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();

        let out = task_start_with_store(&start_args(Uuid::new_v4(), "work"), &session, &store)
            .await
            .unwrap();
        let task_id = out["task_id"].clone();

        {
            let mut fake = fake.lock().unwrap();
            let session = fake.sessions.get_mut("devin-0001").unwrap();
            session["status"] = json!("running");
            session["status_detail"] = json!("waiting_for_approval");
        }
        let out = task_get_with_store(&json!({"task_id": task_id}), &session, &store)
            .await
            .unwrap();
        assert_eq!(out["status"], "waiting_approval");
        assert_eq!(out["remote_status_detail"], "waiting_for_approval");
        let revision = out["revision"].as_u64().unwrap();

        let out = task_get_with_store(
            &json!({"task_id": task_id, "after_revision": revision + 1}),
            &session,
            &store,
        )
        .await
        .unwrap();
        assert_eq!(out["status"], "not_modified");

        fake.lock()
            .unwrap()
            .finish("devin-0001", Some(valid_report()), None);
        let out = task_get_with_store(&json!({"task_id": task_id}), &session, &store)
            .await
            .unwrap();
        assert_eq!(out["status"], "completed");
        assert_eq!(out["report"], valid_report());
        assert_eq!(out["acus_consumed_milli"], 1250);
        assert_eq!(out["pull_requests"][0], "https://example.test/pr/1");
        assert!(out["last_error"].is_null());
        assert!(out["evidence"].is_object());

        // Terminal records are served from the store without remote calls.
        let calls = fake.lock().unwrap().calls.len();
        let again = task_get_with_store(&json!({"task_id": task_id}), &session, &store)
            .await
            .unwrap();
        assert_eq!(again["status"], "completed");
        assert_eq!(fake.lock().unwrap().calls.len(), calls);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_falls_back_to_final_message_report() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();

        let out = task_start_with_store(&start_args(Uuid::new_v4(), "work"), &session, &store)
            .await
            .unwrap();
        let task_id = out["task_id"].clone();
        fake.lock().unwrap().finish(
            "devin-0001",
            None,
            Some("Done.\n{\"status\":\"failed\",\"summary\":\"tests red\"}"),
        );
        let out = task_get_with_store(&json!({"task_id": task_id}), &session, &store)
            .await
            .unwrap();
        assert_eq!(out["status"], "completed");
        assert_eq!(out["report"]["status"], "failed");
        assert!(out["last_error"].is_null());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn control_steers_resumes_and_interrupts_idempotently() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();

        let out = task_start_with_store(&start_args(Uuid::new_v4(), "work"), &session, &store)
            .await
            .unwrap();
        let task_id = out["task_id"].clone();

        let steer_op = Uuid::new_v4();
        let steer = json!({"task_id": task_id, "operation_id": steer_op, "action": "steer", "input": "also add tests"});
        let out = task_control_with_store(&steer, &session, &store)
            .await
            .unwrap();
        assert_eq!(out["status"], "running");
        assert_eq!(out["generation"], 2);
        let replay = task_control_with_store(&steer, &session, &store)
            .await
            .unwrap();
        assert_eq!(replay["generation"], 2);
        {
            let fake = fake.lock().unwrap();
            let messages = &fake.messages["devin-0001"];
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[1]["message"], "also add tests");
        }

        let resume =
            json!({"task_id": task_id, "operation_id": Uuid::new_v4(), "action": "resume"});
        let out = task_control_with_store(&resume, &session, &store)
            .await
            .unwrap();
        assert_eq!(out["status"], "running");
        assert_eq!(
            fake.lock().unwrap().messages["devin-0001"][2]["message"],
            RESUME_INSTRUCTIONS
        );

        let interrupt =
            json!({"task_id": task_id, "operation_id": Uuid::new_v4(), "action": "interrupt"});
        let out = task_control_with_store(&interrupt, &session, &store)
            .await
            .unwrap();
        assert_eq!(out["status"], "interrupted");
        assert!(
            fake.lock()
                .unwrap()
                .calls
                .iter()
                .any(|(method, _)| method == "DELETE")
        );

        let late = json!({"task_id": task_id, "operation_id": Uuid::new_v4(), "action": "steer", "input": "x"});
        let error = task_control_with_store(&late, &session, &store)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("TASK_TERMINAL"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn control_failure_modes_follow_error_certainty() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();

        let out = task_start_with_store(&start_args(Uuid::new_v4(), "work"), &session, &store)
            .await
            .unwrap();
        let task_id = out["task_id"].clone();

        fake.lock().unwrap().message_fail = Some(ApiError::Rejected("HTTP 400".to_owned()));
        let out = task_control_with_store(
            &json!({"task_id": task_id, "operation_id": Uuid::new_v4(), "action": "steer", "input": "x"}),
            &session,
            &store,
        )
        .await
        .unwrap();
        assert_eq!(out["status"], "unknown");

        fake.lock().unwrap().message_fail = Some(ApiError::Uncertain("timeout".to_owned()));
        let out = task_control_with_store(
            &json!({"task_id": task_id, "operation_id": Uuid::new_v4(), "action": "steer", "input": "x"}),
            &session,
            &store,
        )
        .await
        .unwrap();
        assert_eq!(out["status"], "reconciliation_required");

        // A following get re-reads the remote state and recovers.
        fake.lock().unwrap().message_fail = None;
        let out = task_get_with_store(&json!({"task_id": task_id}), &session, &store)
            .await
            .unwrap();
        assert_eq!(out["status"], "running");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tasks_are_invisible_across_sessions() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let (_other_handle, other) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let _fake = install_fake();

        let out = task_start_with_store(&start_args(Uuid::new_v4(), "work"), &session, &store)
            .await
            .unwrap();
        let error = task_get_with_store(&json!({"task_id": out["task_id"]}), &other, &store)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("DEVIN_TASK_NOT_FOUND"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn control_foreign_session_denied() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let (_other_handle, other) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let _fake = install_fake();

        let out = task_start_with_store(&start_args(Uuid::new_v4(), "work"), &session, &store)
            .await
            .unwrap();
        // A control from a different session fails at the task store's
        // ownership check, before any Cloud API call is made.
        let error = task_control_with_store(
            &json!({
                "task_id": out["task_id"],
                "operation_id": Uuid::new_v4(),
                "action": "interrupt",
            }),
            &other,
            &store,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("DEVIN_TASK_NOT_FOUND"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn status_reports_principal_without_secret() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let _fake = install_fake();

        let out = status(&session).await.unwrap();
        assert_eq!(out["compatible"], true);
        assert_eq!(out["principal_type"], "service_user");
        assert_eq!(out["org_id"], "org-test");
        assert_eq!(out["api_key_source"], API_KEY_ENV);
        assert!(
            !serde_json::to_string(&out)
                .unwrap()
                .contains("cog_test_key")
        );
    }

    #[test]
    fn derive_state_maps_remote_lifecycle() {
        let remote =
            |status: &str, detail: Option<&str>, structured: Option<Value>| RemoteSession {
                devin_session_id: "devin-x".to_owned(),
                url: None,
                status: status.to_owned(),
                status_detail: detail.map(str::to_owned),
                acus_consumed_milli: None,
                pull_requests: Vec::new(),
                structured_output: structured,
            };
        let running = TaskStatus::Running;
        assert_eq!(
            derive_state(&remote("new", None, None), running).status,
            TaskStatus::Running
        );
        let waiting = derive_state(&remote("running", Some("waiting_for_user"), None), running);
        assert_eq!(waiting.status, TaskStatus::WaitingInput);
        assert!(waiting.needs_messages);
        // Devin idles after a finished turn; a terminal structured report wins.
        let done_idle = derive_state(
            &remote("running", Some("waiting_for_user"), Some(valid_report())),
            running,
        );
        assert_eq!(done_idle.status, TaskStatus::Completed);
        assert!(!done_idle.needs_messages);
        let failed_idle = derive_state(
            &remote(
                "running",
                Some("waiting_for_user"),
                Some(json!({"status": "failed", "summary": "x"})),
            ),
            running,
        );
        assert_eq!(failed_idle.status, TaskStatus::Failed);
        let blocked_idle = derive_state(
            &remote(
                "running",
                Some("waiting_for_user"),
                Some(json!({"status": "blocked", "summary": "need key"})),
            ),
            running,
        );
        assert_eq!(blocked_idle.status, TaskStatus::WaitingInput);
        let finished = derive_state(&remote("running", Some("finished"), None), running);
        assert_eq!(finished.status, TaskStatus::Completed);
        assert!(finished.needs_messages);
        let exited = derive_state(&remote("exit", None, Some(valid_report())), running);
        assert_eq!(exited.status, TaskStatus::Completed);
        assert_eq!(exited.report, Some(valid_report()));
        assert_eq!(
            derive_state(&remote("error", None, None), running).status,
            TaskStatus::Failed
        );
        assert_eq!(
            derive_state(&remote("suspended", Some("out_of_credits"), None), running).status,
            TaskStatus::Failed
        );
        assert_eq!(
            derive_state(&remote("suspended", Some("inactivity"), None), running).status,
            TaskStatus::WaitingInput
        );
        assert_eq!(
            derive_state(&remote("suspended", Some("user_request"), None), running).status,
            TaskStatus::Interrupted
        );
        assert_eq!(
            derive_state(&remote("weird", None, None), running).status,
            TaskStatus::Unknown
        );
    }

    #[test]
    fn base_url_validation_rejects_unsafe_origins() {
        assert_eq!(
            validate_base_url("https://api.example.test/").unwrap(),
            "https://api.example.test"
        );
        assert!(validate_base_url("http://api.example.test").is_err());
        assert!(validate_base_url("https://user@api.example.test").is_err());
        assert!(validate_base_url("https://api.example.test/?x=1").is_err());
        assert!(validate_base_url("").is_err());
    }

    #[test]
    fn report_extraction_requires_shape() {
        assert!(extract_report("no json here").is_none());
        assert!(extract_report("{\"status\":\"weird\",\"summary\":\"x\"}").is_none());
        let report =
            extract_report("prefix {\"status\":\"blocked\",\"summary\":\"need key\"} suffix")
                .unwrap();
        assert_eq!(report["status"], "blocked");
    }

    // ---------- session-owned task listing ----------

    fn session(root: &Path, id: &str) -> config::Session {
        let cwd = config::canonical_directory(root).unwrap();
        config::Session {
            id: id.to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 1234,
            process_id: 5678,
            permission_mode: config::PermissionMode::Agent,
            grants: config::SessionGrants::default(),
        }
    }

    fn record_for(session: &config::Session, task_id: Uuid, status: TaskStatus) -> TaskRecord {
        let now = config::unix_time();
        TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(session),
            scope_cwd: config::canonical_directory(&session.cwd).unwrap(),
            org_id: "org-test".to_owned(),
            title: None,
            devin_mode: None,
            swe_tier: None,
            effective_devin_mode: None,
            repos: Vec::new(),
            status,
            revision: 1,
            generation: 0,
            devin_session_id: None,
            session_url: None,
            remote_status: None,
            remote_status_detail: None,
            acus_consumed_milli: None,
            pull_requests: Vec::new(),
            report: None,
            last_error: None,
            created_at: now,
            updated_at: now,
            operations: Vec::new(),
            operation_tombstones: Vec::new(),
        }
    }

    #[test]
    fn task_list_projects_only_the_calling_sessions_tasks() {
        let workspace = tempdir();
        let other_workspace = tempdir();
        let store = test_store(&workspace);
        let owner = session(&workspace, "list-owner");
        let other = session(&workspace, "list-other");
        // Same id under a different instance is still a different
        // session; a different scope is a different task list entirely.
        let stale = config::Session {
            started_at: 9999,
            ..owner.clone()
        };
        let elsewhere = session(&other_workspace, "list-owner");
        let owner_task = Uuid::new_v4();
        store
            .save(&record_for(&owner, owner_task, TaskStatus::Running))
            .unwrap();
        for session in [&other, &stale, &elsewhere] {
            store
                .save(&record_for(session, Uuid::new_v4(), TaskStatus::Running))
                .unwrap();
        }

        let view = task_list_with_store(&json!({}), &owner, &store).unwrap();
        assert_eq!(view["backend"], "devin_cloud");
        assert_eq!(view["total"], 1);
        assert_eq!(view["skipped"], 0);
        assert_eq!(view["truncated"], false);
        let tasks = view["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["task_id"], json!(owner_task));
        assert_eq!(tasks[0]["backend"], "devin_cloud");
        assert!(tasks[0]["last_updated_at"].is_u64());
    }

    #[test]
    fn task_list_counts_unreadable_records_as_skipped() {
        let workspace = tempdir();
        let store = test_store(&workspace);
        let owner = session(&workspace, "list-skipped");
        store
            .save(&record_for(&owner, Uuid::new_v4(), TaskStatus::Running))
            .unwrap();
        // A record file that fails to parse cannot be verified as owned:
        // it is reported as skipped, not silently dropped.
        let corrupt = workspace
            .join("devin-cloud-tasks")
            .join(format!("{}.json", Uuid::new_v4()));
        std::fs::write(&corrupt, "{ not json").unwrap();

        let view = task_list_with_store(&json!({}), &owner, &store).unwrap();
        assert_eq!(view["total"], 1);
        assert_eq!(view["skipped"], 1);
        assert_eq!(view["tasks"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn task_list_orders_newest_first_and_truncates_at_limit() {
        let workspace = tempdir();
        let store = test_store(&workspace);
        let owner = session(&workspace, "list-order");
        let now = config::unix_time();
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        for (index, task_id) in ids.iter().enumerate() {
            let mut record = record_for(&owner, *task_id, TaskStatus::Running);
            record.created_at = now - 100;
            record.updated_at = now - index as u64;
            store.save(&record).unwrap();
        }

        let view = task_list_with_store(&json!({"limit": 2}), &owner, &store).unwrap();
        assert_eq!(view["total"], 3);
        assert_eq!(view["truncated"], true);
        let tasks = view["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["task_id"], json!(ids[0]));
        assert_eq!(tasks[1]["task_id"], json!(ids[1]));
    }

    #[test]
    fn task_list_reports_an_empty_store_as_empty() {
        let workspace = tempdir();
        // The directory does not exist until the first record lands.
        let store = test_store(&workspace);
        let owner = session(&workspace, "list-empty");

        let view = task_list_with_store(&json!({}), &owner, &store).unwrap();
        assert_eq!(view["tasks"].as_array().unwrap().len(), 0);
        assert_eq!(view["total"], 0);
        assert_eq!(view["skipped"], 0);
        assert_eq!(view["truncated"], false);
    }

    #[test]
    fn task_list_rejects_invalid_limits() {
        let workspace = tempdir();
        let store = test_store(&workspace);
        let owner = session(&workspace, "list-limit");

        assert_eq!(
            task_list_with_store(&json!({"limit": 0}), &owner, &store)
                .unwrap_err()
                .to_string(),
            "task_list limit must be 1..=128"
        );
        assert_eq!(
            task_list_with_store(&json!({"limit": 129}), &owner, &store)
                .unwrap_err()
                .to_string(),
            "task_list limit must be 1..=128"
        );
        assert_eq!(
            task_list_with_store(&json!({"limit": "many"}), &owner, &store)
                .unwrap_err()
                .to_string(),
            "task_list limit must be an integer"
        );
    }
}
