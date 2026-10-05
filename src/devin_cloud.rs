//! Devin Cloud (API v3) task backend.
//!
//! Drives hosted Devin sessions over the public REST API instead of a local
//! `devin acp` child. The durable contract mirrors the other delegation
//! backends: scope-bound task records with idempotent operation receipts,
//! typed controls, bounded reports and scoped evidence. There is no runtime
//! child-runtime lease because nothing runs locally; short-lived mutation
//! leases fence reads during start/control, and other gets reconcile against
//! the remote session as the source of truth.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use uuid::Uuid;

use crate::orchestration::outcome::{self, DeliveryRecord, VerificationRecord};
use crate::pending_interaction::{
    self, InteractionType, PendingInteractionSummary, ProducerKind, Summary, SummaryState,
};
use crate::{
    config, evidence,
    report_contract::{self, ReportProfile},
};

const MIN_TASK_SCHEMA_VERSION: u64 = 1;
const TASK_SCHEMA_VERSION: u64 = 3;
const TASK_RETENTION_SECONDS: u64 = 24 * 60 * 60;
const MAX_TASK_RECORD_BYTES: usize = 64 * 1024;
const MAX_TASK_DIRECTORY_ENTRIES: usize = 4096;
const MAX_CONTROL_LOCK_FILES: usize = MAX_TASK_DIRECTORY_ENTRIES;
const MAX_TASKS_PER_SCOPE: usize = 128;
const MAX_OPERATION_HISTORY: usize = 32;
const MAX_ARGUMENT_BYTES: usize = 256;
const MAX_REPOS: usize = 16;
const MAX_TASK_INPUT_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 1024;
use crate::report_contract::MAX_TASK_REPORT_BYTES as MAX_REPORT_BYTES;
const MAX_PULL_REQUESTS: usize = 16;
const MAX_MESSAGE_TEXT_BYTES: usize = MAX_REPORT_BYTES * 4;
const MAX_EVIDENCE_MESSAGES: usize = 8;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_API_KEY_BYTES: usize = 512;
const MAX_ORG_ID_BYTES: usize = 128;
const MAX_BASE_URL_BYTES: usize = 512;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DEVIN_MODEL_CATALOG_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_DEVIN_MODEL_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_DEVIN_MODEL_CATALOG_STDERR_BYTES: usize = 16 * 1024;

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

#[derive(Debug, Deserialize)]
struct CloudStatusRead {
    session_id: Option<String>,
    status: Option<String>,
    status_detail: Option<String>,
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
            .https_only(true)
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
            return Err(ApiError::Rejected(format!(
                "Devin Cloud API returned HTTP {status}"
            )));
        }
        if bytes.is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            ApiError::Uncertain(format!("Devin Cloud {path} returned a non-JSON response"))
        })
    }

    async fn get_session_status(&self, org_id: &str, devin_id: &str) -> ApiResult<CloudStatusRead> {
        let path = format!("/v3/organizations/{org_id}/sessions/{devin_id}");
        let response = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| {
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
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ApiError::Uncertain(format!(
                "Devin Cloud response exceeds {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        let mut response = response;
        let mut bytes = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or(0)
                .min(MAX_RESPONSE_BYTES as u64) as usize,
        );
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            ApiError::Uncertain(bound_text(
                &format!("Devin Cloud response read failed: {error}"),
                MAX_ERROR_BYTES,
            ))
        })? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ApiError::Uncertain(format!(
                    "Devin Cloud response exceeds {MAX_RESPONSE_BYTES} bytes"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&bytes);
            return Err(ApiError::Rejected(bound_text(
                &format!("Devin Cloud API returned HTTP {status}: {}", detail.trim()),
                MAX_ERROR_BYTES,
            )));
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            ApiError::Uncertain(
                "Devin Cloud status response did not match the bounded metadata shape".to_owned(),
            )
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
        let response = self.call(reqwest::Method::GET, "/v3/self", &[], None).await;
        #[cfg(test)]
        if let Self::Fake(fake) = self {
            let pause = fake.lock().unwrap().self_read_pause.take();
            if let Some((entered, release)) = pause {
                entered.notify_one();
                release.notified().await;
            }
        }
        response
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

    /// Bounded metadata projection used only by the background observer. It
    /// retains the remote identity and the two status fields needed to classify
    /// pending approval state; the remainder of the response is discarded.
    async fn get_session_status(&self, org_id: &str, devin_id: &str) -> ApiResult<CloudStatusRead> {
        match self {
            Self::Http(http) => http.get_session_status(org_id, devin_id).await,
            #[cfg(test)]
            Self::Fake(fake) => {
                let value = fake.lock().unwrap().call(
                    reqwest::Method::GET,
                    &format!("/v3/organizations/{org_id}/sessions/{devin_id}"),
                    &[],
                    None,
                )?;
                serde_json::from_value(value).map_err(|_| {
                    ApiError::Uncertain(
                        "Devin Cloud status response did not match the bounded metadata shape"
                            .to_owned(),
                    )
                })
            }
        }
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
    #[serde(default)]
    start_fingerprint_version: u64,
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
    #[serde(default)]
    verification: Option<VerificationRecord>,
    #[serde(default)]
    delivery: Option<DeliveryRecord>,
    operations: Vec<OperationReceipt>,
    #[serde(default)]
    operation_tombstones: Vec<OperationTombstone>,
    #[serde(default)]
    pending_interaction_summary: Option<PendingInteractionSummary>,
    #[serde(default)]
    observer_owner_id: Option<String>,
    #[serde(default)]
    producer_epoch: u64,
    #[serde(default)]
    lease_expires_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CloudObservationBinding {
    task_id: Uuid,
    owner: SessionInstance,
    scope_cwd: PathBuf,
    org_id: String,
    // This is a remote resource-routing identifier, not the bearer credential
    // used to authenticate requests. The persisted/public name remains
    // `devin_session_id` for compatibility.
    remote_session_resource_id: String,
    generation: u64,
}

impl CloudObservationBinding {
    fn from_record(record: &TaskRecord) -> Option<Self> {
        Some(Self {
            task_id: record.task_id,
            owner: record.owner.clone(),
            scope_cwd: record.scope_cwd.clone(),
            org_id: record.org_id.clone(),
            remote_session_resource_id: record.devin_session_id.clone()?,
            generation: record.generation,
        })
    }

    fn matches(&self, record: &TaskRecord) -> bool {
        record.task_id == self.task_id
            && record.owner == self.owner
            && record.scope_cwd == self.scope_cwd
            && record.org_id == self.org_id
            && record.devin_session_id.as_deref() == Some(&self.remote_session_resource_id)
            && record.generation == self.generation
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CloudObservationLease {
    owner_id: String,
    producer_epoch: u64,
    lease_expires_at: u64,
    binding: CloudObservationBinding,
}

struct CloudObserverJob {
    owner: SessionInstance,
    handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
pub(crate) struct CloudPendingObserver {
    owner_id: String,
    leases: Arc<AsyncMutex<std::collections::HashMap<Uuid, CloudObservationLease>>>,
    jobs: Arc<AsyncMutex<std::collections::HashMap<Uuid, CloudObserverJob>>>,
    session_gates: Arc<AsyncMutex<std::collections::HashMap<String, Arc<AsyncMutex<()>>>>>,
    retired_sessions: Arc<
        AsyncMutex<std::collections::HashMap<String, std::collections::HashSet<SessionInstance>>>,
    >,
}

#[derive(Clone, Debug)]
struct CloudPendingObservation {
    state: SummaryState,
    count: Option<u8>,
    types: Vec<InteractionType>,
    truncated: bool,
    read_started_at: u64,
    observed_at: u64,
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
    Accepted(TaskRecord, CloudControlLease),
}

enum ControlAcceptance {
    Replay(Value),
    Accepted(Box<TaskRecord>),
}

struct TaskStoreGuard {
    _process: MutexGuard<'static, ()>,
    file: File,
}

struct CloudControlLease {
    path: PathBuf,
    file: File,
}

impl Drop for CloudControlLease {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: this descriptor owns the nonblocking exclusive lock.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn try_control_lock(path: PathBuf, file: File) -> Result<Option<CloudControlLease>> {
    #[cfg(unix)]
    {
        // SAFETY: the descriptor remains open in the returned lease. Never
        // wait for this lock while holding the task store transaction lock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK)
                || error.raw_os_error() == Some(libc::EAGAIN)
            {
                return Ok(None);
            }
            return Err(error).context("cannot lock Devin Cloud task control");
        }
        Ok(Some(CloudControlLease { path, file }))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, file);
        anyhow::bail!("Devin Cloud task controls require Unix cross-process locking")
    }
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

    #[cfg(test)]
    fn load(&self, session: &config::Session, task_id: Uuid) -> Result<TaskRecord> {
        let _guard = self.lock()?;
        self.load_locked(session, task_id)
    }

    fn control_lock_path(&self, task_id: Uuid) -> PathBuf {
        self.directory
            .join("control-locks")
            .join(format!("{task_id}.lock"))
    }

    fn control_in_flight_locked(&self, task_id: Uuid) -> Result<bool> {
        let path = self.control_lock_path(task_id);
        let directory = path.parent().context("control lock has no directory")?;
        match std::fs::symlink_metadata(directory) {
            Ok(metadata) => validate_store_directory(directory, &metadata)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("cannot inspect control lock directory"),
        }
        let file = match open_existing_private_lock_file(&path) {
            Ok(file) => file,
            Err(error) if is_not_found(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(try_control_lock(path, file)?.is_none())
    }

    fn load_for_reconciliation(
        &self,
        session: &config::Session,
        task_id: Uuid,
    ) -> Result<(TaskRecord, bool)> {
        let _guard = self.lock()?;
        let record = self.load_locked(session, task_id)?;
        let deferred = self.control_in_flight_locked(task_id)?;
        Ok((record, deferred))
    }

    fn try_acquire_control(
        &self,
        session: &config::Session,
        task_id: Uuid,
    ) -> Result<(TaskRecord, Option<CloudControlLease>)> {
        let _guard = self.lock()?;
        let record = self.load_locked(session, task_id)?;
        Ok((record, self.try_acquire_control_locked(task_id)?))
    }

    fn try_acquire_control_locked(&self, task_id: Uuid) -> Result<Option<CloudControlLease>> {
        let path = self.control_lock_path(task_id);
        let directory = path.parent().context("control lock has no directory")?;
        create_private_directory(directory)?;
        validate_store_directory(directory, &std::fs::symlink_metadata(directory)?)?;
        let file = match open_existing_private_lock_file(&path) {
            Ok(file) => file,
            Err(error) if is_not_found(&error) => {
                anyhow::ensure!(
                    self.prune_control_locks_locked()? < MAX_CONTROL_LOCK_FILES,
                    "Devin Cloud control lock retention limit reached"
                );
                open_private_lock_file(&path)?
            }
            Err(error) => return Err(error),
        };
        try_control_lock(path, file)
    }

    fn prune_control_locks_locked(&self) -> Result<usize> {
        let directory = self.directory.join("control-locks");
        let mut retained = 0;
        for (index, entry) in std::fs::read_dir(&directory)?.enumerate() {
            anyhow::ensure!(
                index < MAX_CONTROL_LOCK_FILES,
                "Devin Cloud control lock directory exceeds its entry limit"
            );
            let entry = entry?;
            retained += 1;
            let name = entry.file_name();
            let Some(task_id) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".lock"))
                .and_then(|stem| Uuid::parse_str(stem).ok())
            else {
                continue;
            };
            match std::fs::symlink_metadata(self.path(task_id)) {
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("cannot inspect control lock owner"),
            }
            let path = entry.path();
            let file = open_existing_private_lock_file(&path)?;
            let Some(_lease) = try_control_lock(path.clone(), file)? else {
                continue;
            };
            // All openers hold the store lock. Keep that lock and the idle
            // inode's exclusive lease until unlink finishes, so a held lock
            // can never be replaced by a second inode for the same task.
            std::fs::remove_file(path)?;
            retained -= 1;
        }
        Ok(retained)
    }

    fn load_optional(
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
        self.write_record_locked(record)
    }

    /// Persist observer ownership and summary metadata without changing task
    /// activity or running retention pruning as a side effect.
    fn save_metadata_locked(&self, record: &TaskRecord) -> Result<()> {
        self.ensure_directory()?;
        validate_record(record)?;
        self.write_record_locked(record)
    }

    fn write_record_locked(&self, record: &TaskRecord) -> Result<()> {
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
        let previous = record.clone();
        f(&mut record)?;
        if record == previous {
            return Ok(record);
        }
        record.updated_at = config::unix_time();
        self.save_locked(&record)?;
        Ok(record)
    }

    fn update_if_snapshot_matches<F>(
        &self,
        session: &config::Session,
        expected: &TaskRecord,
        control_lease: Option<&CloudControlLease>,
        f: F,
    ) -> Result<(TaskRecord, bool)>
    where
        F: FnOnce(&mut TaskRecord) -> Result<()>,
    {
        if let Some(lease) = control_lease {
            anyhow::ensure!(
                lease.path == self.control_lock_path(expected.task_id),
                "Devin Cloud control lease does not match reconciliation task"
            );
        }
        let mut matched = false;
        let record = self.update(session, expected.task_id, |record| {
            if record.revision != expected.revision
                || record.generation != expected.generation
                || record.devin_session_id != expected.devin_session_id
                || record.org_id != expected.org_id
                || (control_lease.is_none() && self.control_in_flight_locked(record.task_id)?)
            {
                return Ok(());
            }
            matched = true;
            f(record)
        })?;
        Ok((record, matched))
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
                ensure_task_owner(&record, session)?;
                let lease = self
                    .try_acquire_control_locked(record.task_id)?
                    .context("DEVIN_TASK_CONTROL_IN_FLIGHT: a task mutation is still in flight")?;
                self.save_locked(&record)?;
                Ok(StartAcceptance::Accepted(record, lease))
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

    fn acquire_cloud_observation_lease(
        &self,
        binding: &CloudObservationBinding,
        owner_id: &str,
    ) -> Result<Option<CloudObservationLease>> {
        let _guard = self.lock()?;
        let now = config::unix_time();
        self.acquire_cloud_observation_lease_locked(binding, owner_id, now)
    }

    #[cfg(test)]
    fn acquire_cloud_observation_lease_at(
        &self,
        binding: &CloudObservationBinding,
        owner_id: &str,
        now: u64,
    ) -> Result<Option<CloudObservationLease>> {
        let _guard = self.lock()?;
        self.acquire_cloud_observation_lease_locked(binding, owner_id, now)
    }

    fn acquire_cloud_observation_lease_locked(
        &self,
        binding: &CloudObservationBinding,
        owner_id: &str,
        now: u64,
    ) -> Result<Option<CloudObservationLease>> {
        Uuid::parse_str(owner_id).context("invalid Devin Cloud observer owner ID")?;
        let mut record = match self.read_record(binding.task_id) {
            Ok(record) => record,
            Err(error) if is_not_found(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        if !binding.matches(&record)
            || record.status.is_terminal()
            || now.saturating_sub(record.updated_at) >= TASK_RETENTION_SECONDS
        {
            return Ok(None);
        }

        let current_expiry = record.lease_expires_at.unwrap_or(0);
        if record.observer_owner_id.is_some() && current_expiry > now {
            return Ok(None);
        }

        let producer_epoch = record
            .producer_epoch
            .checked_add(1)
            .context("Devin Cloud observer producer epoch overflow")?;
        let lease_expires_at = now
            .checked_add(crate::pending_interaction::CLOUD_OBSERVER_LEASE_SECS)
            .context("Devin Cloud observer lease expiry overflow")?;
        record.observer_owner_id = Some(owner_id.to_owned());
        if record.schema_version < TASK_SCHEMA_VERSION {
            if record.start_fingerprint_version == 0 {
                record.start_fingerprint_version = record.schema_version;
            }
            record.schema_version = TASK_SCHEMA_VERSION;
        }
        record.producer_epoch = producer_epoch;
        record.lease_expires_at = Some(lease_expires_at);
        self.save_metadata_locked(&record)?;
        Ok(Some(CloudObservationLease {
            owner_id: owner_id.to_owned(),
            producer_epoch,
            lease_expires_at,
            binding: binding.clone(),
        }))
    }

    fn renew_cloud_observation_lease(
        &self,
        lease: &CloudObservationLease,
    ) -> Result<Option<CloudObservationLease>> {
        let _guard = self.lock()?;
        let now = config::unix_time();
        self.renew_cloud_observation_lease_locked(lease, now)
    }

    #[cfg(test)]
    fn renew_cloud_observation_lease_at(
        &self,
        lease: &CloudObservationLease,
        now: u64,
    ) -> Result<Option<CloudObservationLease>> {
        let _guard = self.lock()?;
        self.renew_cloud_observation_lease_locked(lease, now)
    }

    fn renew_cloud_observation_lease_locked(
        &self,
        lease: &CloudObservationLease,
        now: u64,
    ) -> Result<Option<CloudObservationLease>> {
        let mut record = match self.read_record(lease.binding.task_id) {
            Ok(record) => record,
            Err(error) if is_not_found(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        if !lease.binding.matches(&record)
            || record.observer_owner_id.as_deref() != Some(lease.owner_id.as_str())
            || record.producer_epoch != lease.producer_epoch
            || record.lease_expires_at != Some(lease.lease_expires_at)
            || now >= lease.lease_expires_at
            || now.saturating_sub(record.updated_at) >= TASK_RETENTION_SECONDS
            || record.status.is_terminal()
        {
            return Ok(None);
        }
        let lease_expires_at = now
            .checked_add(crate::pending_interaction::CLOUD_OBSERVER_LEASE_SECS)
            .context("Devin Cloud observer lease expiry overflow")?;
        record.lease_expires_at = Some(lease_expires_at);
        self.save_metadata_locked(&record)?;
        Ok(Some(CloudObservationLease {
            owner_id: lease.owner_id.clone(),
            producer_epoch: lease.producer_epoch,
            lease_expires_at,
            binding: lease.binding.clone(),
        }))
    }

    fn publish_cloud_observation(
        &self,
        lease: &CloudObservationLease,
        observation: Option<&CloudPendingObservation>,
    ) -> Result<bool> {
        let _guard = self.lock()?;
        let now = config::unix_time();
        self.publish_cloud_observation_locked(lease, observation, now)
    }

    #[cfg(test)]
    fn publish_cloud_observation_at(
        &self,
        lease: &CloudObservationLease,
        observation: Option<&CloudPendingObservation>,
        now: u64,
    ) -> Result<bool> {
        let _guard = self.lock()?;
        self.publish_cloud_observation_locked(lease, observation, now)
    }

    fn publish_cloud_observation_locked(
        &self,
        lease: &CloudObservationLease,
        observation: Option<&CloudPendingObservation>,
        now: u64,
    ) -> Result<bool> {
        let mut record = match self.read_record(lease.binding.task_id) {
            Ok(record) => record,
            Err(error) if is_not_found(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        if !lease.binding.matches(&record)
            || record.status.is_terminal()
            || now.saturating_sub(record.updated_at) >= TASK_RETENTION_SECONDS
            || record.observer_owner_id.as_deref() != Some(lease.owner_id.as_str())
            || record.producer_epoch != lease.producer_epoch
            || record.lease_expires_at != Some(lease.lease_expires_at)
            || now >= lease.lease_expires_at
            || observation.is_some_and(|observation| {
                observation.observed_at < observation.read_started_at
                    || observation.read_started_at
                        < lease
                            .lease_expires_at
                            .saturating_sub(crate::pending_interaction::CLOUD_OBSERVER_LEASE_SECS)
                    || observation.observed_at > now
                    || observation.observed_at >= lease.lease_expires_at
                    || observation.read_started_at > now
            })
        {
            return Ok(false);
        }

        let summary = match observation {
            Some(observation) => Summary::observe(
                record.pending_interaction_summary.as_ref(),
                observation.state,
                observation.count,
                &observation.types,
                observation.truncated,
                ProducerKind::HostRemoteObserver,
                lease.producer_epoch,
                observation.observed_at,
            )?,
            None => Summary::unavailable(
                record.pending_interaction_summary.as_ref(),
                ProducerKind::HostRemoteObserver,
                lease.producer_epoch,
            )?,
        };
        if record
            .pending_interaction_summary
            .as_ref()
            .map(|previous| previous.summary_revision)
            != Some(summary.summary_revision)
        {
            record.revision = record.revision.saturating_add(1);
        }
        record.pending_interaction_summary = Some(summary);
        self.save_metadata_locked(&record)?;
        Ok(true)
    }

    fn release_cloud_observation_lease(&self, lease: &CloudObservationLease) -> Result<bool> {
        let _guard = self.lock()?;
        let mut record = match self.read_record(lease.binding.task_id) {
            Ok(record) => record,
            Err(error) if is_not_found(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        if !lease.binding.matches(&record)
            || record.observer_owner_id.as_deref() != Some(lease.owner_id.as_str())
            || record.producer_epoch != lease.producer_epoch
            || record.lease_expires_at != Some(lease.lease_expires_at)
        {
            return Ok(false);
        }
        record.observer_owner_id = None;
        record.lease_expires_at = None;
        self.save_metadata_locked(&record)?;
        Ok(true)
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
        let wire: Value =
            serde_json::from_slice(&bytes).context("invalid Devin Cloud task record")?;
        let schema_version = wire
            .get("schema_version")
            .and_then(Value::as_u64)
            .context("Devin Cloud task record is missing its schema version")?;
        if schema_version >= 3 {
            for key in [
                "start_fingerprint_version",
                "pending_interaction_summary",
                "observer_owner_id",
                "producer_epoch",
                "lease_expires_at",
            ] {
                anyhow::ensure!(
                    wire.get(key).is_some(),
                    "schema-3 Devin Cloud task record is missing {key}"
                );
            }
        }
        let record: TaskRecord =
            serde_json::from_value(wire).context("invalid Devin Cloud task record")?;
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
            if expired && id != current.task_id && !self.control_in_flight_locked(id)? {
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

fn open_existing_private_lock_file(path: &Path) -> Result<File> {
    reject_symlink_target(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path)?;
    validate_private_regular_file(path, &file.metadata()?)?;
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
        (MIN_TASK_SCHEMA_VERSION..=TASK_SCHEMA_VERSION).contains(&record.schema_version),
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
    validate_swe_selection(record.devin_mode.as_deref(), record.swe_tier.as_deref())?;
    if let Some(effective) = &record.effective_devin_mode {
        validate_argument(effective, "effective_devin_mode")?;
    }
    if record.schema_version >= 2 && record.start_fingerprint_version != 1 {
        match record.swe_tier.as_deref() {
            Some("priority") => {
                let requested = record
                    .devin_mode
                    .as_deref()
                    .context("priority SWE tier requires devin_mode")?;
                let effective = record
                    .effective_devin_mode
                    .as_deref()
                    .context("priority SWE tier requires effective_devin_mode")?;
                anyhow::ensure!(
                    is_swe2_priority_uid(effective, requested),
                    "priority SWE tier effective_devin_mode is not a matching account-visible priority/fast UID"
                );
            }
            Some("promo") | None => anyhow::ensure!(
                record.effective_devin_mode.as_deref() == record.devin_mode.as_deref(),
                "non-priority Devin Cloud record must preserve devin_mode as effective_devin_mode"
            ),
            Some(_) => unreachable!(),
        }
    }
    if record.schema_version >= 3 {
        anyhow::ensure!(
            (1..=TASK_SCHEMA_VERSION).contains(&record.start_fingerprint_version),
            "Devin Cloud task has an invalid start fingerprint version"
        );
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
    if let Some(summary) = &record.pending_interaction_summary {
        summary.validate()?;
        anyhow::ensure!(
            summary.producer_kind == ProducerKind::HostRemoteObserver,
            "Devin Cloud task summary must use the host remote observer producer"
        );
        anyhow::ensure!(
            summary.producer_epoch <= record.producer_epoch,
            "Devin Cloud task summary producer epoch exceeds its observer epoch"
        );
    }
    match record.observer_owner_id.as_deref() {
        Some(owner_id) => {
            Uuid::parse_str(owner_id).context("invalid Devin Cloud observer owner ID")?;
            anyhow::ensure!(
                record.producer_epoch > 0,
                "Devin Cloud observer epoch must be positive"
            );
            anyhow::ensure!(
                record.lease_expires_at.is_some(),
                "Devin Cloud observer owner is missing a lease expiry"
            );
        }
        None => anyhow::ensure!(
            record.lease_expires_at.is_none(),
            "Devin Cloud observer lease expiry is present without an owner"
        ),
    }
    anyhow::ensure!(
        record.pull_requests.len() <= MAX_PULL_REQUESTS
            && record
                .pull_requests
                .iter()
                .all(|url| url.len() <= MAX_BASE_URL_BYTES),
        "Devin Cloud task pull request list exceeds limits"
    );
    if let Some(verification) = &record.verification {
        verification.validate()?;
    }
    if let Some(delivery) = &record.delivery {
        delivery.validate()?;
    }
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

fn validate_swe_selection(devin_mode: Option<&str>, swe_tier: Option<&str>) -> Result<()> {
    let Some(tier) = swe_tier else {
        return Ok(());
    };
    anyhow::ensure!(
        matches!(tier, "promo" | "priority"),
        "swe_tier must be one of promo, priority"
    );
    anyhow::ensure!(
        devin_mode.is_some_and(is_swe2_mode),
        "swe_tier is only valid with devin_mode swe-2-medium, swe-2-high, or swe-2-max"
    );
    Ok(())
}

fn normalized_model_uid(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| match character {
            '.' | '_' => '-',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

fn is_swe2_priority_uid(candidate: &str, requested_mode: &str) -> bool {
    if !is_swe2_mode(requested_mode) {
        return false;
    }
    let normalized = normalized_model_uid(candidate);
    let effort = requested_mode.trim_start_matches("swe-2-");
    let tokens = normalized.split('-').collect::<Vec<_>>();
    (normalized.starts_with("swe-2-") || normalized.contains("-swe-2-"))
        && tokens.contains(&effort)
        && tokens
            .iter()
            .any(|token| matches!(*token, "priority" | "fast"))
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

#[allow(clippy::too_many_arguments)]
fn start_request_fingerprint(
    schema_version: u64,
    task_id: Uuid,
    task: &str,
    title: Option<&str>,
    devin_mode: Option<&str>,
    swe_tier: Option<&str>,
    effective_devin_mode: Option<&str>,
    repos: &[String],
    max_acu_limit: Option<u64>,
) -> Result<Uuid> {
    if schema_version == 1 {
        return fingerprint(&json!({
            "kind": "start",
            "task_id": task_id,
            "task": task,
            "title": title,
            "devin_mode": devin_mode,
            "repos": repos,
            "max_acu_limit": max_acu_limit,
        }));
    }
    fingerprint(&json!({
        "kind": "start",
        "task_id": task_id,
        "task": task,
        "title": title,
        "devin_mode": devin_mode,
        "swe_tier": swe_tier,
        "effective_devin_mode": effective_devin_mode,
        "repos": repos,
        "max_acu_limit": max_acu_limit,
    }))
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

impl CloudPendingObserver {
    pub(crate) fn new() -> Self {
        Self {
            owner_id: Uuid::new_v4().to_string(),
            leases: Arc::new(AsyncMutex::new(std::collections::HashMap::new())),
            jobs: Arc::new(AsyncMutex::new(std::collections::HashMap::new())),
            session_gates: Arc::new(AsyncMutex::new(std::collections::HashMap::new())),
            retired_sessions: Arc::new(AsyncMutex::new(std::collections::HashMap::new())),
        }
    }

    async fn session_gate(&self, session_id: &str) -> Arc<AsyncMutex<()>> {
        let mut gates = self.session_gates.lock().await;
        Arc::clone(
            gates
                .entry(session_id.to_owned())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }

    async fn is_retired(&self, owner: &SessionInstance) -> bool {
        self.retired_sessions
            .lock()
            .await
            .get(&owner.id)
            .is_some_and(|retired| retired.contains(owner))
    }

    pub(crate) async fn observe_session_tasks(&self, session: &config::Session) -> Result<()> {
        #[cfg(not(unix))]
        anyhow::bail!("Devin Cloud pending observer requires Unix cross-process flock");

        #[cfg(unix)]
        {
            let owner = SessionInstance::from_session(session);
            ensure_current_active_instance(&owner, session).await?;
            let gate = self.session_gate(&session.id).await;
            let _gate = gate.lock().await;
            {
                let mut retired = self.retired_sessions.lock().await;
                let instances = retired.entry(session.id.clone()).or_default();
                if instances.contains(&owner) {
                    return Ok(());
                }
                instances.clear();
            }
            ensure_current_active_instance(&owner, session).await?;
            let store = TaskStore::default_store()?;
            let (records, _) = store.list_owned(session)?;
            let candidates = records
                .into_iter()
                .filter(|record| {
                    !record.status.is_terminal()
                        && record.devin_session_id.is_some()
                        && config::unix_time().saturating_sub(record.updated_at)
                            < TASK_RETENTION_SECONDS
                })
                .collect::<Vec<_>>();
            let active_ids = candidates
                .iter()
                .map(|record| record.task_id)
                .collect::<std::collections::HashSet<_>>();
            let stale_jobs = {
                let mut jobs = self.jobs.lock().await;
                jobs.retain(|_, job| !job.handle.is_finished());
                let stale_ids = jobs
                    .iter()
                    .filter_map(|(task_id, job)| {
                        ((job.owner.id == owner.id && job.owner != owner)
                            || (job.owner == owner && !active_ids.contains(task_id)))
                        .then_some(*task_id)
                    })
                    .collect::<Vec<_>>();
                stale_ids
                    .into_iter()
                    .filter_map(|task_id| jobs.remove(&task_id))
                    .collect::<Vec<_>>()
            };
            for job in stale_jobs {
                job.handle.abort();
                let _ = job.handle.await;
            }
            let stale = {
                let mut leases = self.leases.lock().await;
                let stale_ids = leases
                    .iter()
                    .filter_map(|(task_id, lease)| {
                        (lease.binding.owner == owner && !active_ids.contains(task_id))
                            .then_some(*task_id)
                    })
                    .collect::<Vec<_>>();
                stale_ids
                    .into_iter()
                    .filter_map(|task_id| leases.remove(&task_id))
                    .collect::<Vec<_>>()
            };
            let mut failed_releases = Vec::new();
            let mut first_release_error = None;
            for lease in stale {
                if let Err(error) = store.release_cloud_observation_lease(&lease) {
                    if first_release_error.is_none() {
                        first_release_error = Some(error);
                    }
                    failed_releases.push(lease);
                }
            }
            if !failed_releases.is_empty() {
                self.leases.lock().await.extend(
                    failed_releases
                        .into_iter()
                        .map(|lease| (lease.binding.task_id, lease)),
                );
            }
            if let Some(error) = first_release_error {
                return Err(error).context("failed to retire stale Devin Cloud observer leases");
            }
            if candidates.is_empty() {
                return Ok(());
            }

            // Config errors are handled per task after acquiring its observer
            // lease. Only a generic unavailable state is persisted.
            let api = connect().ok().map(|(_, api)| api);
            for record in candidates {
                let task_id = record.task_id;
                let mut jobs = self.jobs.lock().await;
                if jobs.contains_key(&task_id) {
                    continue;
                }
                let observer = self.clone();
                let store = store.clone();
                let session = session.clone();
                let api = api.clone();
                let owner = SessionInstance::from_session(&session);
                let job_observer = observer.clone();
                let handle = tokio::spawn(async move {
                    let _ = observe_one_cloud_task(observer, session, store, record, api).await;
                    job_observer.finish_task(task_id).await;
                });
                jobs.insert(task_id, CloudObserverJob { owner, handle });
            }
            Ok(())
        }
    }

    pub(crate) async fn release_session(&self, session: &config::Session) -> Result<()> {
        #[cfg(unix)]
        {
            let owner = SessionInstance::from_session(session);
            let gate = self.session_gate(&session.id).await;
            let gate_guard = gate.lock().await;
            self.retired_sessions
                .lock()
                .await
                .entry(session.id.clone())
                .or_default()
                .insert(owner.clone());
            let store = TaskStore::default_store()?;
            let cancelled = {
                let mut jobs = self.jobs.lock().await;
                let ids = jobs
                    .iter()
                    .filter_map(|(task_id, job)| (job.owner == owner).then_some(*task_id))
                    .collect::<Vec<_>>();
                ids.into_iter()
                    .filter_map(|task_id| jobs.remove(&task_id))
                    .collect::<Vec<_>>()
            };
            for job in cancelled {
                job.handle.abort();
                let _ = job.handle.await;
            }
            let cached = {
                let mut leases = self.leases.lock().await;
                let stale_ids = leases
                    .iter()
                    .filter_map(|(task_id, lease)| {
                        (lease.binding.owner == owner).then_some(*task_id)
                    })
                    .collect::<Vec<_>>();
                stale_ids
                    .into_iter()
                    .filter_map(|task_id| leases.remove(&task_id))
                    .collect::<Vec<_>>()
            };
            let mut failures = Vec::new();
            let mut first_error = None;
            for lease in cached {
                match store.release_cloud_observation_lease(&lease) {
                    Ok(_) => {}
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        failures.push(lease);
                    }
                }
            }
            if !failures.is_empty() {
                self.leases.lock().await.extend(
                    failures
                        .into_iter()
                        .map(|lease| (lease.binding.task_id, lease)),
                );
            }
            drop(gate_guard);
            if let Some(error) = first_error {
                return Err(error).context("failed to release Devin Cloud observer leases");
            }
        }
        #[cfg(not(unix))]
        {
            let _ = session;
        }
        Ok(())
    }

    async fn finish_task(&self, task_id: Uuid) {
        self.jobs.lock().await.remove(&task_id);
    }
}

async fn observe_one_cloud_task(
    observer: CloudPendingObserver,
    session: config::Session,
    store: TaskStore,
    record: TaskRecord,
    api: Option<CloudApi>,
) -> Result<()> {
    let Some(binding) = CloudObservationBinding::from_record(&record) else {
        return Ok(());
    };

    // Wait for the shared host slot before taking a durable lease. Queued
    // workers hold neither a TaskStore lock nor a lease, so contention cannot
    // consume the 15-second lease lifetime or delay the supervisor refresh.
    let permit = if api.is_some() {
        match tokio::time::timeout(
            Duration::from_secs(pending_interaction::REFRESH_INTERVAL_SECS),
            pending_interaction::acquire_host_semaphore_permit(),
        )
        .await
        {
            Ok(permit) => Some(permit?),
            Err(_) => return Ok(()),
        }
    } else {
        None
    };

    let lease = {
        let gate = observer.session_gate(&session.id).await;
        let _gate = gate.lock().await;
        if observer.is_retired(&binding.owner).await {
            return Ok(());
        }
        ensure_current_active_instance(&binding.owner, &session).await?;
        let previous = observer.leases.lock().await.get(&record.task_id).cloned();
        let mut lease = match previous {
            Some(previous) if previous.binding == binding => {
                match store.renew_cloud_observation_lease(&previous) {
                    Ok(Some(renewed)) => Some(renewed),
                    Ok(None) => {
                        let mut leases = observer.leases.lock().await;
                        if leases.get(&record.task_id) == Some(&previous) {
                            leases.remove(&record.task_id);
                        }
                        None
                    }
                    Err(error) => return Err(error),
                }
            }
            Some(previous) => {
                store.release_cloud_observation_lease(&previous)?;
                let mut leases = observer.leases.lock().await;
                if leases.get(&record.task_id) == Some(&previous) {
                    leases.remove(&record.task_id);
                }
                None
            }
            None => None,
        };
        if lease.is_none() {
            lease = store.acquire_cloud_observation_lease(&binding, &observer.owner_id)?;
        }
        let Some(lease) = lease else {
            return Ok(());
        };
        observer
            .leases
            .lock()
            .await
            .insert(record.task_id, lease.clone());
        lease
    };

    let observation = match (api, permit) {
        (Some(api), Some(permit)) => match read_cloud_status(&api, &lease.binding, permit).await {
            Ok((status, read_started_at, observed_at)) => {
                classify_cloud_pending_status(&status, &lease.binding)
                    .ok()
                    .map(|state| CloudPendingObservation {
                        state,
                        count: (state == SummaryState::None).then_some(0),
                        types: if state == SummaryState::Pending {
                            vec![InteractionType::Approval]
                        } else {
                            Vec::new()
                        },
                        truncated: false,
                        read_started_at,
                        observed_at,
                    })
            }
            Err(_) => None,
        },
        _ => None,
    };

    // Fence responses that return after this exact session instance stopped or
    // changed. TaskStore then rechecks the binding, epoch and lease atomically.
    let gate = observer.session_gate(&session.id).await;
    let _gate = gate.lock().await;
    if observer.is_retired(&lease.binding.owner).await {
        return Ok(());
    }
    if ensure_current_active_instance(&lease.binding.owner, &session)
        .await
        .is_err()
    {
        let _ = store.release_cloud_observation_lease(&lease);
        observer.leases.lock().await.remove(&record.task_id);
        return Ok(());
    }
    let _ = store.publish_cloud_observation(&lease, observation.as_ref())?;
    Ok(())
}

async fn read_cloud_status(
    api: &CloudApi,
    binding: &CloudObservationBinding,
    permit: pending_interaction::HostObservationPermit,
) -> Result<(CloudStatusRead, u64, u64)> {
    let read_started_at = config::unix_time();
    let result = tokio::time::timeout(
        Duration::from_secs(pending_interaction::SCOPED_READ_TIMEOUT_SECS),
        api.get_session_status(&binding.org_id, &binding.remote_session_resource_id),
    )
    .await
    .context("Devin Cloud status read timed out")?
    .map_err(anyhow::Error::from);
    drop(permit);
    let observed_at = config::unix_time();
    let response = result?;
    Ok((response, read_started_at, observed_at))
}

fn classify_cloud_pending_status(
    response: &CloudStatusRead,
    binding: &CloudObservationBinding,
) -> Result<SummaryState> {
    let session_id = response
        .session_id
        .as_deref()
        .context("Devin Cloud status response omitted its session identity")?;
    anyhow::ensure!(
        session_id == binding.remote_session_resource_id,
        "Devin Cloud status response session identity changed"
    );
    if response
        .status_detail
        .as_deref()
        .is_some_and(|detail| detail.len() > 64)
    {
        anyhow::bail!("Devin Cloud status detail exceeded its bound");
    }
    let Some(status) = response.status.as_deref() else {
        return Ok(SummaryState::Unknown);
    };
    let detail = response.status_detail.as_deref();
    if status == "running" && detail == Some("waiting_for_approval") {
        return Ok(SummaryState::Pending);
    }
    let known_detail = detail.is_none_or(|detail| {
        matches!(
            detail,
            "waiting_for_user"
                | "finished"
                | "working"
                | "user_request"
                | "inactivity"
                | "billing_limit"
        )
    });
    let recognized = match status {
        "new" | "claimed" | "resuming" => detail.is_none(),
        "running" => known_detail,
        "exit" | "error" => detail.is_none_or(|detail| detail == "finished"),
        "suspended" => detail
            .is_none_or(|detail| matches!(detail, "user_request" | "inactivity" | "billing_limit")),
        _ => false,
    };
    Ok(if recognized {
        SummaryState::None
    } else {
        SummaryState::Unknown
    })
}

fn task_view(record: &TaskRecord, evidence_ref: Option<&evidence::EvidenceRef>) -> Value {
    json!({
        "task_id": record.task_id,
        "backend": "devin_cloud",
        "status": record.status.as_str(),
        "revision": record.revision,
        "generation": record.generation,
        "execution": outcome::execution_view(record.task_id, record.generation, record.status.as_str()),
        "verification": outcome::verification_view(record.verification.as_ref(), record.revision),
        "delivery": outcome::delivery_view(record.delivery.as_ref()),
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
        "report_source": record.report.as_ref().map(|_| "native_structured_output"),
        "report_capability": "structured_output_required",
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

fn deferred_task_view(record: &TaskRecord, after_revision: Option<u64>) -> Value {
    let mut view = if after_revision == Some(record.revision) {
        not_modified_view(record)
    } else {
        task_view(record, None)
    };
    view["last_updated_at"] = json!(record.updated_at);
    view["reconciliation_deferred"] = json!(true);
    view
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
    report_contract::validate(report, ReportProfile::TaskReport)
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

#[cfg(test)]
fn semantic_task_state_changed(previous: &TaskRecord, record: &TaskRecord) -> bool {
    record.status != previous.status
        || record.remote_status != previous.remote_status
        || record.remote_status_detail != previous.remote_status_detail
        || record.session_url != previous.session_url
        || record.acus_consumed_milli != previous.acus_consumed_milli
        || record.pull_requests != previous.pull_requests
        || record.report != previous.report
        || record.last_error != previous.last_error
}

async fn reconcile(
    session: &config::Session,
    owner: &SessionInstance,
    store: &TaskStore,
    api: &CloudApi,
    record: TaskRecord,
    after_revision: Option<u64>,
    control_lease: Option<&CloudControlLease>,
) -> Result<(TaskRecord, Option<evidence::EvidenceRef>, bool)> {
    let Some(devin_session_id) = record.devin_session_id.clone() else {
        // Accepted but never bound: the create call failed or crashed before
        // the session ID was persisted. The task text is not retained, so this
        // cannot be re-driven safely.
        let (record, matched) =
            store.update_if_snapshot_matches(session, &record, control_lease, |record| {
                if record.status.is_terminal() {
                    return Ok(());
                }
                let previous = record.clone();
                record.status = TaskStatus::ReconciliationRequired;
                let start_operation = record
                    .operations
                    .iter()
                    .find(|receipt| receipt.action == "start")
                    .map(|receipt| receipt.operation_id);
                if let Some(operation_id) = start_operation {
                    update_operation_receipt(record, operation_id, OperationPhase::Accepted);
                }
                if *record != previous {
                    record.revision = record.revision.saturating_add(1);
                    if let Some(operation_id) = start_operation {
                        update_operation_receipt(record, operation_id, OperationPhase::Accepted);
                    }
                }
                Ok(())
            })?;
        return Ok((record, None, !matched));
    };
    let remote = api
        .get_session(&record.org_id, &devin_session_id)
        .await
        .map_err(anyhow::Error::from)
        .and_then(|value| parse_remote_session(&value));
    let remote = match remote {
        Ok(remote) => remote,
        Err(error) => {
            let (record, matched) =
                store.update_if_snapshot_matches(session, &record, control_lease, |record| {
                    if record.status.is_terminal() {
                        return Ok(());
                    }
                    let last_error = Some(bound_text(&format!("{error:#}"), MAX_ERROR_BYTES));
                    if record.status == TaskStatus::Unknown && record.last_error == last_error {
                        return Ok(());
                    }
                    record.status = TaskStatus::Unknown;
                    record.last_error = last_error;
                    record.revision = record.revision.saturating_add(1);
                    Ok(())
                })?;
            return Ok((record, None, !matched));
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
    let (record, matched) =
        store.update_if_snapshot_matches(session, &record, control_lease, |record| {
            if record.status.is_terminal() {
                return Ok(());
            }
            let previous = record.clone();
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
            // Compare the normalized retained state before advancing the cursor.
            // Polling alone must not invalidate verification or extend retention.
            if *record == previous {
                return Ok(());
            }
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
    if !matched {
        return Ok((record, None, true));
    }
    let mut evidence_ref = None;
    if record.status.is_terminal() && after_revision != Some(record.revision) {
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
        evidence_ref = evidence::store_for_session(
            session,
            serde_json::to_string(&payload).unwrap_or_default(),
        )
        .ok()
        .flatten();
    }
    Ok((record, evidence_ref, false))
}
async fn drain_catalog_stream_bounded<R>(
    mut reader: R,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut captured = Vec::with_capacity(limit.min(64 * 1024));
    let mut overflow = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if captured.len() < limit {
            let remaining = limit - captured.len();
            let keep = remaining.min(read);
            captured.extend_from_slice(&chunk[..keep]);
            overflow |= keep < read;
        } else {
            overflow = true;
        }
    }
    Ok((captured, overflow))
}

fn parse_devin_model_catalog(bytes: &[u8]) -> Result<Value> {
    anyhow::ensure!(
        bytes.len() <= MAX_DEVIN_MODEL_CATALOG_BYTES,
        "DEVIN_SWE_PRIORITY_CATALOG_TOO_LARGE: Devin model catalog exceeded the bounded capture limit"
    );
    serde_json::from_slice(bytes)
        .context("DEVIN_SWE_PRIORITY_CATALOG_INVALID: devin models list returned invalid JSON")
}

fn collect_model_uids(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_model_uids(item, out);
            }
        }
        Value::Object(object) => {
            for key in [
                "id",
                "uid",
                "model",
                "model_id",
                "modelId",
                "model_uid",
                "modelUid",
            ] {
                if let Some(uid) = object.get(key).and_then(Value::as_str)
                    && !uid.is_empty()
                    && uid.len() <= MAX_ARGUMENT_BYTES
                {
                    out.insert(uid.to_owned());
                }
            }
            for child in object.values() {
                collect_model_uids(child, out);
            }
        }
        _ => {}
    }
}

fn resolve_swe2_priority_uid(catalog: &Value, requested_mode: &str) -> Result<String> {
    anyhow::ensure!(
        is_swe2_mode(requested_mode),
        "DEVIN_SWE_PRIORITY_INVALID_MODE: priority requires a SWE-2 devin_mode"
    );
    let mut uids = BTreeSet::new();
    collect_model_uids(catalog, &mut uids);
    let candidates = uids
        .into_iter()
        .filter(|uid| is_swe2_priority_uid(uid, requested_mode))
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [uid] => Ok(uid.clone()),
        [] => anyhow::bail!(
            "DEVIN_SWE_PRIORITY_UNAVAILABLE: account model catalog exposes no selectable priority/fast UID for {requested_mode}"
        ),
        _ => anyhow::bail!(
            "DEVIN_SWE_PRIORITY_AMBIGUOUS: account model catalog exposes multiple priority/fast UIDs for {requested_mode}"
        ),
    }
}

async fn fetch_devin_model_catalog() -> Result<Value> {
    #[cfg(test)]
    if let Some(result) = FAKE_MODEL_CATALOG
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .clone()
    {
        let bytes = result.map_err(anyhow::Error::msg)?;
        return parse_devin_model_catalog(&bytes);
    }

    let binary = crate::devin_acp::resolve_devin_executable()
        .map_err(|error| anyhow::anyhow!("DEVIN_SWE_PRIORITY_DISCOVERY_FAILED: {error}"))?;
    let mut command = tokio::process::Command::new(&binary);
    command
        .arg("models")
        .arg("list")
        .arg("--format")
        .arg("json")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().with_context(|| {
        format!(
            "DEVIN_SWE_PRIORITY_DISCOVERY_FAILED: could not start {}",
            binary.display()
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .context("DEVIN_SWE_PRIORITY_DISCOVERY_FAILED: Devin catalog stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("DEVIN_SWE_PRIORITY_DISCOVERY_FAILED: Devin catalog stderr unavailable")?;

    let collected = tokio::time::timeout(DEVIN_MODEL_CATALOG_TIMEOUT, async {
        tokio::join!(
            drain_catalog_stream_bounded(stdout, MAX_DEVIN_MODEL_CATALOG_BYTES),
            drain_catalog_stream_bounded(stderr, MAX_DEVIN_MODEL_CATALOG_STDERR_BYTES),
            child.wait(),
        )
    })
    .await;
    let (stdout, stderr, status) = match collected {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!(
                "DEVIN_SWE_PRIORITY_DISCOVERY_TIMEOUT: devin models list did not finish within {} seconds",
                DEVIN_MODEL_CATALOG_TIMEOUT.as_secs()
            );
        }
    };
    let (stdout, stdout_overflow) = stdout
        .context("DEVIN_SWE_PRIORITY_DISCOVERY_FAILED: could not read Devin catalog stdout")?;
    let (stderr, stderr_overflow) = stderr
        .context("DEVIN_SWE_PRIORITY_DISCOVERY_FAILED: could not read Devin catalog stderr")?;
    let status =
        status.context("DEVIN_SWE_PRIORITY_DISCOVERY_FAILED: could not wait for Devin catalog")?;
    anyhow::ensure!(
        !stdout_overflow && !stderr_overflow,
        "DEVIN_SWE_PRIORITY_CATALOG_TOO_LARGE: devin models list output exceeded bounded capture limits"
    );
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        let hint = if stderr.contains("Not logged in") {
            " (Devin CLI is not logged in)"
        } else {
            ""
        };
        anyhow::bail!(
            "DEVIN_SWE_PRIORITY_DISCOVERY_FAILED: devin models list exited with {status}{hint}"
        );
    }
    parse_devin_model_catalog(&stdout)
}

async fn resolve_effective_devin_mode(
    devin_mode: Option<&str>,
    swe_tier: Option<&str>,
) -> Result<Option<String>> {
    validate_swe_selection(devin_mode, swe_tier)?;
    match swe_tier {
        Some("priority") => {
            let requested = devin_mode.context("priority SWE tier requires devin_mode")?;
            let catalog = fetch_devin_model_catalog().await?;
            resolve_swe2_priority_uid(&catalog, requested).map(Some)
        }
        Some("promo") | None => Ok(devin_mode.map(str::to_owned)),
        Some(_) => unreachable!(),
    }
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
    validate_swe_selection(devin_mode, swe_tier)?;
    if let Some(limit) = max_acu_limit {
        anyhow::ensure!(
            (1..=100_000).contains(&limit),
            "max_acu_limit must be within 1..=100000"
        );
    }

    ensure_current_active_instance(&owner, session).await?;
    let task_id = task_id_for_operation(session, operation_id)?;

    if let Some(existing) = store.load_optional(session, task_id)? {
        // A schema-v1 receipt predates swe_tier. Treating a new tier as an
        // exact replay would silently ignore the caller's requested service
        // lane, because the legacy fingerprint cannot contain that field.
        let fingerprint_version = if existing.start_fingerprint_version > 0 {
            existing.start_fingerprint_version
        } else {
            existing.schema_version
        };
        anyhow::ensure!(
            fingerprint_version >= 2 || swe_tier.is_none(),
            "OPERATION_CONFLICT: operation_id was already accepted with a different request"
        );
        let effective = if fingerprint_version >= 2 {
            existing.effective_devin_mode.as_deref()
        } else {
            None
        };
        let request_fingerprint = start_request_fingerprint(
            fingerprint_version,
            task_id,
            task,
            title,
            devin_mode,
            swe_tier,
            effective,
            &repos,
            max_acu_limit,
        )?;
        return replay_operation(&existing, operation_id, request_fingerprint);
    }

    // Priority resolution is deliberately local and happens before any Devin
    // Cloud API request. If the account-visible catalog cannot prove one exact
    // selectable priority/fast UID, start fails closed without creating a
    // hosted session or silently falling back to the promo/normal lane.
    let effective_devin_mode = resolve_effective_devin_mode(devin_mode, swe_tier).await?;
    let request_fingerprint = start_request_fingerprint(
        TASK_SCHEMA_VERSION,
        task_id,
        task,
        title,
        devin_mode,
        swe_tier,
        effective_devin_mode.as_deref(),
        &repos,
        max_acu_limit,
    )?;

    let (config, api) = connect()?;
    let org_id = resolve_org_id(&config, &api).await?;

    let now = config::unix_time();
    let mut record = TaskRecord {
        schema_version: TASK_SCHEMA_VERSION,
        start_fingerprint_version: TASK_SCHEMA_VERSION,
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
        verification: None,
        delivery: None,
        operations: Vec::new(),
        operation_tombstones: Vec::new(),
        pending_interaction_summary: None,
        observer_owner_id: None,
        producer_epoch: 0,
        lease_expires_at: None,
    };
    record.operations.push(OperationReceipt {
        operation_id,
        request_fingerprint,
        action: "start".to_owned(),
        phase: OperationPhase::Accepted,
        outcome: record.outcome(),
    });
    ensure_current_active_instance(&owner, session).await?;
    let (record, _start_lease) = match store.accept_start(session, record)? {
        StartAcceptance::Existing(existing) => {
            return replay_operation(&existing, operation_id, request_fingerprint);
        }
        StartAcceptance::Accepted(record, lease) => (record, lease),
    };

    let mut body = json!({
        "prompt": format!("{REPORT_INSTRUCTIONS}{task}"),
        "title": record.title.clone().unwrap_or_else(|| format!("temote-mcp task {task_id}")),
        "tags": [SESSION_TAG],
        "structured_output_schema": report_contract::report_json_schema(),
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

    ensure_current_active_instance(&owner, session).await?;
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
    let (record, deferred) = store.load_for_reconciliation(session, task_id)?;
    if deferred {
        return Ok(deferred_task_view(&record, after_revision));
    }
    if record.status.is_terminal() {
        if after_revision == Some(record.revision) {
            return Ok(not_modified_view(&record));
        }
        return Ok(task_view(&record, None));
    }
    let (_config, api) = connect()?;
    let (record, evidence_ref, deferred) =
        reconcile(session, &owner, store, &api, record, after_revision, None).await?;
    if deferred {
        return Ok(deferred_task_view(&record, after_revision));
    }
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
    item["pending_interaction"] = cloud_pending_projection(record, config::unix_time());
    item
}

fn cloud_pending_projection(record: &TaskRecord, now: u64) -> Value {
    let Some(summary) = record.pending_interaction_summary.as_ref() else {
        return Summary::projection(None, now);
    };
    let observer_is_live = summary.producer_kind != ProducerKind::HostRemoteObserver
        || (record.observer_owner_id.is_some()
            && record.lease_expires_at.is_some_and(|expires| now < expires)
            && summary.producer_epoch == record.producer_epoch);
    if !observer_is_live {
        if summary.validate().is_err() {
            return json!({"state": "unavailable"});
        }
        let mut stale = summary.clone();
        stale.state = SummaryState::Unavailable;
        stale.count = None;
        stale.types.clear();
        stale.truncated = false;
        return serde_json::to_value(stale).unwrap_or_else(|_| json!({"state": "unavailable"}));
    }
    Summary::projection(Some(summary), now)
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
    let (retained, control_lease) = store.try_acquire_control(session, task_id)?;
    let control_lease = match control_lease {
        Some(lease) => lease,
        None => {
            if retained
                .operations
                .iter()
                .any(|receipt| receipt.operation_id == operation_id)
                || retained
                    .operation_tombstones
                    .iter()
                    .any(|receipt| receipt.operation_id == operation_id)
            {
                return replay_operation(&retained, operation_id, request_fingerprint);
            }
            anyhow::bail!(
                "DEVIN_TASK_CONTROL_IN_FLIGHT: a task mutation is still in flight; retry with the same operation_id"
            );
        }
    };
    let mut record =
        match store.accept_control(session, task_id, operation_id, request_fingerprint, action)? {
            ControlAcceptance::Replay(result) => return Ok(result),
            ControlAcceptance::Accepted(record) => *record,
        };
    let (_config, api) = connect()?;

    if record.devin_session_id.is_none() {
        let (reconciled, _, deferred) = reconcile(
            session,
            &owner,
            store,
            &api,
            record,
            None,
            Some(&control_lease),
        )
        .await?;
        record = reconciled;
        if deferred {
            return Ok(deferred_task_view(&record, None));
        }
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

    ensure_current_active_instance(&owner, session).await?;
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
type FakeModelCatalogResult = Result<Vec<u8>, String>;
#[cfg(test)]
static FAKE_MODEL_CATALOG: OnceLock<Mutex<Option<FakeModelCatalogResult>>> = OnceLock::new();

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
    get_fail: Option<ApiError>,
    self_read_pause: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
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
                    "devin_mode": body.get("devin_mode").cloned().unwrap_or(Value::Null),
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
            ("GET", ["v3", "organizations", _, "sessions", id]) => {
                if let Some(error) = &self.get_fail {
                    return Err(error.clone());
                }
                self.sessions
                    .get(*id)
                    .cloned()
                    .ok_or_else(|| ApiError::Rejected("HTTP 404".to_owned()))
            }
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
    use crate::named_roots::NamedRoots;
    use crate::supervisor::SessionSupervisor;

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
        }
    }

    fn install_fake() -> Arc<Mutex<FakeApi>> {
        let fake = Arc::new(Mutex::new(FakeApi::default()));
        *FAKE_API.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(Arc::clone(&fake));
        fake
    }

    fn clear_fake() {
        *FAKE_API.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;
        *FAKE_MODEL_CATALOG
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = None;
    }

    fn install_fake_model_catalog(value: Value) {
        *FAKE_MODEL_CATALOG
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(Ok(serde_json::to_vec(&value).unwrap()));
    }

    fn install_fake_model_catalog_error(message: &str) {
        *FAKE_MODEL_CATALOG
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(Err(message.to_owned()));
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

    async fn wait_for_cloud_summary_state(
        store: &TaskStore,
        task_id: Uuid,
        state: SummaryState,
        prior_revision: u64,
    ) -> u64 {
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(pending_interaction::REFRESH_INTERVAL_SECS.saturating_add(3));
        loop {
            if let Ok(current) = store.read_record(task_id)
                && let Some(summary) = current.pending_interaction_summary
                && summary.state == state
                && summary.summary_revision > prior_revision
            {
                return summary.summary_revision;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "supervisor did not refresh retained Devin Cloud task"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
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
                "models": [
                    {"id": "swe-2-high"},
                    {"model_uid": "swe-2-high-priority"},
                    {"id": "gpt-5-6-sol-high-priority"}
                ]
            }]
        });
        assert_eq!(
            resolve_swe2_priority_uid(&catalog, "swe-2-high").unwrap(),
            "swe-2-high-priority"
        );

        let promo_only = json!([{"id": "swe-2-high", "fast_status": {"is_active": true}}]);
        assert!(
            resolve_swe2_priority_uid(&promo_only, "swe-2-high")
                .unwrap_err()
                .to_string()
                .contains("DEVIN_SWE_PRIORITY_UNAVAILABLE")
        );

        let ambiguous = json!([
            {"id": "swe-2-high-priority"},
            {"modelUid": "swe-2-high-fast"}
        ]);
        assert!(
            resolve_swe2_priority_uid(&ambiguous, "swe-2-high")
                .unwrap_err()
                .to_string()
                .contains("DEVIN_SWE_PRIORITY_AMBIGUOUS")
        );
    }

    #[test]
    fn swe_priority_catalog_parser_rejects_malformed_and_oversized_json() {
        assert!(
            parse_devin_model_catalog(b"{")
                .unwrap_err()
                .to_string()
                .contains("DEVIN_SWE_PRIORITY_CATALOG_INVALID")
        );
        let oversized = vec![b' '; MAX_DEVIN_MODEL_CATALOG_BYTES + 1];
        assert!(
            parse_devin_model_catalog(&oversized)
                .unwrap_err()
                .to_string()
                .contains("DEVIN_SWE_PRIORITY_CATALOG_TOO_LARGE")
        );
    }

    #[test]
    fn task_record_v1_remains_readable_without_swe_fields() {
        let workspace = tempdir();
        let owner = session(&workspace, "legacy-record");
        let mut value =
            serde_json::to_value(record_for(&owner, Uuid::new_v4(), TaskStatus::Running)).unwrap();
        value["schema_version"] = json!(1);
        let object = value.as_object_mut().unwrap();
        object.remove("swe_tier");
        object.remove("effective_devin_mode");
        let legacy: TaskRecord = serde_json::from_value(value).unwrap();
        validate_record(&legacy).unwrap();
        assert_eq!(legacy.schema_version, 1);
        assert!(legacy.swe_tier.is_none());
        assert!(legacy.effective_devin_mode.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn swe_priority_resolves_catalog_uid_and_replays_without_rediscovery() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();
        install_fake_model_catalog(json!({
            "models": [
                {"id": "swe-2-high"},
                {"id": "swe-2-high-priority"}
            ]
        }));

        let operation_id = Uuid::new_v4();
        let mut args = start_args(operation_id, "priority task");
        args["devin_mode"] = json!("swe-2-high");
        args["swe_tier"] = json!("priority");
        let first = task_start_with_store(&args, &session, &store)
            .await
            .unwrap();
        assert_eq!(first["status"], "running");
        assert_eq!(first["swe_tier"], "priority");
        assert_eq!(first["devin_mode"], "swe-2-high");
        assert_eq!(first["effective_devin_mode"], "swe-2-high-priority");
        assert_eq!(
            fake.lock().unwrap().sessions["devin-0001"]["devin_mode"],
            "swe-2-high-priority"
        );

        install_fake_model_catalog_error("catalog unavailable");
        let replay = task_start_with_store(&args, &session, &store)
            .await
            .unwrap();
        assert_eq!(replay["task_id"], first["task_id"]);
        assert_eq!(fake.lock().unwrap().sessions.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn swe_priority_unavailable_fails_before_cloud_api_side_effect() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();
        install_fake_model_catalog(json!({
            "models": [{"id": "swe-2-high", "fast_status": {"is_active": true}}]
        }));

        let mut args = start_args(Uuid::new_v4(), "priority task");
        args["devin_mode"] = json!("swe-2-high");
        args["swe_tier"] = json!("priority");
        let error = task_start_with_store(&args, &session, &store)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("DEVIN_SWE_PRIORITY_UNAVAILABLE"));
        assert!(fake.lock().unwrap().calls.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn swe_promo_preserves_requested_mode_without_catalog_discovery() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();
        install_fake_model_catalog_error("must not be read");

        let mut args = start_args(Uuid::new_v4(), "promo task");
        args["devin_mode"] = json!("swe-2-max");
        args["swe_tier"] = json!("promo");
        let out = task_start_with_store(&args, &session, &store)
            .await
            .unwrap();
        assert_eq!(out["swe_tier"], "promo");
        assert_eq!(out["effective_devin_mode"], "swe-2-max");
        assert_eq!(
            fake.lock().unwrap().sessions["devin-0001"]["devin_mode"],
            "swe-2-max"
        );
    }

    #[test]
    fn swe_tier_rejects_non_swe_modes() {
        let error = validate_swe_selection(Some("fast"), Some("priority")).unwrap_err();
        assert!(error.to_string().contains("swe_tier is only valid"));
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
    async fn schema_v1_replay_rejects_new_swe_tier_before_side_effect() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();

        let operation_id = Uuid::new_v4();
        let task_id = task_id_for_operation(&session, operation_id).unwrap();
        let mut record = record_for(&session, task_id, TaskStatus::Running);
        record.schema_version = 1;
        record.title = Some("test task".to_owned());
        record.devin_mode = Some("swe-2-high".to_owned());
        record.repos = vec!["f4ah6o/temote-mcp".to_owned()];
        let request_fingerprint = start_request_fingerprint(
            1,
            task_id,
            "same task",
            Some("test task"),
            Some("swe-2-high"),
            None,
            None,
            &record.repos,
            None,
        )
        .unwrap();
        record.operations.push(OperationReceipt {
            operation_id,
            request_fingerprint,
            action: "start".to_owned(),
            phase: OperationPhase::Applied,
            outcome: record.outcome(),
        });
        store.save(&record).unwrap();

        let mut args = start_args(operation_id, "same task");
        args["devin_mode"] = json!("swe-2-high");
        args["swe_tier"] = json!("priority");
        let error = task_start_with_store(&args, &session, &store)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("OPERATION_CONFLICT"));
        assert!(fake.lock().unwrap().calls.is_empty());
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
    async fn session_stopped_during_start_preflight_is_not_accepted_or_created() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();
        // SAFETY: this test holds serial() for all Cloud environment changes.
        unsafe { std::env::remove_var(ORG_ID_ENV) };
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        fake.lock().unwrap().self_read_pause = Some((Arc::clone(&entered), Arc::clone(&release)));
        let operation_id = Uuid::new_v4();
        let args = start_args(operation_id, "work");
        let start = task_start_with_store(&args, &session, &store);
        tokio::pin!(start);
        tokio::select! {
            result = &mut start => panic!("start completed before preflight barrier: {result:?}"),
            _ = entered.notified() => {}
        }
        handle.shutdown().await.unwrap();
        release.notify_one();
        assert!(start.await.is_err());
        let task_id = task_id_for_operation(&session, operation_id).unwrap();
        assert!(store.load_optional(&session, task_id).unwrap().is_none());
        let fake = fake.lock().unwrap();
        assert_eq!(fake.calls, vec![("GET".to_owned(), "/v3/self".to_owned())]);
        assert!(fake.sessions.is_empty());
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
            &json!({"task_id": task_id, "after_revision": revision}),
            &session,
            &store,
        )
        .await
        .unwrap();
        assert_eq!(out["status"], "not_modified");
        assert_eq!(out["revision"], revision);

        fake.lock()
            .unwrap()
            .finish("devin-0001", Some(valid_report()), None);
        let out = task_get_with_store(&json!({"task_id": task_id}), &session, &store)
            .await
            .unwrap();
        assert_eq!(out["status"], "completed");
        assert_eq!(out["revision"], revision + 1);
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
        assert_eq!(again["revision"], out["revision"]);
        assert_eq!(fake.lock().unwrap().calls.len(), calls);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_terminal_reconcile_defers_without_evidence_for_any_cursor() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();
        let started = task_start_with_store(&start_args(Uuid::new_v4(), "work"), &session, &store)
            .await
            .unwrap();
        let task_id: Uuid = serde_json::from_value(started["task_id"].clone()).unwrap();
        let stale = store.load(&session, task_id).unwrap();
        fake.lock()
            .unwrap()
            .finish("devin-0001", Some(valid_report()), None);
        let finished = task_get_with_store(&json!({"task_id": task_id}), &session, &store)
            .await
            .unwrap();
        assert!(finished["evidence"].is_object());
        let before = store.load(&session, task_id).unwrap();
        let owner = SessionInstance::from_session(&session);
        let (_, api) = connect().unwrap();

        for after_revision in [Some(before.revision), None] {
            let (current, evidence_ref, deferred) = reconcile(
                &session,
                &owner,
                &store,
                &api,
                stale.clone(),
                after_revision,
                None,
            )
            .await
            .unwrap();
            assert_eq!(current, before);
            assert!(deferred);
            assert!(evidence_ref.is_none());
            assert_eq!(store.load(&session, task_id).unwrap(), before);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_success_error_and_unbound_reconciliation_preserve_newer_record() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();
        let started = task_start_with_store(&start_args(Uuid::new_v4(), "work"), &session, &store)
            .await
            .unwrap();
        let task_id: Uuid = serde_json::from_value(started["task_id"].clone()).unwrap();
        let stale = store.load(&session, task_id).unwrap();
        // Another observation finishes applying while this caller still holds
        // its earlier snapshot. The old backend reply must not roll it back.
        let mut latest = stale.clone();
        latest.status = TaskStatus::WaitingApproval;
        latest.remote_status = Some("running".to_owned());
        latest.remote_status_detail = Some("waiting_for_approval".to_owned());
        latest.acus_consumed_milli = Some(2000);
        latest.revision += 1;
        latest.updated_at = latest.updated_at.saturating_sub(10);
        latest.verification = Some(passed_verification_at(latest.revision));
        store.save(&latest).unwrap();
        let owner = SessionInstance::from_session(&session);
        let (_, api) = connect().unwrap();

        for failed_read in [false, true] {
            fake.lock().unwrap().get_fail =
                failed_read.then(|| ApiError::Uncertain("stale transport failure".to_owned()));
            let (current, evidence_ref, deferred) =
                reconcile(&session, &owner, &store, &api, stale.clone(), None, None)
                    .await
                    .unwrap();
            assert!(deferred);
            assert!(evidence_ref.is_none());
            assert_eq!(current, latest);
            assert_eq!(store.load(&session, task_id).unwrap(), latest);
            let full = deferred_task_view(&current, None);
            assert_eq!(full["status"], "waiting_approval");
            assert_eq!(full["revision"], latest.revision);
            assert_eq!(full["reconciliation_deferred"], true);
            assert!(full["evidence"].is_null());
            assert_eq!(full["verification"]["status"], "passed");
            assert_eq!(
                deferred_task_view(&current, Some(latest.revision)),
                json!({
                    "task_id": task_id,
                    "status": "not_modified",
                    "revision": latest.revision,
                    "last_updated_at": latest.updated_at,
                    "reconciliation_deferred": true,
                })
            );
        }

        let mut unbound = stale;
        unbound.devin_session_id = None;
        unbound.generation = 0;
        unbound.status = TaskStatus::Accepted;
        let calls = fake.lock().unwrap().calls.len();
        let (current, evidence_ref, deferred) =
            reconcile(&session, &owner, &store, &api, unbound, None, None)
                .await
                .unwrap();
        assert!(deferred);
        assert!(evidence_ref.is_none());
        assert_eq!(current, latest);
        assert_eq!(store.load(&session, task_id).unwrap(), latest);
        assert_eq!(fake.lock().unwrap().calls.len(), calls);
        assert_eq!(fake.lock().unwrap().sessions.len(), 1);
    }

    #[test]
    fn reconciliation_fence_checks_revision_generation_and_remote_binding() {
        let root = tempdir();
        let session = session(&root, "snapshot-fence");
        let store = test_store(&root);
        let task_id = Uuid::new_v4();
        let expected = cloud_observer_record(&session, task_id, TaskStatus::Running);
        for change in ["revision", "generation", "session", "organization"] {
            let mut latest = expected.clone();
            match change {
                "revision" => latest.revision += 1,
                "generation" => latest.generation += 1,
                "session" => latest.devin_session_id = Some("different-session".to_owned()),
                "organization" => latest.org_id = "different-org".to_owned(),
                _ => unreachable!(),
            }
            latest.updated_at = latest.updated_at.saturating_sub(10);
            store.save(&latest).unwrap();
            let (current, matched) = store
                .update_if_snapshot_matches(&session, &expected, None, |_| {
                    panic!("must not apply an observation after {change} changed")
                })
                .unwrap();
            assert!(!matched);
            assert_eq!(current, latest);
            assert_eq!(store.load(&session, task_id).unwrap(), latest);
        }
    }

    #[cfg(unix)]
    #[test]
    fn start_lease_fences_accepted_record_until_create_finishes() {
        let root = tempdir();
        let session = session(&root, "start-fence");
        let store = test_store(&root);
        let task_id = Uuid::new_v4();
        let record = record_for(&session, task_id, TaskStatus::Accepted);
        let lease = match store.accept_start(&session, record.clone()).unwrap() {
            StartAcceptance::Accepted(_, lease) => lease,
            StartAcceptance::Existing(_) => panic!("fresh start was already accepted"),
        };
        let (loaded, deferred) = store.load_for_reconciliation(&session, task_id).unwrap();
        assert!(deferred);
        assert_eq!(loaded, record);
        let (_, matched) = store
            .update_if_snapshot_matches(&session, &loaded, None, |_| {
                panic!("an unbound read must not reconcile an in-flight create")
            })
            .unwrap();
        assert!(!matched);
        drop(lease);
        let (loaded, deferred) = store.load_for_reconciliation(&session, task_id).unwrap();
        assert!(
            !deferred,
            "abandoned accepted starts must remain reconcilable"
        );
        assert_eq!(loaded, record);
    }

    #[cfg(unix)]
    #[test]
    fn control_lock_gc_preserves_active_inode_and_removes_idle_orphan() {
        use std::os::unix::fs::MetadataExt;

        let root = tempdir();
        let session = session(&root, "control-lock-gc");
        let store = test_store(&root);
        let task_id = Uuid::new_v4();
        let mut old = cloud_observer_record(&session, task_id, TaskStatus::Running);
        old.updated_at = config::unix_time().saturating_sub(TASK_RETENTION_SECONDS + 1);
        old.created_at = old.updated_at;
        store.save(&old).unwrap();
        let (_, lease) = store.try_acquire_control(&session, task_id).unwrap();
        let lease = lease.unwrap();
        let path = store.control_lock_path(task_id);
        let inode = std::fs::metadata(&path).unwrap().ino();

        let other = cloud_observer_record(&session, Uuid::new_v4(), TaskStatus::Running);
        store.save(&other).unwrap();
        let (_, other_lease) = store.try_acquire_control(&session, other.task_id).unwrap();
        assert!(other_lease.is_some());
        assert_eq!(store.load(&session, task_id).unwrap(), old);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_cloud_control_probe(&store, task_id, true);

        drop(lease);
        let next = cloud_observer_record(&session, Uuid::new_v4(), TaskStatus::Running);
        store.save(&next).unwrap();
        assert!(store.load_optional(&session, task_id).unwrap().is_none());
        let (_, next_lease) = store.try_acquire_control(&session, next.task_id).unwrap();
        assert!(next_lease.is_some());
        assert!(!path.exists());
        assert!(store.control_lock_path(other.task_id).exists());
    }

    #[cfg(unix)]
    #[test]
    fn active_control_defers_same_revision_reads_without_blocking_crash_recovery() {
        let root = tempdir();
        let session = session(&root, "control-fence");
        let store = test_store(&root);
        let task_id = Uuid::new_v4();
        let record = cloud_observer_record(&session, task_id, TaskStatus::Running);
        store.save(&record).unwrap();
        let (_, lease) = store.try_acquire_control(&session, task_id).unwrap();
        let lease = lease.unwrap();
        let accepted = match store
            .accept_control(&session, task_id, Uuid::new_v4(), Uuid::new_v4(), "steer")
            .unwrap()
        {
            ControlAcceptance::Accepted(record) => *record,
            ControlAcceptance::Replay(_) => panic!("fresh operation replayed"),
        };
        let (loaded, deferred) = store.load_for_reconciliation(&session, task_id).unwrap();
        assert!(deferred);
        assert_eq!(loaded, accepted);
        assert!(
            store
                .try_acquire_control(&session, task_id)
                .unwrap()
                .1
                .is_none()
        );
        let (latest, matched) = store
            .update_if_snapshot_matches(&session, &loaded, None, |_| {
                panic!("a response read after control acceptance must still defer")
            })
            .unwrap();
        assert!(!matched);
        assert_eq!(latest, accepted);
        assert_eq!(store.load(&session, task_id).unwrap(), accepted);
        assert_cloud_control_probe(&store, task_id, true);

        drop(lease);
        assert_cloud_control_probe(&store, task_id, false);
        let (loaded, deferred) = store.load_for_reconciliation(&session, task_id).unwrap();
        assert!(
            !deferred,
            "a durable Accepted receipt alone must not block recovery"
        );
        assert_eq!(loaded, accepted);
        let (recovered, matched) = store
            .update_if_snapshot_matches(&session, &loaded, None, |record| {
                record.status = TaskStatus::Completed;
                record.revision += 1;
                Ok(())
            })
            .unwrap();
        assert!(matched);
        assert_eq!(recovered.status, TaskStatus::Completed);
        assert_eq!(recovered.revision, accepted.revision + 1);
    }

    #[cfg(unix)]
    fn assert_cloud_control_probe(store: &TaskStore, task_id: Uuid, expected_busy: bool) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("devin_cloud::tests::cloud_control_lease_subprocess_probe")
            .arg("--nocapture")
            .env("TEMOTE_CLOUD_CONTROL_PROBE_STORE", &store.directory)
            .env("TEMOTE_CLOUD_CONTROL_PROBE_TASK", task_id.to_string())
            .env("TEMOTE_CLOUD_CONTROL_PROBE_BUSY", expected_busy.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "control lease probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn cloud_control_lease_subprocess_probe() {
        let Some(directory) = std::env::var_os("TEMOTE_CLOUD_CONTROL_PROBE_STORE") else {
            return;
        };
        let store = TaskStore::new(PathBuf::from(directory));
        let task_id =
            Uuid::parse_str(&std::env::var("TEMOTE_CLOUD_CONTROL_PROBE_TASK").unwrap()).unwrap();
        let expected_busy: bool = std::env::var("TEMOTE_CLOUD_CONTROL_PROBE_BUSY")
            .unwrap()
            .parse()
            .unwrap();
        let _guard = store.lock().unwrap();
        assert_eq!(
            store.control_in_flight_locked(task_id).unwrap(),
            expected_busy
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unchanged_get_preserves_revision_verification_and_retention() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();
        let started = task_start_with_store(&start_args(Uuid::new_v4(), "work"), &session, &store)
            .await
            .unwrap();
        let task_id: Uuid = serde_json::from_value(started["task_id"].clone()).unwrap();
        // The first read retains usage that was absent from the start response.
        task_get_with_store(&json!({"task_id": task_id}), &session, &store)
            .await
            .unwrap();
        let mut before = store.load(&session, task_id).unwrap();
        before.updated_at = before.updated_at.saturating_sub(10);
        before.verification = Some(passed_verification_at(before.revision));
        store.save(&before).unwrap();
        let calls = fake.lock().unwrap().calls.len();

        for _ in 0..2 {
            let view = task_get_with_store(&json!({"task_id": task_id}), &session, &store)
                .await
                .unwrap();
            assert_eq!(view["revision"], before.revision);
            assert_eq!(view["verification"]["status"], "passed");
            assert_eq!(view["delivery"]["status"], "not_started");
            let compact = task_get_with_store(
                &json!({"task_id": task_id, "after_revision": before.revision}),
                &session,
                &store,
            )
            .await
            .unwrap();
            assert_eq!(compact, not_modified_view(&before));
            assert_eq!(store.load(&session, task_id).unwrap(), before);
        }
        let fake = fake.lock().unwrap();
        assert_eq!(fake.sessions.len(), 1);
        assert_eq!(fake.calls.len(), calls + 4);
        assert!(
            fake.calls[calls..]
                .iter()
                .all(|(method, _)| method == "GET")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_advances_revision_for_normalized_usage_and_report_changes() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();
        let started = task_start_with_store(&start_args(Uuid::new_v4(), "work"), &session, &store)
            .await
            .unwrap();
        let task_id = started["task_id"].clone();
        let first = task_get_with_store(&json!({"task_id": task_id}), &session, &store)
            .await
            .unwrap();
        let mut revision = first["revision"].as_u64().unwrap();

        for (acus, changed) in [(1.25, true), (1.2501, false)] {
            fake.lock().unwrap().sessions.get_mut("devin-0001").unwrap()["acus_consumed"] =
                json!(acus);
            let view = task_get_with_store(
                &json!({"task_id": task_id, "after_revision": revision}),
                &session,
                &store,
            )
            .await
            .unwrap();
            if changed {
                revision += 1;
                assert_eq!(view["status"], "running");
                assert_eq!(view["acus_consumed_milli"], 1250);
            } else {
                assert_eq!(view["status"], "not_modified");
            }
            assert_eq!(view["revision"], revision);
        }

        for summary in ["need a decision", "need a different decision"] {
            let report = json!({"status": "needs_decision", "summary": summary});
            {
                let mut fake = fake.lock().unwrap();
                let remote = fake.sessions.get_mut("devin-0001").unwrap();
                remote["status"] = json!("running");
                remote["status_detail"] = json!("waiting_for_user");
                remote["structured_output"] = report.clone();
            }
            let view = task_get_with_store(
                &json!({"task_id": task_id, "after_revision": revision}),
                &session,
                &store,
            )
            .await
            .unwrap();
            revision += 1;
            assert_eq!(view["status"], "waiting_input");
            assert_eq!(view["revision"], revision);
            assert_eq!(view["report"], report);
            let unchanged = task_get_with_store(
                &json!({"task_id": task_id, "after_revision": revision}),
                &session,
                &store,
            )
            .await
            .unwrap();
            assert_eq!(unchanged["status"], "not_modified");
            assert_eq!(unchanged["revision"], revision);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_read_errors_advance_revision_once_and_recover() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();
        let started = task_start_with_store(&start_args(Uuid::new_v4(), "work"), &session, &store)
            .await
            .unwrap();
        let task_id: Uuid = serde_json::from_value(started["task_id"].clone()).unwrap();
        let mut revision = started["revision"].as_u64().unwrap();

        for message in ["request timed out", "connection unavailable"] {
            fake.lock().unwrap().get_fail = Some(ApiError::Uncertain(message.to_owned()));
            let failed_read = task_get_with_store(
                &json!({"task_id": task_id, "after_revision": revision}),
                &session,
                &store,
            )
            .await
            .unwrap();
            revision += 1;
            assert_eq!(failed_read["status"], "unknown");
            assert_eq!(failed_read["revision"], revision);
            assert!(
                failed_read["last_error"]
                    .as_str()
                    .unwrap()
                    .contains(message)
            );
            let mut before = store.load(&session, task_id).unwrap();
            before.updated_at = before.updated_at.saturating_sub(10);
            store.save(&before).unwrap();
            let unchanged = task_get_with_store(
                &json!({"task_id": task_id, "after_revision": revision}),
                &session,
                &store,
            )
            .await
            .unwrap();
            assert_eq!(unchanged, not_modified_view(&before));
            assert_eq!(store.load(&session, task_id).unwrap(), before);
        }
        fake.lock().unwrap().get_fail = None;
        let recovered = task_get_with_store(
            &json!({"task_id": task_id, "after_revision": revision}),
            &session,
            &store,
        )
        .await
        .unwrap();
        assert_eq!(recovered["status"], "running");
        assert_eq!(recovered["revision"], revision + 1);
        assert!(recovered["last_error"].is_null());
        assert_eq!(recovered["verification"]["status"], "not_run");
        assert_eq!(recovered["delivery"]["status"], "not_started");
        assert_eq!(fake.lock().unwrap().sessions.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unbound_get_preserves_reconciliation_revision_and_receipt() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let fake = install_fake();
        let task_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let fingerprint = Uuid::new_v4();
        let mut record = record_for(&session, task_id, TaskStatus::Accepted);
        record.operations.push(OperationReceipt {
            operation_id,
            request_fingerprint: fingerprint,
            action: "start".to_owned(),
            phase: OperationPhase::Accepted,
            outcome: record.outcome(),
        });
        store.save(&record).unwrap();
        let first = task_get_with_store(
            &json!({"task_id": task_id, "after_revision": record.revision}),
            &session,
            &store,
        )
        .await
        .unwrap();
        assert_eq!(first["status"], "reconciliation_required");
        assert_eq!(first["revision"], record.revision + 1);
        let mut before = store.load(&session, task_id).unwrap();
        before.updated_at = before.updated_at.saturating_sub(10);
        store.save(&before).unwrap();
        for _ in 0..2 {
            let unchanged = task_get_with_store(
                &json!({"task_id": task_id, "after_revision": before.revision}),
                &session,
                &store,
            )
            .await
            .unwrap();
            assert_eq!(unchanged, not_modified_view(&before));
            assert_eq!(store.load(&session, task_id).unwrap(), before);
        }
        assert_eq!(before.operations[0].outcome.revision, before.revision);
        let replay = replay_operation(&before, operation_id, fingerprint).unwrap();
        assert_eq!(replay["status"], "reconciliation_required");
        assert_eq!(replay["revision"], before.revision);
        assert!(fake.lock().unwrap().calls.is_empty());
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

    #[tokio::test(flavor = "current_thread")]
    async fn http_transport_rejects_internally_constructed_http_config_before_sending_key() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let secret = "devin-secret-must-not-cross-http";
        let config = CloudConfig {
            api_key: secret.to_owned(),
            api_key_source: API_KEY_ENV,
            org_id: Some("org-test".to_owned()),
            base_url: format!("http://{address}"),
            create_as_user_id: None,
        };
        // Construct HttpApi directly to cover internal callers that bypass
        // resolve_config's https-only validation.
        let api = HttpApi::new(&config).unwrap();
        let error = api
            .get_session_status("org-test", "session-test")
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Rejected(_)));
        assert!(!error.message().contains(secret));
        assert!(
            tokio::time::timeout(Duration::from_millis(250), listener.accept())
                .await
                .is_err()
        );
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
            start_fingerprint_version: TASK_SCHEMA_VERSION,
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
            verification: None,
            delivery: None,
            operations: Vec::new(),
            operation_tombstones: Vec::new(),
            pending_interaction_summary: None,
            observer_owner_id: None,
            producer_epoch: 0,
            lease_expires_at: None,
        }
    }

    #[test]
    fn semantic_revision_ignores_observation_time_and_detects_public_change() {
        let root = tempdir();
        let owner = session(&root, "semantic-revision");
        let record = record_for(&owner, Uuid::new_v4(), TaskStatus::Running);
        let mut observed = record.clone();
        observed.updated_at += 1;
        assert!(!semantic_task_state_changed(&record, &observed));
        observed.remote_status = Some("running".to_owned());
        assert!(semantic_task_state_changed(&record, &observed));
        observed = record.clone();
        observed.status = TaskStatus::Completed;
        assert!(semantic_task_state_changed(&record, &observed));
    }

    fn cloud_observer_record(
        session: &config::Session,
        task_id: Uuid,
        status: TaskStatus,
    ) -> TaskRecord {
        let mut record = record_for(session, task_id, status);
        record.generation = 7;
        record.devin_session_id = Some(format!("remote-{}", task_id.simple()));
        record
    }

    fn cloud_observation(read_started_at: u64, observed_at: u64) -> CloudPendingObservation {
        CloudPendingObservation {
            state: SummaryState::Pending,
            count: None,
            types: vec![InteractionType::Approval],
            truncated: false,
            read_started_at,
            observed_at,
        }
    }

    fn summary_of_wire_record(record: &TaskRecord) -> Value {
        serde_json::to_value(record).unwrap()
    }

    fn run_cloud_lease_probe_child(
        store_path: &Path,
        task_id: Uuid,
        now: u64,
        owner_id: &str,
        expect_acquire: bool,
    ) {
        let store = TaskStore::new(store_path.to_path_buf());
        let record = store.read_record(task_id).unwrap();
        let binding = CloudObservationBinding::from_record(&record).unwrap();
        let lease = store
            .acquire_cloud_observation_lease_at(&binding, owner_id, now)
            .unwrap();
        assert_eq!(lease.is_some(), expect_acquire);
    }

    #[test]
    fn cloud_observer_lease_subprocess_probe() {
        let Ok(path) = std::env::var("TEMOTE_CLOUD_LEASE_PROBE_STORE") else {
            return;
        };
        let task_id =
            Uuid::parse_str(&std::env::var("TEMOTE_CLOUD_LEASE_PROBE_TASK").unwrap()).unwrap();
        let now = std::env::var("TEMOTE_CLOUD_LEASE_PROBE_NOW")
            .unwrap()
            .parse()
            .unwrap();
        let owner_id = std::env::var("TEMOTE_CLOUD_LEASE_PROBE_OWNER").unwrap();
        let expect_acquire =
            std::env::var("TEMOTE_CLOUD_LEASE_PROBE_MODE").is_ok_and(|mode| mode == "acquire");
        if let Ok(marker) = std::env::var("TEMOTE_CLOUD_LEASE_PROBE_MARKER") {
            std::fs::write(marker, b"attempting").unwrap();
        }
        run_cloud_lease_probe_child(Path::new(&path), task_id, now, &owner_id, expect_acquire);
    }

    #[cfg(unix)]
    fn assert_other_process_cannot_acquire_live_lease(
        store: &TaskStore,
        task_id: Uuid,
        now: u64,
        owner_id: &str,
    ) {
        let executable = std::env::current_exe().unwrap();
        let output = std::process::Command::new(executable)
            .arg("--exact")
            .arg("devin_cloud::tests::cloud_observer_lease_subprocess_probe")
            .arg("--nocapture")
            .env("TEMOTE_CLOUD_LEASE_PROBE_STORE", &store.directory)
            .env("TEMOTE_CLOUD_LEASE_PROBE_TASK", task_id.to_string())
            .env("TEMOTE_CLOUD_LEASE_PROBE_NOW", now.to_string())
            .env("TEMOTE_CLOUD_LEASE_PROBE_OWNER", owner_id)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "subprocess lease probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    fn assert_cloud_store_flock_blocks_competing_process(
        store: &TaskStore,
        task_id: Uuid,
        now: u64,
        owner_id: &str,
        marker: &Path,
    ) {
        let lock = store.lock().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("devin_cloud::tests::cloud_observer_lease_subprocess_probe")
            .arg("--nocapture")
            .env("TEMOTE_CLOUD_LEASE_PROBE_STORE", &store.directory)
            .env("TEMOTE_CLOUD_LEASE_PROBE_TASK", task_id.to_string())
            .env("TEMOTE_CLOUD_LEASE_PROBE_NOW", now.to_string())
            .env("TEMOTE_CLOUD_LEASE_PROBE_OWNER", owner_id)
            .env("TEMOTE_CLOUD_LEASE_PROBE_MODE", "acquire")
            .env("TEMOTE_CLOUD_LEASE_PROBE_MARKER", marker)
            .spawn()
            .unwrap();

        let marker_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !marker.exists() {
            assert!(
                std::time::Instant::now() < marker_deadline,
                "competing process did not reach the store transaction"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            child.try_wait().unwrap().is_none(),
            "competing process acquired a lease while another process held the TaskStore lock"
        );
        drop(lock);

        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "competing process failed after lock release: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record = store.read_record(task_id).unwrap();
        assert_eq!(record.observer_owner_id.as_deref(), Some(owner_id));
        assert_eq!(record.producer_epoch, 1);
    }

    #[test]
    fn cloud_observer_lease_cas_preserves_task_metadata_and_fences_old_epochs() {
        let workspace = tempdir();
        let owner = session(&workspace, "observer-cas");
        let store = test_store(&workspace);
        let task_id = Uuid::new_v4();
        let mut record = cloud_observer_record(&owner, task_id, TaskStatus::Running);
        record.revision = 19;
        record.verification = Some(passed_verification_at(19));
        record.delivery = Some(submitted_delivery());
        record.updated_at = config::unix_time();
        record.created_at = record.updated_at.saturating_sub(123);
        record.operations.push(OperationReceipt {
            operation_id: Uuid::new_v4(),
            request_fingerprint: Uuid::new_v4(),
            action: "start".to_owned(),
            phase: OperationPhase::Applied,
            outcome: record.outcome(),
        });
        let task_metadata = (
            record.status,
            record.revision,
            record.generation,
            record.created_at,
            record.updated_at,
            record.operations.clone(),
            record.operation_tombstones.clone(),
            record.verification.clone(),
            record.delivery.clone(),
        );
        store.save(&record).unwrap();
        let binding = CloudObservationBinding::from_record(&record).unwrap();
        let owner_id = Uuid::new_v4().to_string();
        let now = record.updated_at.saturating_add(1);

        let first = store
            .acquire_cloud_observation_lease_at(&binding, &owner_id, now)
            .unwrap()
            .unwrap();
        let after_acquire = store.read_record(task_id).unwrap();
        assert_eq!(
            (
                after_acquire.status,
                after_acquire.revision,
                after_acquire.generation,
                after_acquire.created_at,
                after_acquire.updated_at,
                after_acquire.operations.clone(),
                after_acquire.operation_tombstones.clone(),
                after_acquire.verification.clone(),
                after_acquire.delivery.clone(),
            ),
            task_metadata,
            "observer metadata writes must not touch task state, receipts, or retention clocks"
        );
        assert!(
            store
                .acquire_cloud_observation_lease_at(&binding, &owner_id, now + 1)
                .unwrap()
                .is_none()
        );

        let renewed = store
            .renew_cloud_observation_lease_at(&first, now + 1)
            .unwrap()
            .unwrap();
        assert_eq!(renewed.producer_epoch, first.producer_epoch);
        assert!(
            !store
                .renew_cloud_observation_lease_at(&first, now + 2)
                .unwrap()
                .is_some()
        );
        assert!(
            !store
                .publish_cloud_observation_at(
                    &first,
                    Some(&cloud_observation(now, now + 2)),
                    now + 2,
                )
                .unwrap()
        );

        assert!(
            store
                .publish_cloud_observation_at(
                    &renewed,
                    Some(&cloud_observation(now + 1, now + 2)),
                    now + 2,
                )
                .unwrap()
        );
        assert!(!store.release_cloud_observation_lease(&first).unwrap());
        let after_publish = store.read_record(task_id).unwrap();
        assert_eq!(
            task_view(&after_publish, None)["verification"]["status"],
            "not_run"
        );
        let mut expected_metadata = task_metadata.clone();
        expected_metadata.1 += 1;
        assert_eq!(
            after_publish.start_fingerprint_version,
            record.start_fingerprint_version
        );
        assert_eq!(
            (
                after_publish.status,
                after_publish.revision,
                after_publish.generation,
                after_publish.created_at,
                after_publish.updated_at,
                after_publish.operations.clone(),
                after_publish.operation_tombstones.clone(),
                after_publish.verification.clone(),
                after_publish.delivery.clone(),
            ),
            expected_metadata
        );
        assert_eq!(
            after_publish
                .pending_interaction_summary
                .as_ref()
                .unwrap()
                .producer_epoch,
            renewed.producer_epoch
        );
        let previous_summary = after_publish.pending_interaction_summary.unwrap();
        assert!(
            store
                .publish_cloud_observation_at(&renewed, None, now + 3)
                .unwrap()
        );
        let unavailable = store.read_record(task_id).unwrap();
        let unavailable_summary = unavailable.pending_interaction_summary.unwrap();
        assert_eq!(unavailable_summary.state, SummaryState::Unavailable);
        assert_eq!(
            unavailable_summary.observed_at,
            previous_summary.observed_at
        );
        assert_eq!(unavailable_summary.expires_at, previous_summary.expires_at);
        assert_eq!(
            unavailable_summary.summary_revision,
            previous_summary.summary_revision + 1
        );

        assert!(store.release_cloud_observation_lease(&renewed).unwrap());
        let next = store
            .acquire_cloud_observation_lease_at(&binding, &owner_id, now + 3)
            .unwrap()
            .unwrap();
        assert_eq!(next.producer_epoch, renewed.producer_epoch + 1);
        assert!(
            !store
                .publish_cloud_observation_at(
                    &renewed,
                    Some(&cloud_observation(now + 1, now + 2)),
                    now + 3,
                )
                .unwrap()
        );
        assert!(!store.release_cloud_observation_lease(&renewed).unwrap());

        #[cfg(unix)]
        assert_other_process_cannot_acquire_live_lease(&store, task_id, now + 4, &owner_id);

        #[cfg(unix)]
        {
            let competing_task_id = Uuid::new_v4();
            let competing = cloud_observer_record(&owner, competing_task_id, TaskStatus::Running);
            let competing_now = competing.updated_at.saturating_add(1);
            store.save(&competing).unwrap();
            assert_cloud_store_flock_blocks_competing_process(
                &store,
                competing_task_id,
                competing_now,
                &Uuid::new_v4().to_string(),
                &workspace.join("child-ready"),
            );
        }

        let expired_at = next.lease_expires_at;
        assert!(
            store
                .renew_cloud_observation_lease_at(&next, expired_at)
                .unwrap()
                .is_none()
        );
        let takeover = store
            .acquire_cloud_observation_lease_at(&binding, &owner_id, expired_at)
            .unwrap()
            .unwrap();
        assert_eq!(takeover.producer_epoch, next.producer_epoch + 1);
    }

    #[test]
    fn cloud_observer_rejects_binding_retention_and_overflow_boundaries() {
        let workspace = tempdir();
        let owner = session(&workspace, "observer-boundaries");
        let store = test_store(&workspace);
        let now = config::unix_time();
        let owner_id = Uuid::new_v4().to_string();

        let task_id = Uuid::new_v4();
        let mut record = cloud_observer_record(&owner, task_id, TaskStatus::Running);
        record.updated_at = now;
        store.save(&record).unwrap();
        let binding = CloudObservationBinding::from_record(&record).unwrap();
        let lease = store
            .acquire_cloud_observation_lease_at(&binding, &owner_id, now + 1)
            .unwrap()
            .unwrap();
        let mut rebound = store.read_record(task_id).unwrap();
        rebound.devin_session_id = Some("replacement-session".to_owned());
        rebound.generation += 1;
        store.save(&rebound).unwrap();
        assert!(
            store
                .renew_cloud_observation_lease_at(&lease, now + 2)
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .publish_cloud_observation_at(
                    &lease,
                    Some(&cloud_observation(now + 1, now + 2)),
                    now + 2,
                )
                .unwrap()
        );
        assert!(!store.release_cloud_observation_lease(&lease).unwrap());

        let retained_task = Uuid::new_v4();
        let mut retained = cloud_observer_record(&owner, retained_task, TaskStatus::Running);
        retained.updated_at = now.saturating_sub(TASK_RETENTION_SECONDS);
        store.save(&retained).unwrap();
        let retained_binding = CloudObservationBinding::from_record(&retained).unwrap();
        assert!(
            store
                .acquire_cloud_observation_lease_at(&retained_binding, &owner_id, now)
                .unwrap()
                .is_none()
        );

        let overflow_task = Uuid::new_v4();
        let mut overflow = cloud_observer_record(&owner, overflow_task, TaskStatus::Running);
        overflow.producer_epoch = u64::MAX;
        overflow.updated_at = now;
        store.save(&overflow).unwrap();
        let overflow_binding = CloudObservationBinding::from_record(&overflow).unwrap();
        assert!(
            store
                .acquire_cloud_observation_lease_at(&overflow_binding, &owner_id, now + 1)
                .unwrap_err()
                .to_string()
                .contains("epoch overflow")
        );

        let expiry_task = Uuid::new_v4();
        let mut expiry_overflow = cloud_observer_record(&owner, expiry_task, TaskStatus::Running);
        expiry_overflow.created_at = u64::MAX - 20;
        expiry_overflow.updated_at = u64::MAX - 20;
        store.save(&expiry_overflow).unwrap();
        let expiry_binding = CloudObservationBinding::from_record(&expiry_overflow).unwrap();
        assert!(
            store
                .acquire_cloud_observation_lease_at(&expiry_binding, &owner_id, u64::MAX - 3,)
                .unwrap_err()
                .to_string()
                .contains("expiry overflow")
        );

        let deleted_task = Uuid::new_v4();
        let mut deleted = cloud_observer_record(&owner, deleted_task, TaskStatus::Running);
        deleted.updated_at = now;
        store.save(&deleted).unwrap();
        let deleted_binding = CloudObservationBinding::from_record(&deleted).unwrap();
        let deleted_lease = store
            .acquire_cloud_observation_lease_at(&deleted_binding, &owner_id, now + 1)
            .unwrap()
            .unwrap();
        std::fs::remove_file(store.path(deleted_task)).unwrap();
        assert!(
            store
                .renew_cloud_observation_lease_at(&deleted_lease, now + 2)
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .publish_cloud_observation_at(
                    &deleted_lease,
                    Some(&cloud_observation(now + 1, now + 2)),
                    now + 2,
                )
                .unwrap()
        );
    }

    #[test]
    fn cloud_binding_names_remote_resource_id_without_changing_record_wire_name() {
        let workspace = tempdir();
        let owner = session(&workspace, "observer-resource-binding");
        let task_id = Uuid::new_v4();
        let record = cloud_observer_record(&owner, task_id, TaskStatus::Running);

        let wire = serde_json::to_value(&record).unwrap();
        assert_eq!(
            wire["devin_session_id"],
            record.devin_session_id.as_deref().unwrap()
        );
        assert!(wire.get("remote_session_resource_id").is_none());

        let binding = CloudObservationBinding::from_record(&record).unwrap();
        assert_eq!(
            binding.remote_session_resource_id,
            record.devin_session_id.as_deref().unwrap()
        );
        assert!(binding.matches(&record));

        let mut rebound = record.clone();
        rebound.devin_session_id = Some("different-remote-resource".to_owned());
        assert!(!binding.matches(&rebound));
    }

    #[test]
    fn cloud_pending_classifier_requires_a_consistent_running_approval_status() {
        let workspace = tempdir();
        let owner = session(&workspace, "observer-classifier");
        let record = cloud_observer_record(&owner, Uuid::new_v4(), TaskStatus::Running);
        let binding = CloudObservationBinding::from_record(&record).unwrap();

        let remote = |session_id: &str, status: &str, detail: &str| CloudStatusRead {
            session_id: Some(session_id.to_owned()),
            status: Some(status.to_owned()),
            status_detail: Some(detail.to_owned()),
        };
        assert_eq!(
            classify_cloud_pending_status(
                &remote(
                    &binding.remote_session_resource_id,
                    "running",
                    "waiting_for_approval"
                ),
                &binding,
            )
            .unwrap(),
            SummaryState::Pending
        );
        assert_eq!(
            classify_cloud_pending_status(
                &remote(
                    &binding.remote_session_resource_id,
                    "running",
                    "waiting_for_user"
                ),
                &binding,
            )
            .unwrap(),
            SummaryState::None,
            "waiting_for_user belongs to task status, not the pending summary"
        );
        assert_eq!(
            classify_cloud_pending_status(
                &remote(
                    &binding.remote_session_resource_id,
                    "future_status",
                    "waiting_for_approval"
                ),
                &binding,
            )
            .unwrap(),
            SummaryState::Unknown
        );
        assert_eq!(
            classify_cloud_pending_status(
                &remote(
                    &binding.remote_session_resource_id,
                    "exit",
                    "waiting_for_approval"
                ),
                &binding,
            )
            .unwrap(),
            SummaryState::Unknown
        );
        assert!(
            classify_cloud_pending_status(
                &remote("some-other-session", "running", "waiting_for_approval"),
                &binding,
            )
            .is_err()
        );
    }

    #[test]
    fn observer_migration_promotes_legacy_record_and_preserves_start_replay_marker() {
        let workspace = tempdir();
        let owner = session(&workspace, "observer-v1-migration");
        let store = test_store(&workspace);
        let task_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let mut legacy = cloud_observer_record(&owner, task_id, TaskStatus::Running);
        legacy.schema_version = 1;
        legacy.start_fingerprint_version = 0;
        legacy.title = Some("legacy title".to_owned());
        legacy.devin_mode = Some("fast".to_owned());
        legacy.repos = vec!["f4ah6o/temote-mcp".to_owned()];
        legacy.operations.push(OperationReceipt {
            operation_id,
            request_fingerprint: start_request_fingerprint(
                1,
                task_id,
                "legacy start",
                legacy.title.as_deref(),
                legacy.devin_mode.as_deref(),
                None,
                None,
                &legacy.repos,
                None,
            )
            .unwrap(),
            action: "start".to_owned(),
            phase: OperationPhase::Applied,
            outcome: legacy.outcome(),
        });
        store.save(&legacy).unwrap();

        // Simulate a schema-1 writer: the old format has none of the observer
        // keys, and its defaulted fields must migrate only under the store CAS.
        let mut wire = serde_json::to_value(&legacy).unwrap();
        for key in [
            "start_fingerprint_version",
            "pending_interaction_summary",
            "observer_owner_id",
            "producer_epoch",
            "lease_expires_at",
        ] {
            wire.as_object_mut().unwrap().remove(key);
        }
        std::fs::write(store.path(task_id), serde_json::to_vec(&wire).unwrap()).unwrap();
        let loaded = store.read_record(task_id).unwrap();
        let binding = CloudObservationBinding::from_record(&loaded).unwrap();
        let lease = store
            .acquire_cloud_observation_lease_at(
                &binding,
                &Uuid::new_v4().to_string(),
                config::unix_time(),
            )
            .unwrap()
            .unwrap();
        let promoted = store.read_record(task_id).unwrap();
        assert_eq!(promoted.schema_version, 3);
        assert_eq!(promoted.start_fingerprint_version, 1);
        assert_eq!(promoted.producer_epoch, 1);
        let replay_fingerprint = start_request_fingerprint(
            promoted.start_fingerprint_version,
            task_id,
            "legacy start",
            promoted.title.as_deref(),
            promoted.devin_mode.as_deref(),
            None,
            None,
            &promoted.repos,
            None,
        )
        .unwrap();
        let replay = replay_operation(&promoted, operation_id, replay_fingerprint).unwrap();
        assert_eq!(replay["task_id"], json!(task_id));
        assert_eq!(
            promoted.operations[0].request_fingerprint,
            replay_fingerprint
        );
        assert_eq!(promoted.status, legacy.status);
        assert_eq!(promoted.revision, legacy.revision);
        assert_eq!(promoted.generation, legacy.generation);
        assert_eq!(promoted.created_at, legacy.created_at);
        assert_eq!(promoted.updated_at, legacy.updated_at);
        assert_eq!(promoted.operations, legacy.operations);
        assert!(store.release_cloud_observation_lease(&lease).unwrap());
    }

    #[test]
    fn schema_three_rejects_observer_metadata_stripped_by_legacy_writer() {
        let workspace = tempdir();
        let owner = session(&workspace, "observer-schema-three");
        let store = test_store(&workspace);
        let task_id = Uuid::new_v4();
        store
            .save(&record_for(&owner, task_id, TaskStatus::Running))
            .unwrap();
        let mut wire = summary_of_wire_record(&store.read_record(task_id).unwrap());
        wire.as_object_mut().unwrap().remove("producer_epoch");
        std::fs::write(store.path(task_id), serde_json::to_vec(&wire).unwrap()).unwrap();
        assert!(
            store
                .read_record(task_id)
                .unwrap_err()
                .to_string()
                .contains("missing producer_epoch")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cloud_observer_discovers_more_than_four_tasks_without_task_get() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let session_id = test_id();
        let (handle, session) = active_test_session(&root, &session_id).await;
        let store = TaskStore::default_store().unwrap();
        let fake = install_fake();
        let mut task_ids = Vec::new();
        {
            let mut fake = fake.lock().unwrap();
            for _ in 0..6 {
                let task_id = Uuid::new_v4();
                let mut record = cloud_observer_record(&session, task_id, TaskStatus::Running);
                let remote_id = record.devin_session_id.clone().unwrap();
                record.updated_at = config::unix_time();
                store.save(&record).unwrap();
                fake.sessions.insert(
                    remote_id.clone(),
                    json!({
                        "session_id": remote_id,
                        "status": "running",
                        "status_detail": "waiting_for_approval",
                        "structured_output": {"should_not_be_read": "raw payload"},
                    }),
                );
                task_ids.push(task_id);
            }
        }

        let observer = CloudPendingObserver::new();
        observer.observe_session_tasks(&session).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let all_observed = task_ids.iter().all(|task_id| {
                store
                    .read_record(*task_id)
                    .ok()
                    .and_then(|record| record.pending_interaction_summary)
                    .is_some_and(|summary| summary.state == SummaryState::Pending)
            });
            if all_observed {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "not all retained tasks were observed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let calls = fake.lock().unwrap().calls.clone();
        assert_eq!(calls.len(), 6);
        assert!(calls.iter().all(|(method, path)| {
            method == "GET"
                && path.starts_with("/v3/organizations/org-test/sessions/")
                && !path.ends_with("/messages")
        }));
        let tasks = task_list_with_store(&json!({}), &session, &store).unwrap();
        assert_eq!(tasks["total"], 6);
        assert!(tasks["tasks"].as_array().unwrap().iter().all(|task| {
            task["pending_interaction"]["state"] == "pending"
                && task.get("structured_output").is_none()
        }));

        observer.release_session(&session).await.unwrap();
        let after_stop = task_list_with_store(&json!({}), &session, &store).unwrap();
        assert!(
            after_stop["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|task| { task["pending_interaction"]["state"] == "unavailable" })
        );
        for task_id in task_ids {
            let _ = std::fs::remove_file(store.path(task_id));
        }
        handle.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn old_session_release_preserves_replacement_jobs_and_leases() {
        let root = tempdir();
        let session_id = test_id();
        let (handle, old_session) = active_test_session(&root, &session_id).await;
        let new_session = config::Session {
            started_at: old_session.started_at.saturating_add(1),
            process_id: old_session.process_id.saturating_add(1),
            ..old_session.clone()
        };
        let store = TaskStore::default_store().unwrap();
        let task_id = Uuid::new_v4();
        let record = cloud_observer_record(&new_session, task_id, TaskStatus::Running);
        store.save(&record).unwrap();
        let binding = CloudObservationBinding::from_record(&record).unwrap();
        let observer = CloudPendingObserver::new();
        let lease = store
            .acquire_cloud_observation_lease_at(&binding, &observer.owner_id, config::unix_time())
            .unwrap()
            .unwrap();
        observer.leases.lock().await.insert(task_id, lease.clone());
        let job = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        observer.jobs.lock().await.insert(
            task_id,
            CloudObserverJob {
                owner: SessionInstance::from_session(&new_session),
                handle: job,
            },
        );
        let original_gate = observer.session_gate(&session_id).await;

        observer.release_session(&old_session).await.unwrap();

        assert!(
            observer
                .is_retired(&SessionInstance::from_session(&old_session))
                .await
        );
        assert!(
            !observer
                .is_retired(&SessionInstance::from_session(&new_session))
                .await
        );
        let replacement_job = observer.jobs.lock().await;
        assert!(replacement_job.contains_key(&task_id));
        assert!(!replacement_job.get(&task_id).unwrap().handle.is_finished());
        drop(replacement_job);
        assert_eq!(observer.leases.lock().await.get(&task_id), Some(&lease));
        assert_eq!(
            store
                .read_record(task_id)
                .unwrap()
                .observer_owner_id
                .as_deref(),
            Some(observer.owner_id.as_str())
        );
        assert!(Arc::ptr_eq(
            &original_gate,
            &observer.session_gate(&session_id).await
        ));

        let removed_job = observer.jobs.lock().await.remove(&task_id).unwrap();
        removed_job.handle.abort();
        let _ = removed_job.handle.await;
        observer.leases.lock().await.remove(&task_id);
        assert!(store.release_cloud_observation_lease(&lease).unwrap());
        let _ = std::fs::remove_file(store.path(task_id));
        handle.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn observer_retains_cached_lease_when_store_io_fails() {
        use std::os::unix::fs::symlink;

        let root = tempdir();
        let (handle, session) = active_test_session(&root, &test_id()).await;
        let store = test_store(&root);
        let task_id = Uuid::new_v4();
        let record = cloud_observer_record(&session, task_id, TaskStatus::Running);
        store.save(&record).unwrap();
        let binding = CloudObservationBinding::from_record(&record).unwrap();
        let observer = CloudPendingObserver::new();
        let lease = store
            .acquire_cloud_observation_lease_at(&binding, &observer.owner_id, config::unix_time())
            .unwrap()
            .unwrap();
        observer.leases.lock().await.insert(task_id, lease.clone());
        let lock_path = store.directory.join(".store.lock");
        std::fs::remove_file(&lock_path).unwrap();
        symlink("/dev/null", &lock_path).unwrap();
        let fake = Arc::new(Mutex::new(FakeApi::default()));

        assert!(
            observe_one_cloud_task(
                observer.clone(),
                session.clone(),
                store.clone(),
                record.clone(),
                Some(CloudApi::Fake(fake.clone())),
            )
            .await
            .is_err()
        );
        assert_eq!(observer.leases.lock().await.get(&task_id), Some(&lease));

        let mut rebound = record;
        rebound.generation += 1;
        assert!(
            observe_one_cloud_task(
                observer.clone(),
                session,
                store.clone(),
                rebound,
                Some(CloudApi::Fake(fake)),
            )
            .await
            .is_err()
        );
        assert_eq!(observer.leases.lock().await.get(&task_id), Some(&lease));

        std::fs::remove_file(lock_path).unwrap();
        assert!(store.release_cloud_observation_lease(&lease).unwrap());
        handle.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supervisor_reobserves_retained_cloud_tasks_on_its_interval() {
        let _serial = serial().await;
        let _env = EnvGuard::install();
        let root = tempdir();
        let repo = root.join("volume").join("repo-a");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical_root = config::canonical_directory(root.join("volume").as_path()).unwrap();
        let roots = NamedRoots::from_canonical_roots(std::collections::BTreeMap::from([(
            "src".to_owned(),
            canonical_root,
        )]))
        .unwrap();
        let fake = install_fake();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let session_id = test_id();
        let managed = supervisor
            .start(
                &format!("src/{}", repo.file_name().unwrap().to_string_lossy()),
                Some(&session_id),
            )
            .await
            .unwrap();
        let session = config::read_session_metadata(&managed.session_id)
            .await
            .unwrap();
        let store = TaskStore::default_store().unwrap();
        let task_id = Uuid::new_v4();
        let mut record = cloud_observer_record(&session, task_id, TaskStatus::Running);
        record.updated_at = config::unix_time();
        let original_task_metadata = (record.status, record.revision, record.updated_at);
        let remote_id = record.devin_session_id.clone().unwrap();
        fake.lock().unwrap().sessions.insert(
            remote_id.clone(),
            json!({
                "session_id": remote_id,
                "status": "running",
                "status_detail": "working",
            }),
        );
        store.save(&record).unwrap();

        let first_revision =
            wait_for_cloud_summary_state(&store, task_id, SummaryState::None, 0).await;
        assert_eq!(first_revision, 1);
        fake.lock().unwrap().sessions.get_mut(&remote_id).unwrap()["status_detail"] =
            json!("waiting_for_approval");
        let pending_revision =
            wait_for_cloud_summary_state(&store, task_id, SummaryState::Pending, first_revision)
                .await;
        assert_eq!(pending_revision, first_revision + 1);
        fake.lock().unwrap().sessions.get_mut(&remote_id).unwrap()["status_detail"] =
            json!("working");
        let none_revision =
            wait_for_cloud_summary_state(&store, task_id, SummaryState::None, pending_revision)
                .await;
        assert_eq!(none_revision, pending_revision + 1);

        let current = store.read_record(task_id).unwrap();
        assert_eq!(current.status, original_task_metadata.0);
        assert_eq!(current.revision, original_task_metadata.1 + 3);
        assert_eq!(current.updated_at, original_task_metadata.2);
        let calls = fake.lock().unwrap().calls.clone();
        assert!(calls.len() >= 3);
        assert!(calls.iter().all(|(method, path)| {
            method == "GET"
                && path.starts_with("/v3/organizations/org-test/sessions/")
                && !path.ends_with("/messages")
        }));

        supervisor.shutdown().await.unwrap();
        let _ = std::fs::remove_file(store.path(task_id));
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

    // ---------- separated execution / verification / delivery state (A4) ----------

    fn passed_verification_at(record_revision: u64) -> VerificationRecord {
        VerificationRecord {
            status: outcome::VerificationStatus::Passed,
            target: outcome::VerificationTarget::Commit {
                commit: "abc123".to_owned(),
            },
            record_revision,
            checked_at: 1_700_000_000,
        }
    }

    fn submitted_delivery() -> DeliveryRecord {
        DeliveryRecord {
            status: outcome::DeliveryStatus::Submitted,
            branch: Some("feat/a4".to_owned()),
            pull_request: Some("https://example.invalid/pr/1".to_owned()),
            updated_at: 1_700_000_001,
        }
    }

    #[test]
    fn task_view_separates_execution_from_verification_and_delivery() {
        let workspace = tempdir();
        let owner = session(&workspace, "state-separation");
        let task_id = Uuid::new_v4();
        let mut record = record_for(&owner, task_id, TaskStatus::Completed);

        // A completed execution alone is not a verification PASS and not a
        // delivery.
        let view = task_view(&record, None);
        assert_eq!(view["status"], "completed");
        assert_eq!(view["execution"]["state"], "completed");
        assert_eq!(
            view["execution"]["id"],
            outcome::execution_id(task_id, record.generation).to_string()
        );
        assert_eq!(view["verification"]["status"], "not_run");
        assert_eq!(view["delivery"]["status"], "not_started");

        record.verification = Some(passed_verification_at(record.revision));
        record.delivery = Some(submitted_delivery());
        let view = task_view(&record, None);
        assert_eq!(view["verification"]["status"], "passed");
        assert_eq!(view["delivery"]["status"], "submitted");
    }

    #[test]
    fn agent_reported_pull_requests_do_not_set_delivery_state() {
        let workspace = tempdir();
        let owner = session(&workspace, "delivery-separation");
        let mut record = record_for(&owner, Uuid::new_v4(), TaskStatus::Completed);
        record.pull_requests = vec!["https://example.invalid/org/repo/pull/1".to_owned()];

        let view = task_view(&record, None);
        assert_eq!(view["status"], "completed");
        assert_eq!(
            view["delivery"]["status"], "not_started",
            "an agent-reported pull request is not a Temote delivery operation"
        );
        assert_eq!(view["delivery"]["pull_request"], Value::Null);
    }

    #[test]
    fn legacy_records_without_outcome_fields_read_as_not_run() {
        let workspace = tempdir();
        let owner = session(&workspace, "legacy-state");
        let record = record_for(&owner, Uuid::new_v4(), TaskStatus::Completed);

        let mut value = serde_json::to_value(&record).unwrap();
        let object = value.as_object_mut().unwrap();
        assert!(object.remove("verification").is_some());
        assert!(object.remove("delivery").is_some());
        let legacy: TaskRecord = serde_json::from_value(value).unwrap();
        assert!(legacy.verification.is_none());
        assert!(legacy.delivery.is_none());

        let view = task_view(&legacy, None);
        assert_eq!(view["verification"]["status"], "not_run");
        assert_eq!(view["delivery"]["status"], "not_started");
    }

    #[test]
    fn stale_verification_is_not_reported_as_a_current_pass() {
        let workspace = tempdir();
        let owner = session(&workspace, "stale-state");
        let mut record = record_for(&owner, Uuid::new_v4(), TaskStatus::Completed);
        record.revision = 2;
        record.verification = Some(passed_verification_at(1));

        let view = task_view(&record, None);
        assert_eq!(view["verification"]["status"], "not_run");
        assert_eq!(view["verification"]["stale"], true);
        assert_eq!(view["verification"]["target"]["commit"], "abc123");
    }
}
