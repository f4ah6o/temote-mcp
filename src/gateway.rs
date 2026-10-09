use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

use crate::{approvals, config, host_identity, mcp, session_control};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(40);
const OBSERVATION_SYNC_TIMEOUT: Duration = Duration::from_secs(8);
const OBSERVATION_SYNC_INTERVAL: Duration = Duration::from_secs(5);
const MAX_GATEWAY_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_GATEWAY_SYNC_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_GATEWAY_POLL_ENVELOPE_BYTES: usize = 64 * 1024;
const MAX_GATEWAY_POLL_RESPONSE_BYTES: usize =
    MAX_GATEWAY_RESPONSE_BYTES + MAX_GATEWAY_POLL_ENVELOPE_BYTES;
const _: () = assert!(MAX_GATEWAY_POLL_ENVELOPE_BYTES >= 64 * 1024);
const MAX_GATEWAY_ERROR_BYTES: usize = 64 * 1024;
const MAX_GATEWAY_ERROR_DISPLAY_CHARS: usize = 4096;
const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_secs(2);
const MIN_EMPTY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MAX_CONCURRENT_GATEWAY_REQUESTS: usize = 8;
const CAPACITY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const RESPONSE_UPLOAD_ATTEMPTS: usize = 3;
const HOST_AGENT_PROTOCOL_VERSION: u64 = 1;
const HOST_CAPABILITIES: &[&str] = &["session_lifecycle", "session_tools", "named_roots"];
const HOST_AGENT_RECORD_SCHEMA_VERSION: u64 = 1;
const MAX_HOST_AGENT_RECORD_BYTES: usize = 4096;
const HOST_AGENT_RECORD_DIRECTORY: &str = "gateway-agents";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Auto,
    Macos,
    Linux,
    Wsl2,
    Windows,
}

impl std::str::FromStr for Platform {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "macos" => Ok(Self::Macos),
            "linux" => Ok(Self::Linux),
            "wsl2" => Ok(Self::Wsl2),
            "windows" => Ok(Self::Windows),
            _ => Err(format!(
                "unknown platform {value:?}; expected auto, macos, linux, wsl2, or windows"
            )),
        }
    }
}

impl Platform {
    fn resolve(self) -> &'static str {
        match self {
            Self::Auto => detected_platform(),
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Wsl2 => "wsl2",
            Self::Windows => "windows",
        }
    }
}

pub struct AgentOptions {
    pub gateway_url: String,
    pub session_id: Option<String>,
    pub host_id: Option<String>,
    pub host_token: String,
    pub access_client_id: Option<String>,
    pub access_client_secret: Option<String>,
    pub platform: Platform,
    pub reconnect_delay: Duration,
}

#[derive(Clone, Copy)]
struct AgentCapacity {
    limit: usize,
    heartbeat_supported: bool,
}

impl AgentCapacity {
    fn negotiated(advertised: Option<usize>) -> Self {
        Self {
            limit: advertised
                .unwrap_or(1)
                .clamp(1, MAX_CONCURRENT_GATEWAY_REQUESTS),
            heartbeat_supported: advertised.is_some(),
        }
    }
}

type GatewayRequestResult = (u64, Result<Option<GenerationExit>>);
type GatewayRequests = tokio::task::JoinSet<GatewayRequestResult>;
type CompletedGatewayRequest =
    Option<std::result::Result<GatewayRequestResult, tokio::task::JoinError>>;

enum PollOutcome {
    Idle,
    Request(Box<PollEnvelope>),
    Exit(GenerationExit),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GatewayConflict {
    StaleRequest,
    PollReplaced,
    Replaced,
}

#[derive(Clone)]
enum AgentRoute {
    Legacy {
        session_id: String,
    },
    Host {
        host_id: String,
        sessions: session_control::SessionBackend,
        metadata: HostSupervisorMetadata,
    },
}

#[derive(Clone)]
struct AgentGeneration {
    gateway: GatewayClient,
    route: AgentRoute,
    instance_id: String,
    generation: u64,
    capacity: AgentCapacity,
    admission: Arc<AtomicBool>,
    browser_authority: Option<crate::fabric_browser::FabricAuthority>,
    worker_auth: Option<crate::fabric_browser::WorkerAuthSnapshot>,
}

struct GenerationAdmissionGuard(Arc<AtomicBool>);

impl Drop for GenerationAdmissionGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// A bounded diagnostic projection. It never starts a Link or reconciles a task.
pub async fn fabric_status() -> Result<serde_json::Value> {
    Ok(crate::doctor::fabric_status_projection().await)
}

#[derive(Clone)]
pub(crate) struct GatewayClient {
    pub(crate) client: Client,
    pub(crate) sync_client: Client,
    pub(crate) base_url: String,
    pub(crate) host_token: String,
    pub(crate) access_client_id: Option<String>,
    pub(crate) access_client_secret: Option<String>,
    pub(crate) browser_auth: Option<Arc<RwLock<BrowserWireAuth>>>,
    pub(crate) browser_auth_blocked: Option<Arc<AtomicBool>>,
}

#[derive(Clone)]
pub(crate) struct BrowserWireAuth {
    access_token: String,
    host_grant: String,
    access_expires_at: u64,
}

#[derive(Serialize)]
struct LegacyConnectRequest<'a> {
    session_id: &'a str,
    instance_id: &'a str,
    platform: &'a str,
}

#[derive(Deserialize)]
struct LegacyConnectResponse {
    session_id: String,
    generation: u64,
    lease_seconds: u64,
    #[serde(default)]
    concurrent_requests: Option<usize>,
}

#[derive(Serialize)]
struct LegacyGenerationRequest<'a> {
    session_id: &'a str,
    instance_id: &'a str,
    generation: u64,
}

#[derive(Serialize)]
struct LegacyPollRequest<'a> {
    #[serde(flatten)]
    identity: LegacyGenerationRequest<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    accept_requests: Option<bool>,
}

#[derive(Serialize)]
struct HostConnectRequest<'a> {
    host_id: &'a str,
    instance_id: &'a str,
    platform: &'a str,
    agent_protocol: u64,
    runtime_version: &'a str,
    control_protocol: u64,
    capabilities: &'a [&'static str],
    named_roots: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    _fabric_auth: Option<&'a crate::fabric_browser::WorkerAuthSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HostSupervisorMetadata {
    runtime_version: String,
    control_protocol: u64,
    named_roots: Vec<String>,
}

#[derive(Deserialize)]
struct HostConnectResponse {
    host_id: String,
    generation: u64,
    lease_seconds: u64,
    #[serde(default)]
    concurrent_requests: Option<usize>,
}

#[derive(Serialize)]
struct HostGenerationRequest<'a> {
    host_id: &'a str,
    instance_id: &'a str,
    generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    _fabric_auth: Option<&'a crate::fabric_browser::WorkerAuthSnapshot>,
}

#[derive(Serialize)]
struct HostPollRequest<'a> {
    host_id: &'a str,
    instance_id: &'a str,
    generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_availability: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    accept_requests: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    _fabric_auth: Option<&'a crate::fabric_browser::WorkerAuthSnapshot>,
}

/// Bounded, non-secret session availability the host agent reports to the
/// gateway so a read-only `/v1/hosts/status` read can distinguish a reachable
/// endpoint with no serviceable session from a healthy one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostSessionAvailability {
    Ready,
    SessionUnavailable,
    Unavailable,
}

impl HostSessionAvailability {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::SessionUnavailable => "session_unavailable",
            Self::Unavailable => "unavailable",
        }
    }
}

fn classify_host_session_availability(sessions: &[(&str, bool)]) -> HostSessionAvailability {
    let has_public_session = sessions
        .iter()
        .any(|(status, yolo)| *status == "active" && !yolo);
    if has_public_session {
        HostSessionAvailability::Ready
    } else {
        HostSessionAvailability::SessionUnavailable
    }
}

/// Refreshes the local supervisor's session inventory through its
/// legacy-compatible operational control path. Enumeration failure is
/// reported as `unavailable`; it is never presented as ready.
async fn current_host_session_availability() -> HostSessionAvailability {
    match session_control::request_session_views_operational().await {
        Ok(views) => {
            let sessions = views
                .iter()
                .map(|view| (view.status.as_str(), view.yolo))
                .collect::<Vec<_>>();
            classify_host_session_availability(&sessions)
        }
        Err(_) => HostSessionAvailability::Unavailable,
    }
}

#[derive(Deserialize)]
struct PollEnvelope {
    request_id: String,
    request: Value,
    #[serde(default)]
    _fabric_auth: Option<crate::fabric_browser::WorkerAuthSnapshot>,
    #[serde(default)]
    _fabric_session: Option<crate::fabric_browser::BrowserSessionBinding>,
}

#[derive(Serialize)]
struct LegacyResponseRequest<'a> {
    session_id: &'a str,
    instance_id: &'a str,
    generation: u64,
    request_id: &'a str,
    response: &'a Value,
}

#[derive(Serialize)]
struct HostResponseRequest<'a> {
    host_id: &'a str,
    instance_id: &'a str,
    generation: u64,
    request_id: &'a str,
    response: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    _fabric_auth: Option<&'a crate::fabric_browser::WorkerAuthSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    _fabric_session: Option<&'a crate::fabric_browser::BrowserSessionBinding>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenerationExit {
    Replaced,
    Disconnected,
    SupervisorChanged,
}

pub async fn run_agent(options: AgentOptions) -> Result<()> {
    anyhow::ensure!(
        options.session_id.is_some() ^ options.host_id.is_some(),
        "gateway agent requires exactly one of session_id or host_id"
    );
    anyhow::ensure!(
        !options.host_token.trim().is_empty(),
        "gateway host token must not be empty"
    );
    validate_access_service_token(
        options.access_client_id.as_deref(),
        options.access_client_secret.as_deref(),
    )?;

    let base_url = normalize_gateway_url(&options.gateway_url)?;
    let gateway = GatewayClient {
        client: Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to create gateway HTTP client")?,
        sync_client: Client::builder()
            .timeout(OBSERVATION_SYNC_TIMEOUT)
            .build()
            .context("failed to create observation sync HTTP client")?,
        base_url,
        host_token: options.host_token,
        access_client_id: options.access_client_id,
        access_client_secret: options.access_client_secret,
        browser_auth: None,
        browser_auth_blocked: None,
    };
    let reconnect_delay = if options.reconnect_delay.is_zero() {
        DEFAULT_RECONNECT_DELAY
    } else {
        options.reconnect_delay
    };
    let platform = options.platform.resolve();
    let instance_id = Uuid::new_v4().to_string();

    if let Some(host_id) = options.host_id {
        return run_host_agent(
            &gateway,
            &host_id,
            &instance_id,
            platform,
            reconnect_delay,
            None,
        )
        .await;
    }

    run_legacy_agent(
        &gateway,
        options
            .session_id
            .as_deref()
            .expect("validated session mode"),
        &instance_id,
        platform,
        reconnect_delay,
    )
    .await
}

pub(crate) async fn run_browser_link_with_lease(
    protocol: crate::fabric_browser::BrowserOAuth,
    prepared: crate::fabric_browser::PreparedBrowserLink,
    _link_lease: crate::fabric_browser::ProfileLock,
) -> Result<()> {
    prepared
        .record
        .validate_for_link(&prepared.record.gateway_origin)?;
    validate_federated_platform(detected_platform())?;
    let browser_auth = Arc::new(RwLock::new(BrowserWireAuth {
        access_token: prepared.record.oauth_access_token.clone(),
        host_grant: prepared.record.grant_secret.clone(),
        access_expires_at: prepared.record.access_expires_at,
    }));
    let blocked = Arc::new(AtomicBool::new(false));
    let gateway = GatewayClient {
        client: Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to create browser Link HTTP client")?,
        sync_client: Client::builder()
            .timeout(OBSERVATION_SYNC_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to create browser Link sync client")?,
        base_url: normalize_gateway_url(&protocol.options.gateway_url)?,
        host_token: String::new(),
        access_client_id: None,
        access_client_secret: None,
        browser_auth: Some(browser_auth.clone()),
        browser_auth_blocked: Some(blocked.clone()),
    };
    let refresh_task = tokio::spawn(run_browser_refresh_manager(
        protocol,
        prepared.profile.clone(),
        prepared.record.owner_key.clone(),
        prepared.record.host_id.clone(),
        prepared.record.grant_id.clone(),
        prepared.authority.grant_generation,
        browser_auth,
        blocked,
    ));
    let result = run_host_agent(
        &gateway,
        &prepared.record.host_id,
        &Uuid::new_v4().to_string(),
        detected_platform(),
        DEFAULT_RECONNECT_DELAY,
        Some(prepared.authority),
    )
    .await;
    refresh_task.abort();
    let _ = refresh_task.await;
    result
}

// Keep each refresh authority and lifecycle signal explicit in this security boundary.
#[allow(clippy::too_many_arguments)]
async fn run_browser_refresh_manager(
    protocol: crate::fabric_browser::BrowserOAuth,
    profile: String,
    owner_key: String,
    host_id: String,
    grant_id: String,
    grant_generation: u64,
    browser_auth: Arc<RwLock<BrowserWireAuth>>,
    blocked: Arc<AtomicBool>,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        if blocked.load(Ordering::SeqCst) {
            return;
        }
        let should_refresh = browser_auth
            .read()
            .map(|auth| {
                auth.access_expires_at <= crate::fabric_browser::unix_now().saturating_add(90)
            })
            .unwrap_or(true);
        if !should_refresh {
            continue;
        }
        let transition = match crate::fabric_browser::ProfileLock::transition(&profile) {
            Ok(lock) => lock,
            Err(_) => {
                blocked.store(true, Ordering::SeqCst);
                eprintln!("browser OAuth refresh halted: code=credential_lock");
                return;
            }
        };
        let store = match crate::fabric_browser::OsCredentialStore::open_profile(&profile) {
            Ok(store) => store,
            Err(_) => {
                blocked.store(true, Ordering::SeqCst);
                eprintln!("browser OAuth refresh halted: code=secure_store_unavailable");
                return;
            }
        };
        let mut record = match crate::fabric_browser::CredentialStore::load(&store) {
            Ok(Some(record)) => record,
            _ => {
                blocked.store(true, Ordering::SeqCst);
                eprintln!("browser OAuth refresh halted: code=credential_record_unavailable");
                return;
            }
        };
        if record.owner_key != owner_key
            || record.host_id != host_id
            || record.grant_id != grant_id
            || record.grant_generation != grant_generation
        {
            blocked.store(true, Ordering::SeqCst);
            eprintln!("browser OAuth refresh halted: code=grant_snapshot_changed");
            return;
        }
        if crate::fabric_browser::refresh_record_for_link(&mut record, &store, &protocol)
            .await
            .is_err()
        {
            blocked.store(true, Ordering::SeqCst);
            eprintln!("browser OAuth refresh halted: code=refresh_or_commit_failed");
            return;
        }
        match browser_auth.write() {
            Ok(mut auth) => {
                auth.access_token = record.oauth_access_token;
                auth.host_grant = record.grant_secret;
                auth.access_expires_at = record.access_expires_at;
            }
            Err(_) => {
                blocked.store(true, Ordering::SeqCst);
                eprintln!("browser OAuth refresh halted: code=credential_state_unavailable");
                return;
            }
        }
        drop(transition);
    }
}

async fn run_legacy_agent(
    gateway: &GatewayClient,
    session_id: &str,
    instance_id: &str,
    platform: &str,
    reconnect_delay: Duration,
) -> Result<()> {
    config::validate_session_id(session_id)?;
    let session = config::load_session(session_id).await?;
    let approved = approvals::request(
        &session.id,
        "gateway_connect",
        format!(
            "gateway={} platform={platform} instance_id={instance_id}",
            gateway.base_url
        ),
        session.cwd.clone(),
    )
    .await?;
    anyhow::ensure!(approved, "gateway connection was denied at the endpoint");

    eprintln!(
        "temote-mcp legacy gateway agent approved\nsession_id: {}\nplatform: {}\ninstance_id: {}\ngateway: {}",
        session.id, platform, instance_id, gateway.base_url
    );

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Keep accepted work alive, and bounded, across transport reconnections.
    // A new generation must never restart or abort an accepted operation.
    let mut requests = GatewayRequests::new();
    loop {
        let connected = tokio::select! {
            result = connect_legacy(gateway, &session.id, instance_id, platform) => result,
            signal = &mut ctrl_c => {
                signal.context("failed to receive Ctrl-C")?;
                eprintln!("Stopping gateway agent for session {}", session.id);
                return Ok(());
            }
        };

        let connection = match connected {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("gateway connect failed: {error:#}");
                if wait_or_stop(reconnect_delay, &mut ctrl_c, &session.id).await? {
                    return Ok(());
                }
                continue;
            }
        };
        eprintln!(
            "gateway connected: mode=legacy-session session_id={} generation={} lease_seconds={}",
            connection.session_id, connection.generation, connection.lease_seconds
        );

        let outcome = tokio::select! {
            result = run_legacy_generation(
                gateway,
                &session.id,
                instance_id,
                connection.generation,
                AgentCapacity::negotiated(connection.concurrent_requests),
                &mut requests,
            ) => result,
            signal = &mut ctrl_c => {
                signal.context("failed to receive Ctrl-C")?;
                disconnect_legacy(
                    gateway,
                    &session.id,
                    instance_id,
                    connection.generation,
                ).await;
                eprintln!("Stopping gateway agent for session {}", session.id);
                return Ok(());
            }
        };

        log_generation_outcome(outcome, connection.generation);
        if wait_or_stop(reconnect_delay, &mut ctrl_c, &session.id).await? {
            return Ok(());
        }
    }
}

async fn run_host_agent(
    gateway: &GatewayClient,
    host_id: &str,
    instance_id: &str,
    platform: &str,
    reconnect_delay: Duration,
    browser_authority: Option<crate::fabric_browser::FabricAuthority>,
) -> Result<()> {
    validate_federated_platform(platform)?;
    let host_id = host_identity::validate(host_id)?;
    let worker_auth =
        browser_authority
            .as_ref()
            .map(|authority| crate::fabric_browser::WorkerAuthSnapshot {
                mode: "browser".to_owned(),
                owner_key: authority.owner_key.clone(),
                grant_id: authority.grant_id.clone(),
                grant_generation: authority.grant_generation,
                approved_roots: authority.approved_roots.clone(),
            });
    let sessions = session_control::SessionBackend::local_control().await?;
    let (activity_sender, mut activity_receiver) = tokio::sync::mpsc::channel(128);
    if browser_authority.is_none() {
        let activity_host_id = host_id.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) =
                    session_control::follow_lifecycle_activity(activity_sender.clone()).await
                {
                    eprintln!("Fabric lifecycle activity reconnecting: {error:#}");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        tokio::spawn(async move {
            while let Some(event) = activity_receiver.recv().await {
                if let Err(error) =
                    crate::events_host::record_lifecycle(&event, &activity_host_id).await
                {
                    eprintln!("Fabric lifecycle transition deferred: {error:#}");
                }
            }
        });
    }

    eprintln!(
        "temote-mcp federated gateway agent\nhost_id: {}\nplatform: {}\ninstance_id: {}\ngateway: {}",
        host_id, platform, instance_id, gateway.base_url,
    );

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut requests = GatewayRequests::new();
    loop {
        if gateway
            .browser_auth_blocked
            .as_ref()
            .is_some_and(|blocked| blocked.load(Ordering::SeqCst))
        {
            anyhow::bail!(
                "browser OAuth state is uncertain; Link has halted and requires interactive recovery"
            );
        }
        let metadata = tokio::select! {
            result = sessions.status() => result
                .context("local supervisor is unavailable")
                .and_then(|status| host_supervisor_metadata(&status)),
            signal = &mut ctrl_c => {
                signal.context("failed to receive Ctrl-C")?;
                eprintln!("Stopping gateway agent for host {host_id}");
                return Ok(());
            }
        };
        let metadata = match metadata {
            Ok(metadata) => metadata,
            Err(error) => {
                eprintln!("gateway host metadata refresh failed: {error:#}");
                if wait_or_stop(reconnect_delay, &mut ctrl_c, &host_id).await? {
                    return Ok(());
                }
                continue;
            }
        };

        let connected = tokio::select! {
            result = connect_host(
                gateway,
                &host_id,
                instance_id,
                platform,
                &metadata,
                worker_auth.as_ref(),
            ) => result,
            signal = &mut ctrl_c => {
                signal.context("failed to receive Ctrl-C")?;
                eprintln!("Stopping gateway agent for host {host_id}");
                return Ok(());
            }
        };

        let connection = match connected {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("gateway host connect failed: {error:#}");
                if wait_or_stop(reconnect_delay, &mut ctrl_c, &host_id).await? {
                    return Ok(());
                }
                continue;
            }
        };
        eprintln!(
            "gateway connected: mode=host host_id={} generation={} lease_seconds={} named_roots={}",
            connection.host_id,
            connection.generation,
            connection.lease_seconds,
            if worker_auth
                .as_ref()
                .map_or(&metadata.named_roots, |auth| &auth.approved_roots)
                .is_empty()
            {
                "(none)".to_owned()
            } else {
                worker_auth
                    .as_ref()
                    .map_or(&metadata.named_roots, |auth| &auth.approved_roots)
                    .join(",")
            }
        );
        let connection_record =
            HostAgentConnectionRecordFile::create(&host_id, connection.generation);
        if let Err(error) = &connection_record {
            eprintln!("gateway agent connection record unavailable: {error:#}");
        }
        let _connection_record = connection_record.ok();

        let outcome = tokio::select! {
            result = run_host_generation(
                AgentGeneration {
                    gateway: gateway.clone(),
                    route: AgentRoute::Host {
                        host_id: host_id.clone(),
                        sessions: sessions.clone(),
                        metadata: metadata.clone(),
                    },
                    instance_id: instance_id.to_owned(),
                    generation: connection.generation,
                    capacity: AgentCapacity::negotiated(connection.concurrent_requests),
                    admission: Arc::new(AtomicBool::new(true)),
                    browser_authority: browser_authority.clone(),
                    worker_auth: worker_auth.clone(),
                },
                &mut requests,
            ) => result,
            signal = &mut ctrl_c => {
                signal.context("failed to receive Ctrl-C")?;
                disconnect_host(
                    gateway,
                    &host_id,
                    instance_id,
                    connection.generation,
                    worker_auth.as_ref(),
                ).await;
                eprintln!("Stopping gateway agent for host {host_id}");
                return Ok(());
            }
        };

        log_generation_outcome(outcome, connection.generation);
        if gateway
            .browser_auth_blocked
            .as_ref()
            .is_some_and(|blocked| blocked.load(Ordering::SeqCst))
        {
            anyhow::bail!(
                "browser OAuth state is uncertain; Link has halted and requires interactive recovery"
            );
        }
        if wait_or_stop(reconnect_delay, &mut ctrl_c, &host_id).await? {
            return Ok(());
        }
    }
}

fn log_generation_outcome(outcome: Result<GenerationExit>, generation: u64) {
    match outcome {
        Ok(GenerationExit::Replaced) => {
            eprintln!("gateway generation {generation} was replaced; reconnecting");
        }
        Ok(GenerationExit::Disconnected) => {
            eprintln!("gateway disconnected; reconnecting");
        }
        Ok(GenerationExit::SupervisorChanged) => {
            eprintln!("local supervisor metadata changed; reconnecting gateway host generation");
        }
        Err(error) => {
            eprintln!("gateway generation ended: {error:#}");
        }
    }
}

async fn wait_or_stop<F>(
    delay: Duration,
    ctrl_c: &mut std::pin::Pin<&mut F>,
    route_label: &str,
) -> Result<bool>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(false),
        signal = ctrl_c => {
            signal.context("failed to receive Ctrl-C")?;
            eprintln!("Stopping gateway agent for {route_label}");
            Ok(true)
        }
    }
}

#[derive(Serialize, Deserialize)]
struct HostAgentConnectionRecord {
    schema: u64,
    host_id: String,
    generation: u64,
    updated_at: u64,
}

fn host_agent_record_directory() -> Result<PathBuf> {
    Ok(config::state_dir()?.join(HOST_AGENT_RECORD_DIRECTORY))
}

fn host_agent_record_path_in(directory: &Path, host_id: &str) -> Result<PathBuf> {
    host_identity::validate(host_id)?;
    Ok(directory.join(format!("{host_id}.json")))
}

/// Owns the non-secret local record of the active host-level gateway agent
/// generation. The record lets a local `doctor` distinguish a replaced agent
/// generation from a healthy one without a remote protocol change. It contains
/// no credential and is removed when the owning generation ends.
struct HostAgentConnectionRecordFile {
    path: PathBuf,
}

impl HostAgentConnectionRecordFile {
    fn create(host_id: &str, generation: u64) -> Result<Self> {
        Self::create_in(&host_agent_record_directory()?, host_id, generation)
    }

    fn create_in(directory: &Path, host_id: &str, generation: u64) -> Result<Self> {
        let path = host_agent_record_path_in(directory, host_id)?;
        std::fs::create_dir_all(directory).with_context(|| {
            format!(
                "cannot create gateway agent record directory {}",
                directory.display()
            )
        })?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        let record = HostAgentConnectionRecord {
            schema: HOST_AGENT_RECORD_SCHEMA_VERSION,
            host_id: host_id.to_owned(),
            generation,
            updated_at: config::unix_time(),
        };
        let bytes = serde_json::to_vec(&record)?;
        anyhow::ensure!(
            bytes.len() <= MAX_HOST_AGENT_RECORD_BYTES,
            "gateway agent record is oversized"
        );
        let temporary = directory.join(format!(".{host_id}.{}.tmp", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temporary)
                .with_context(|| {
                    format!("cannot create gateway agent record {}", temporary.display())
                })?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, &path).with_context(|| {
                format!("cannot replace gateway agent record {}", path.display())
            })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
        Ok(Self { path })
    }
}

impl Drop for HostAgentConnectionRecordFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Reads the non-secret generation recorded by the local host-level gateway
/// agent, if a trusted record exists. Any unsafe or malformed record is treated
/// as absent rather than as a diagnostic failure.
pub fn read_host_agent_generation(host_id: &str) -> Option<u64> {
    let directory = host_agent_record_directory().ok()?;
    read_host_agent_generation_in(&directory, host_id)
}

fn read_host_agent_generation_in(directory: &Path, host_id: &str) -> Option<u64> {
    let path = host_agent_record_path_in(directory, host_id).ok()?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    if metadata.len() > MAX_HOST_AGENT_RECORD_BYTES as u64 {
        return None;
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take((MAX_HOST_AGENT_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_HOST_AGENT_RECORD_BYTES {
        return None;
    }
    let record: HostAgentConnectionRecord = serde_json::from_slice(&bytes).ok()?;
    if record.schema != HOST_AGENT_RECORD_SCHEMA_VERSION
        || record.host_id != host_id
        || record.generation == 0
        || record.updated_at == 0
    {
        return None;
    }
    Some(record.generation)
}

async fn connect_legacy(
    gateway: &GatewayClient,
    session_id: &str,
    instance_id: &str,
    platform: &str,
) -> Result<LegacyConnectResponse> {
    let response = gateway
        .request(Method::POST, "/v1/hosts/connect", None)
        .json(&LegacyConnectRequest {
            session_id,
            instance_id,
            platform,
        })
        .send()
        .await
        .context("gateway connect request failed")?;
    let response = require_success(response, "gateway connect").await?;
    let bytes = read_bounded_body(response, MAX_GATEWAY_RESPONSE_BYTES, "gateway connect").await?;
    let body: LegacyConnectResponse =
        serde_json::from_slice(&bytes).context("gateway connect returned invalid JSON")?;
    anyhow::ensure!(
        body.session_id == session_id,
        "gateway returned a different session_id"
    );
    Ok(body)
}

async fn connect_host(
    gateway: &GatewayClient,
    host_id: &str,
    instance_id: &str,
    platform: &str,
    metadata: &HostSupervisorMetadata,
    worker_auth: Option<&crate::fabric_browser::WorkerAuthSnapshot>,
) -> Result<HostConnectResponse> {
    let response = gateway
        .request(Method::POST, "/v1/hosts/connect", Some(host_id))
        .json(&host_connect_request(
            host_id,
            instance_id,
            platform,
            metadata,
            worker_auth,
        ))
        .send()
        .await
        .context("gateway host connect request failed")?;
    let response = require_success(response, "gateway host connect").await?;
    let bytes =
        read_bounded_body(response, MAX_GATEWAY_RESPONSE_BYTES, "gateway host connect").await?;
    let body: HostConnectResponse =
        serde_json::from_slice(&bytes).context("gateway host connect returned invalid JSON")?;
    anyhow::ensure!(
        body.host_id == host_id,
        "gateway returned a different host_id"
    );
    Ok(body)
}

async fn run_legacy_generation(
    gateway: &GatewayClient,
    session_id: &str,
    instance_id: &str,
    generation: u64,
    capacity: AgentCapacity,
    requests: &mut GatewayRequests,
) -> Result<GenerationExit> {
    run_agent_generation(
        AgentGeneration {
            gateway: gateway.clone(),
            route: AgentRoute::Legacy {
                session_id: session_id.to_owned(),
            },
            instance_id: instance_id.to_owned(),
            generation,
            capacity,
            admission: Arc::new(AtomicBool::new(true)),
            browser_authority: None,
            worker_auth: None,
        },
        requests,
    )
    .await
}

async fn run_host_generation(
    context: AgentGeneration,
    requests: &mut GatewayRequests,
) -> Result<GenerationExit> {
    let AgentRoute::Host { host_id, .. } = &context.route else {
        anyhow::bail!("host generation requires a host route");
    };
    let side_tasks = if context.browser_authority.is_none() {
        let gateway_for_sync = context.gateway.clone();
        let host_id_for_sync = host_id.clone();
        let generation = context.generation;
        let sync_task = tokio::spawn(async move {
            run_host_observation_sync(&gateway_for_sync, &host_id_for_sync).await;
        });
        let gateway_for_events = context.gateway.clone();
        let host_id_for_events = host_id.clone();
        let instance_id_for_events = context.instance_id.clone();
        let event_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            loop {
                interval.tick().await;
                match tokio::time::timeout(
                    Duration::from_secs(8),
                    crate::events_host::deliver_pending(
                        &gateway_for_events,
                        &host_id_for_events,
                        &instance_id_for_events,
                        generation,
                    ),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => eprintln!("Fabric event delivery deferred: {error:#}"),
                    Err(_) => eprintln!("Fabric event delivery deferred: batch_timeout"),
                }
            }
        });
        Some((sync_task, event_task))
    } else {
        None
    };
    let result = run_agent_generation(context, requests).await;
    if let Some((sync_task, event_task)) = side_tasks {
        sync_task.abort();
        let _ = sync_task.await;
        event_task.abort();
        let _ = event_task.await;
    }
    result
}

async fn run_host_observation_sync(gateway: &GatewayClient, host_id: &str) {
    let mut replicator =
        match crate::observation::replicator::HostReplicator::new(host_id, &gateway.base_url) {
            Ok(replicator) => replicator,
            Err(_) => {
                eprintln!("observation sync unavailable: code=local_store");
                return;
            }
        };
    let mut interval = tokio::time::interval(OBSERVATION_SYNC_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        match replicator
            .sync_next(|batch| gateway.sync_observations(host_id, batch))
            .await
        {
            Ok(crate::observation::replicator::SyncOutcome::Acked {
                records,
                through_revision,
                ..
            }) if records > 0 => eprintln!(
                "observation sync acked: records={records} through_revision={through_revision}"
            ),
            Ok(crate::observation::replicator::SyncOutcome::NoWork)
            | Ok(crate::observation::replicator::SyncOutcome::Busy)
            | Ok(crate::observation::replicator::SyncOutcome::Backoff { .. })
            | Ok(crate::observation::replicator::SyncOutcome::Acked { .. }) => {}
            Err(error) => eprintln!("observation sync deferred: code={}", error.code.as_str()),
        }
    }
}

async fn run_agent_generation(
    context: AgentGeneration,
    requests: &mut GatewayRequests,
) -> Result<GenerationExit> {
    let _admission = GenerationAdmissionGuard(context.admission.clone());
    let poll_context = context.clone();
    run_concurrent_generation(
        requests,
        context.generation,
        context.capacity,
        move |accept_requests| {
            let context = poll_context.clone();
            async move { context.poll(accept_requests).await }
        },
        move |envelope| {
            let context = context.clone();
            async move { context.dispatch(envelope).await }
        },
    )
    .await
}

/// Keep one poll future pinned while accepted requests finish. This prevents a
/// fast completion from cancelling and duplicating an in-flight long poll.
async fn run_concurrent_generation<P, PF, D, DF>(
    requests: &mut GatewayRequests,
    generation: u64,
    capacity: AgentCapacity,
    mut poll: P,
    mut dispatch: D,
) -> Result<GenerationExit>
where
    P: FnMut(bool) -> PF,
    PF: std::future::Future<Output = Result<PollOutcome>>,
    D: FnMut(PollEnvelope) -> DF,
    DF: std::future::Future<Output = Result<Option<GenerationExit>>> + Send + 'static,
{
    loop {
        let accept_requests = requests.len() < capacity.limit;
        if !accept_requests && !capacity.heartbeat_supported {
            if let Some(exit) = completed_request(requests.join_next().await, generation)? {
                return Ok(exit);
            }
            continue;
        }

        let polling = poll(accept_requests);
        tokio::pin!(polling);
        let outcome = loop {
            tokio::select! {
                biased;
                completed = requests.join_next(), if !requests.is_empty() => {
                    if let Some(exit) = completed_request(completed, generation)? {
                        return Ok(exit);
                    }
                }
                outcome = &mut polling => break outcome?,
            }
        };
        match outcome {
            PollOutcome::Request(envelope) => {
                anyhow::ensure!(
                    accept_requests,
                    "gateway sent a request to a saturated agent"
                );
                let request = dispatch(*envelope);
                requests.spawn(async move { (generation, request.await) });
            }
            PollOutcome::Exit(exit) => return Ok(exit),
            PollOutcome::Idle if requests.len() >= capacity.limit => {
                tokio::select! {
                    completed = requests.join_next() => {
                        if let Some(exit) = completed_request(completed, generation)? {
                            return Ok(exit);
                        }
                    }
                    _ = tokio::time::sleep(CAPACITY_HEARTBEAT_INTERVAL) => {}
                }
            }
            PollOutcome::Idle => {}
        }
    }
}

fn completed_request(
    completed: CompletedGatewayRequest,
    generation: u64,
) -> Result<Option<GenerationExit>> {
    let Some(completed) = completed else {
        return Ok(None);
    };
    let (request_generation, outcome) = completed.context("gateway request worker failed")?;
    match outcome {
        Ok(exit) if request_generation == generation => Ok(exit),
        Ok(_) => Ok(None),
        Err(_) if request_generation != generation => {
            // Work accepted by a retired generation may finish after its
            // replacement connects. Its failure must not retire that newer
            // generation.
            eprintln!(
                "gateway request failure ignored: code=retired_generation request_generation={request_generation} current_generation={generation}"
            );
            Ok(None)
        }
        Err(error) if gateway_transport_error(&error) => {
            // Delivery uncertainty says nothing about execution. Keep the
            // generation connected and let the caller reconcile the operation.
            eprintln!(
                "gateway request outcome unavailable: code=transport generation={request_generation}; reconcile the original operation"
            );
            Ok(None)
        }
        Err(_) => {
            // Unknown conflicts and other non-transient protocol failures
            // cannot be treated as successful delivery. Retire this generation
            // without logging any remote response body.
            eprintln!(
                "gateway request failed: code=protocol_or_local_error generation={request_generation}; reconnecting"
            );
            Err(anyhow::anyhow!(
                "gateway request failed in generation {request_generation}: protocol_or_local_error"
            ))
        }
    }
}

impl AgentGeneration {
    fn host_id(&self) -> Option<&str> {
        match &self.route {
            AgentRoute::Legacy { .. } => None,
            AgentRoute::Host { host_id, .. } => Some(host_id),
        }
    }

    async fn verify(&self) -> Result<Option<GenerationExit>> {
        match &self.route {
            AgentRoute::Legacy { session_id } => {
                if config::session_is_active(session_id).await? {
                    Ok(None)
                } else {
                    disconnect_legacy(
                        &self.gateway,
                        session_id,
                        &self.instance_id,
                        self.generation,
                    )
                    .await;
                    Ok(Some(GenerationExit::Disconnected))
                }
            }
            AgentRoute::Host {
                host_id,
                sessions,
                metadata,
            } => {
                if self
                    .gateway
                    .browser_auth_blocked
                    .as_ref()
                    .is_some_and(|blocked| blocked.load(Ordering::SeqCst))
                {
                    return Ok(Some(GenerationExit::Disconnected));
                }
                verify_host_generation_metadata(
                    &self.gateway,
                    sessions,
                    host_id,
                    &self.instance_id,
                    self.generation,
                    metadata,
                    self.worker_auth.as_ref(),
                )
                .await
            }
        }
    }

    async fn poll(&self, accept_requests: bool) -> Result<PollOutcome> {
        if self
            .gateway
            .browser_auth_blocked
            .as_ref()
            .is_some_and(|blocked| blocked.load(Ordering::SeqCst))
        {
            return Ok(PollOutcome::Exit(GenerationExit::Disconnected));
        }
        if let Some(exit) = self.verify().await? {
            return Ok(PollOutcome::Exit(exit));
        }
        let accept_requests = self.capacity.heartbeat_supported.then_some(accept_requests);
        let poll_started = Instant::now();
        let request = self
            .gateway
            .request(Method::POST, "/v1/hosts/poll", self.host_id());
        let request = match &self.route {
            AgentRoute::Legacy { session_id } => request.json(&LegacyPollRequest {
                identity: LegacyGenerationRequest {
                    session_id,
                    instance_id: &self.instance_id,
                    generation: self.generation,
                },
                accept_requests,
            }),
            AgentRoute::Host { host_id, .. } => {
                let availability = if let (Some(authority), AgentRoute::Host { sessions, .. }) =
                    (self.browser_authority.as_ref(), &self.route)
                {
                    match sessions
                        .fabric_tool(authority.clone(), "session_list", json!({}), None)
                        .await
                    {
                        Ok(value)
                            if value
                                .as_array()
                                .is_some_and(|sessions| !sessions.is_empty()) =>
                        {
                            HostSessionAvailability::Ready
                        }
                        Ok(_) => HostSessionAvailability::SessionUnavailable,
                        Err(_) => HostSessionAvailability::Unavailable,
                    }
                } else {
                    current_host_session_availability().await
                };
                request.json(&HostPollRequest {
                    host_id,
                    instance_id: &self.instance_id,
                    generation: self.generation,
                    session_availability: Some(availability.as_str()),
                    accept_requests,
                    _fabric_auth: self.worker_auth.as_ref(),
                })
            }
        };
        let response = match request.send().await {
            Ok(response) => response,
            Err(_) => {
                eprintln!(
                    "gateway poll deferred: code=transport; retaining generation {}",
                    self.generation
                );
                tokio::time::sleep(DEFAULT_RECONNECT_DELAY).await;
                return Ok(PollOutcome::Idle);
            }
        };
        if transient_gateway_status(response.status()) {
            eprintln!(
                "gateway poll deferred: code=remote_unavailable status={}; retaining generation {}",
                response.status().as_u16(),
                self.generation
            );
            tokio::time::sleep(DEFAULT_RECONNECT_DELAY).await;
            return Ok(PollOutcome::Idle);
        }
        match poll_envelope(response, poll_started).await {
            Err(error) if gateway_transport_error(&error) => {
                eprintln!(
                    "gateway poll deferred: code=response_transport; retaining generation {}",
                    self.generation
                );
                tokio::time::sleep(DEFAULT_RECONNECT_DELAY).await;
                Ok(PollOutcome::Idle)
            }
            outcome => outcome,
        }
    }

    async fn dispatch(&self, envelope: PollEnvelope) -> Result<Option<GenerationExit>> {
        // Revalidate after the long poll and immediately before local admission.
        // Session ownership and full-instance fencing remain in public dispatch.
        if let Some(exit) = self.verify().await? {
            return Ok(Some(exit));
        }
        if !self.admission.load(Ordering::SeqCst) {
            return Ok(Some(GenerationExit::Replaced));
        }
        let response = match &self.route {
            AgentRoute::Legacy { .. } => dispatch_response(&envelope.request).await,
            AgentRoute::Host { sessions, .. } => {
                if let Some(authority) = &self.browser_authority {
                    anyhow::ensure!(
                        same_worker_grant(
                            envelope._fabric_auth.as_ref(),
                            self.worker_auth.as_ref()
                        ),
                        "Worker authorization snapshot changed during the long poll"
                    );
                    dispatch_browser_host_response(
                        &envelope.request,
                        sessions,
                        authority,
                        envelope._fabric_session.as_ref(),
                    )
                    .await
                } else {
                    dispatch_host_response(&envelope.request, sessions).await
                }
            }
        };
        if let (Some(authority), Some(binding), AgentRoute::Host { sessions, .. }) = (
            self.browser_authority.as_ref(),
            envelope._fabric_session.as_ref(),
            &self.route,
        ) {
            let name = envelope
                .request
                .pointer("/params/name")
                .and_then(Value::as_str);
            // Stop and restart intentionally retire the bound instance. Their
            // admission and mutation are one Supervisor-locked transaction;
            // every other result is discarded if the same instance cannot be
            // revalidated immediately before response upload.
            if !matches!(name, Some("session_stop" | "session_restart"))
                && sessions
                    .fabric_verify_session(authority.clone(), binding.clone())
                    .await
                    .is_err()
            {
                return self.upload_response(
                    &envelope.request_id,
                    &json!({"jsonrpc":"2.0","id":envelope.request.get("id").cloned().unwrap_or(Value::Null),
                        "error":{"code":-32000,"message":"browser session changed during request"}}),
                    envelope._fabric_auth.as_ref(),
                    envelope._fabric_session.as_ref(),
                ).await;
            }
        }
        self.upload_response(
            &envelope.request_id,
            &response,
            envelope._fabric_auth.as_ref(),
            envelope._fabric_session.as_ref(),
        )
        .await
    }

    async fn upload_response(
        &self,
        request_id: &str,
        rpc_response: &Value,
        worker_auth: Option<&crate::fabric_browser::WorkerAuthSnapshot>,
        browser_session: Option<&crate::fabric_browser::BrowserSessionBinding>,
    ) -> Result<Option<GenerationExit>> {
        for attempt in 0..RESPONSE_UPLOAD_ATTEMPTS {
            let request = self
                .gateway
                .request(Method::POST, "/v1/hosts/respond", self.host_id());
            let request = match &self.route {
                AgentRoute::Legacy { session_id } => request.json(&LegacyResponseRequest {
                    session_id,
                    instance_id: &self.instance_id,
                    generation: self.generation,
                    request_id,
                    response: rpc_response,
                }),
                AgentRoute::Host { host_id, .. } => request.json(&HostResponseRequest {
                    host_id,
                    instance_id: &self.instance_id,
                    generation: self.generation,
                    request_id,
                    response: rpc_response,
                    _fabric_auth: worker_auth,
                    _fabric_session: browser_session,
                }),
            };
            match request.send().await {
                Ok(response) if response.status().is_success() => return Ok(None),
                Ok(response) if response.status() == StatusCode::CONFLICT => {
                    match read_gateway_conflict(response).await {
                        Ok(GatewayConflict::StaleRequest) => {
                            eprintln!(
                                "gateway response no longer awaited: code=stale_request generation={}; operation is not replayed",
                                self.generation
                            );
                            return Ok(None);
                        }
                        Ok(GatewayConflict::Replaced) => {
                            return Ok(Some(GenerationExit::Replaced));
                        }
                        Ok(GatewayConflict::PollReplaced) => {
                            anyhow::bail!("unexpected poll conflict during response delivery")
                        }
                        Err(error) if gateway_transport_error(&error) => {}
                        Err(error) => return Err(error),
                    }
                }
                Ok(response) if !transient_gateway_status(response.status()) => {
                    require_success(response, "gateway response upload").await?;
                    return Ok(None);
                }
                Ok(_) | Err(_) => {}
            }
            if attempt + 1 < RESPONSE_UPLOAD_ATTEMPTS {
                // Only resend this exact response envelope, never local dispatch.
                tokio::time::sleep(Duration::from_millis(250 * (attempt as u64 + 1))).await;
            }
        }
        eprintln!(
            "gateway response delivery unavailable: code=transport_or_remote_unavailable generation={}; reconcile the original operation",
            self.generation
        );
        Ok(None)
    }
}

fn transient_gateway_status(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
}

fn gateway_transport_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<reqwest::Error>().is_some())
}

fn gateway_conflict(value: &Value) -> Result<GatewayConflict> {
    match value.get("error").and_then(Value::as_str) {
        Some("stale_request") => Ok(GatewayConflict::StaleRequest),
        Some("poll_replaced") => Ok(GatewayConflict::PollReplaced),
        Some(
            "stale_generation"
            | "generation_replaced"
            | "host_offline"
            | "host_disconnected"
            | "host_lease_expired"
            | "registry_registration_failed"
            | "registry_renewal_failed",
        ) => Ok(GatewayConflict::Replaced),
        _ => anyhow::bail!("gateway returned an unrecognized conflict"),
    }
}

async fn read_gateway_conflict(response: Response) -> Result<GatewayConflict> {
    let bytes = read_bounded_body(response, MAX_GATEWAY_ERROR_BYTES, "gateway conflict").await?;
    gateway_conflict(
        &serde_json::from_slice(&bytes).context("gateway conflict returned invalid JSON")?,
    )
}

async fn verify_host_generation_metadata(
    gateway: &GatewayClient,
    sessions: &session_control::SessionBackend,
    host_id: &str,
    instance_id: &str,
    generation: u64,
    connected_metadata: &HostSupervisorMetadata,
    worker_auth: Option<&crate::fabric_browser::WorkerAuthSnapshot>,
) -> Result<Option<GenerationExit>> {
    let status = match sessions.status().await {
        Ok(status) => status,
        Err(error) => {
            disconnect_host(gateway, host_id, instance_id, generation, worker_auth).await;
            return Err(error).context("local supervisor is unavailable");
        }
    };
    let changed = match supervisor_metadata_changed(&status, connected_metadata) {
        Ok(changed) => changed,
        Err(error) => {
            disconnect_host(gateway, host_id, instance_id, generation, worker_auth).await;
            return Err(error);
        }
    };
    if changed {
        disconnect_host(gateway, host_id, instance_id, generation, worker_auth).await;
        return Ok(Some(GenerationExit::SupervisorChanged));
    }
    Ok(None)
}

fn supervisor_metadata_changed(
    status: &Value,
    connected_metadata: &HostSupervisorMetadata,
) -> Result<bool> {
    Ok(host_supervisor_metadata(status)? != *connected_metadata)
}

async fn poll_envelope(response: Response, poll_started: Instant) -> Result<PollOutcome> {
    if response.status() == StatusCode::NO_CONTENT {
        let delay = empty_poll_delay(poll_started.elapsed());
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        return Ok(PollOutcome::Idle);
    }
    if response.status() == StatusCode::CONFLICT {
        return match read_gateway_conflict(response).await? {
            GatewayConflict::Replaced => Ok(PollOutcome::Exit(GenerationExit::Replaced)),
            GatewayConflict::PollReplaced => Ok(PollOutcome::Idle),
            GatewayConflict::StaleRequest => {
                anyhow::bail!("unexpected request conflict during poll")
            }
        };
    }
    let response = require_success(response, "gateway poll").await?;
    let bytes =
        read_bounded_body(response, MAX_GATEWAY_POLL_RESPONSE_BYTES, "gateway poll").await?;
    let envelope: PollEnvelope =
        serde_json::from_slice(&bytes).context("gateway poll returned invalid JSON")?;
    anyhow::ensure!(
        !envelope.request_id.is_empty() && envelope.request_id.len() <= 256,
        "gateway poll returned invalid request identity"
    );
    Ok(PollOutcome::Request(Box::new(envelope)))
}

fn empty_poll_delay(elapsed: Duration) -> Duration {
    MIN_EMPTY_POLL_INTERVAL.saturating_sub(elapsed)
}

async fn dispatch_response(request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    match mcp::dispatch_public(request, None).await {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": format!("{error:#}")}
        }),
    }
}

async fn dispatch_host_response(
    request: &Value,
    sessions: &session_control::SessionBackend,
) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    match mcp::dispatch_public(request, Some(sessions)).await {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": format!("{error:#}")}
        }),
    }
}

async fn dispatch_browser_host_response(
    request: &Value,
    sessions: &session_control::SessionBackend,
    authority: &crate::fabric_browser::FabricAuthority,
    binding: Option<&crate::fabric_browser::BrowserSessionBinding>,
) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let routed = async {
        anyhow::ensure!(
            request.get("jsonrpc").and_then(Value::as_str) == Some("2.0"),
            "invalid JSON-RPC request"
        );
        anyhow::ensure!(
            request.get("method").and_then(Value::as_str) == Some("tools/call"),
            "method is unavailable for browser-enrolled Hosts"
        );
        let params = request
            .get("params")
            .and_then(Value::as_object)
            .context("invalid tool request")?;
        let (name, args) = browser_tool_request(params)?;
        anyhow::ensure!(
            matches!(
                name.as_str(),
                "session_list"
                    | "session_start"
                    | "session_info"
                    | "session_stop"
                    | "session_restart"
                    | "codex_status"
                    | "codex_task_start"
                    | "codex_task_get"
                    | "codex_task_control"
                    | "evidence_read"
                    | "task_list"
                    | "poll_job"
                    | "job_list"
                    | "stop_job"
            ),
            "tool is unavailable for browser-enrolled Hosts"
        );
        let session_bound = !matches!(name.as_str(), "session_list" | "session_start");
        let binding = if session_bound {
            let session_id = args
                .get("session_id")
                .and_then(Value::as_str)
                .context("browser-scoped tool requires session_id")?;
            let binding = binding.context("Worker omitted the live session instance binding")?;
            anyhow::ensure!(
                binding.session_id == session_id
                    && Uuid::parse_str(&binding.session_instance).is_ok(),
                "Worker session instance binding does not match the request"
            );
            Some(binding.clone())
        } else {
            anyhow::ensure!(binding.is_none(), "unexpected session instance binding");
            None
        };
        let result = sessions
            .fabric_tool(authority.clone(), &name, args, binding)
            .await?;
        Ok::<_, anyhow::Error>((name, result))
    }
    .await;
    match routed {
        Ok((name, result)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": browser_tool_mcp_result(&name, result),
        }),
        // Supervisor errors may include local filesystem paths or other
        // diagnostics. The public browser channel returns a stable bounded
        // denial and keeps those details in local logs only.
        Err(_) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": "browser-scoped request denied"}
        }),
    }
}

fn browser_tool_request(params: &serde_json::Map<String, Value>) -> Result<(String, Value)> {
    anyhow::ensure!(
        params
            .keys()
            .all(|key| matches!(key.as_str(), "name" | "arguments" | "_meta")),
        "invalid tool request"
    );
    if let Some(metadata) = params.get("_meta") {
        anyhow::ensure!(metadata.is_object(), "invalid protocol metadata");
    }
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("missing tool name")?
        .to_owned();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    anyhow::ensure!(args.is_object(), "tool arguments must be an object");
    Ok((name, args))
}

fn browser_tool_mcp_result(name: &str, value: serde_json::Value) -> serde_json::Value {
    if matches!(
        name,
        "session_list" | "session_start" | "session_info" | "session_stop" | "session_restart"
    ) {
        json!({
            "content": [{
                "type": "text",
                "text": value.to_string(),
            }],
        })
    } else {
        value
    }
}

fn same_worker_grant(
    incoming: Option<&crate::fabric_browser::WorkerAuthSnapshot>,
    expected: Option<&crate::fabric_browser::WorkerAuthSnapshot>,
) -> bool {
    match (incoming, expected) {
        (Some(incoming), Some(expected)) => {
            incoming.mode == expected.mode
                && incoming.owner_key == expected.owner_key
                && incoming.grant_id == expected.grant_id
                && incoming.grant_generation == expected.grant_generation
                && incoming.approved_roots == expected.approved_roots
        }
        (None, None) => true,
        _ => false,
    }
}

async fn disconnect_legacy(
    gateway: &GatewayClient,
    session_id: &str,
    instance_id: &str,
    generation: u64,
) {
    let result = gateway
        .request(Method::POST, "/v1/hosts/disconnect", None)
        .json(&LegacyGenerationRequest {
            session_id,
            instance_id,
            generation,
        })
        .send()
        .await;
    if let Err(error) = result {
        eprintln!("gateway disconnect failed: {error}");
    }
}

async fn disconnect_host(
    gateway: &GatewayClient,
    host_id: &str,
    instance_id: &str,
    generation: u64,
    worker_auth: Option<&crate::fabric_browser::WorkerAuthSnapshot>,
) {
    let result = gateway
        .request(Method::POST, "/v1/hosts/disconnect", Some(host_id))
        .json(&HostGenerationRequest {
            host_id,
            instance_id,
            generation,
            _fabric_auth: worker_auth,
        })
        .send()
        .await;
    if let Err(error) = result {
        eprintln!("gateway host disconnect failed: {error}");
    }
}

fn host_supervisor_metadata(status: &Value) -> Result<HostSupervisorMetadata> {
    anyhow::ensure!(
        status.get("status").and_then(Value::as_str) == Some("active"),
        "local supervisor is not active"
    );
    let runtime_version = status
        .get("version")
        .and_then(Value::as_str)
        .context("local supervisor did not report runtime version")?
        .to_owned();
    let control_protocol = status
        .get("control_protocol")
        .and_then(Value::as_u64)
        .context("local supervisor did not report control protocol")?;
    anyhow::ensure!(
        control_protocol == session_control::CONTROL_PROTOCOL_VERSION,
        "local supervisor control protocol is incompatible"
    );
    Ok(HostSupervisorMetadata {
        runtime_version,
        control_protocol,
        named_roots: status_named_roots(status)?,
    })
}

fn host_connect_request<'a>(
    host_id: &'a str,
    instance_id: &'a str,
    platform: &'a str,
    metadata: &'a HostSupervisorMetadata,
    worker_auth: Option<&'a crate::fabric_browser::WorkerAuthSnapshot>,
) -> HostConnectRequest<'a> {
    HostConnectRequest {
        host_id,
        instance_id,
        platform,
        agent_protocol: HOST_AGENT_PROTOCOL_VERSION,
        runtime_version: &metadata.runtime_version,
        control_protocol: metadata.control_protocol,
        capabilities: HOST_CAPABILITIES,
        named_roots: worker_auth.map_or(&metadata.named_roots, |auth| &auth.approved_roots),
        _fabric_auth: worker_auth,
    }
}

fn status_named_roots(status: &Value) -> Result<Vec<String>> {
    let values = status
        .get("named_roots")
        .and_then(Value::as_array)
        .context("local supervisor did not report named roots")?;
    let mut roots = Vec::with_capacity(values.len());
    for value in values {
        let root = value
            .as_str()
            .context("local supervisor returned an invalid named root")?;
        crate::named_roots::validate_root_name(root)?;
        roots.push(root.to_owned());
    }
    Ok(roots)
}

impl GatewayClient {
    pub(crate) fn request(
        &self,
        method: Method,
        path: &str,
        host_id: Option<&str>,
    ) -> RequestBuilder {
        self.request_with_client(&self.client, method, path, host_id)
    }

    pub(crate) fn request_with_client(
        &self,
        client: &Client,
        method: Method,
        path: &str,
        host_id: Option<&str>,
    ) -> RequestBuilder {
        let mut request = client.request(method, format!("{}{}", self.base_url, path));
        if let Some(browser_auth) = &self.browser_auth {
            match browser_auth.read() {
                Ok(auth) => {
                    request = request
                        .bearer_auth(&auth.access_token)
                        .header("X-Temote-Fabric-Host-Grant", &auth.host_grant);
                }
                Err(_) => request = request.bearer_auth("invalid-browser-credential-state"),
            }
        } else {
            request = request.bearer_auth(&self.host_token);
        }
        if let Some(host_id) = host_id {
            request = request.header("X-Temote-Host-Id", host_id);
        }
        if self.browser_auth.is_none()
            && let (Some(client_id), Some(client_secret)) = (
                self.access_client_id.as_deref(),
                self.access_client_secret.as_deref(),
            )
        {
            request = request
                .header("CF-Access-Client-Id", client_id)
                .header("CF-Access-Client-Secret", client_secret);
        }
        request
    }

    async fn sync_observations(
        &self,
        host_id: &str,
        batch: crate::observation::replicator::SyncRequest,
    ) -> std::result::Result<
        crate::observation::replicator::SyncResponse,
        crate::observation::replicator::SyncFailure,
    > {
        use crate::observation::replicator::{SyncFailure, SyncFailureCode, SyncResponse};

        let path = format!("/v1/hosts/{host_id}/observations/sync");
        let response = self
            .request_with_client(&self.sync_client, Method::POST, &path, Some(host_id))
            .json(&batch)
            .send()
            .await
            .map_err(|error| {
                SyncFailure::new(if error.is_timeout() {
                    SyncFailureCode::Timeout
                } else {
                    SyncFailureCode::Transport
                })
            })?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(SyncFailure::new(SyncFailureCode::Authentication));
        }
        if !status.is_success() {
            return Err(SyncFailure::new(SyncFailureCode::RemoteRejected));
        }
        let bytes = read_bounded_body(
            response,
            MAX_GATEWAY_SYNC_RESPONSE_BYTES,
            "observation sync",
        )
        .await
        .map_err(|_| SyncFailure::new(SyncFailureCode::InvalidResponse))?;
        serde_json::from_slice::<SyncResponse>(&bytes)
            .map_err(|_| SyncFailure::new(SyncFailureCode::InvalidResponse))
    }
}

async fn require_success(response: Response, operation: &str) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = read_bounded_body(response, MAX_GATEWAY_ERROR_BYTES, operation)
        .await
        .with_context(|| format!("{operation} failed with HTTP {status}"))?;
    let detail = String::from_utf8_lossy(&body)
        .chars()
        .take(MAX_GATEWAY_ERROR_DISPLAY_CHARS)
        .collect::<String>();
    anyhow::bail!("{operation} failed with HTTP {status}: {detail}")
}

pub(crate) async fn read_bounded_body(
    mut response: Response,
    limit: usize,
    operation: &str,
) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= limit as u64,
            "{operation} response exceeds {limit} bytes"
        );
    }
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("failed to read {operation} response"))?
    {
        append_bounded_body_chunk(&mut body, &chunk, limit, operation)?;
    }
    Ok(body)
}

fn append_bounded_body_chunk(
    body: &mut Vec<u8>,
    chunk: &[u8],
    limit: usize,
    operation: &str,
) -> Result<()> {
    let next = body
        .len()
        .checked_add(chunk.len())
        .context("gateway response size overflow")?;
    anyhow::ensure!(next <= limit, "{operation} response exceeds {limit} bytes");
    body.extend_from_slice(chunk);
    Ok(())
}

pub(crate) fn normalize_gateway_url(value: &str) -> Result<String> {
    let parsed = Url::parse(value.trim()).context("gateway URL is invalid")?;
    anyhow::ensure!(
        parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.path().trim_matches('/').is_empty(),
        "gateway URL must be an origin without credentials, path, query, or fragment"
    );
    let host = parsed.host_str().context("gateway URL has no host")?;
    let local_http = parsed.scheme() == "http" && matches!(host, "localhost" | "127.0.0.1" | "::1");
    anyhow::ensure!(
        parsed.scheme() == "https" || local_http,
        "gateway URL must use HTTPS (HTTP is allowed only for localhost)"
    );
    Ok(parsed
        .origin()
        .ascii_serialization()
        .trim_end_matches('/')
        .to_owned())
}

fn validate_access_service_token(
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> Result<()> {
    match (client_id, client_secret) {
        (None, None) => Ok(()),
        (Some(client_id), Some(client_secret)) => {
            anyhow::ensure!(
                !client_id.trim().is_empty() && !client_secret.trim().is_empty(),
                "Cloudflare Access client ID and secret must not be empty"
            );
            Ok(())
        }
        _ => anyhow::bail!("Cloudflare Access client ID and secret must be provided together"),
    }
}

fn validate_federated_platform(platform: &str) -> Result<()> {
    anyhow::ensure!(
        matches!(platform, "macos" | "linux" | "wsl2"),
        "native Windows federation is not supported yet; run Temote inside WSL2"
    );
    Ok(())
}

fn detected_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::env::var_os("WSL_INTEROP").is_some()
    {
        "wsl2"
    } else {
        "linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[cfg(feature = "network")]
    #[test]
    fn browser_link_uses_distinct_oauth_and_host_grant_headers() {
        let client = Client::builder().build().unwrap();
        let gateway = GatewayClient {
            client: client.clone(),
            sync_client: client,
            base_url: "https://fabric.example".to_owned(),
            host_token: String::new(),
            access_client_id: Some("must-not-be-used".to_owned()),
            access_client_secret: Some("must-not-be-used".to_owned()),
            browser_auth: Some(Arc::new(RwLock::new(BrowserWireAuth {
                access_token: "oauth:fake-access".to_owned(),
                host_grant: "fake-host-grant".to_owned(),
                access_expires_at: 1,
            }))),
            browser_auth_blocked: Some(Arc::new(AtomicBool::new(false))),
        };
        let request = gateway
            .request(Method::POST, "/v1/hosts/poll", Some("host-a"))
            .build()
            .unwrap();
        assert_eq!(
            request.headers()["authorization"],
            "Bearer oauth:fake-access"
        );
        assert_eq!(
            request.headers()["x-temote-fabric-host-grant"],
            "fake-host-grant"
        );
        assert_eq!(request.headers()["x-temote-host-id"], "host-a");
        assert!(!request.headers().contains_key("cf-access-client-id"));
        assert!(!request.headers().contains_key("cf-access-client-secret"));
    }

    #[test]
    fn browser_tool_response_uses_worker_session_list_mcp_content_envelope() {
        let raw_supervisor_value = json!([{
            "session_id": "session-a",
            "session_instance": "00000000-0000-4000-8000-000000000001",
            "root_name": "src",
            "logical_path": "repo",
        }]);
        let response = browser_tool_mcp_result("session_list", raw_supervisor_value);
        let text = response["content"][0]["text"].as_str().unwrap();
        let sessions: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(sessions[0]["session_id"], "session-a");
        assert_eq!(sessions[0]["root_name"], "src");
    }

    #[test]
    fn browser_typed_tool_result_keeps_mcp_error_metadata_and_content() {
        let typed = json!({
            "content": [{"type":"text","text":"task failed"}],
            "isError": true,
            "_meta": {"diagnosticCode":"backend-denied"}
        });
        assert_eq!(
            browser_tool_mcp_result("codex_task_get", typed.clone()),
            typed
        );
    }

    #[test]
    fn browser_dispatch_accepts_protocol_meta_but_never_promotes_it_to_authority() {
        let params = json!({
            "name": "codex_status",
            "arguments": {"session_id": "session-a"},
            "_meta": {
                "progressToken": 42,
                "owner_key": "forged-owner",
                "approved_roots": ["outside"],
                "session_instance": "forged-instance"
            }
        });
        let parsed = browser_tool_request(params.as_object().unwrap()).unwrap();
        assert_eq!(parsed.0, "codex_status");
        assert_eq!(parsed.1, json!({"session_id":"session-a"}));
        assert!(
            browser_tool_request(
                json!({"name":"codex_status","_meta":[]})
                    .as_object()
                    .unwrap()
            )
            .is_err()
        );
    }

    fn request_envelope(id: u64) -> PollOutcome {
        PollOutcome::Request(Box::new(PollEnvelope {
            request_id: id.to_string(),
            request: json!({ "jsonrpc": "2.0", "id": id, "method": "ping" }),
            _fabric_auth: None,
            _fabric_session: None,
        }))
    }

    #[test]
    fn gateway_parallel_capacity_is_bounded_and_old_gateways_remain_compatible() {
        for (advertised, expected) in [
            (None, 1),
            (Some(0), 1),
            (Some(1), 1),
            (Some(8), 8),
            (Some(1024), 8),
        ] {
            let capacity = AgentCapacity::negotiated(advertised);
            assert_eq!(capacity.limit, expected);
            assert_eq!(capacity.heartbeat_supported, advertised.is_some());
        }
    }

    #[test]
    fn a_stale_request_is_not_evidence_of_a_replaced_generation() {
        assert_eq!(
            gateway_conflict(&json!({"error":"stale_request"})).unwrap(),
            GatewayConflict::StaleRequest
        );
        assert_eq!(
            gateway_conflict(&json!({"error":"poll_replaced"})).unwrap(),
            GatewayConflict::PollReplaced
        );
        for code in [
            "stale_generation",
            "generation_replaced",
            "host_offline",
            "host_disconnected",
            "host_lease_expired",
        ] {
            assert_eq!(
                gateway_conflict(&json!({"error":code})).unwrap(),
                GatewayConflict::Replaced
            );
        }
        for value in [
            json!({}),
            json!({"error":false}),
            json!({"error":"unknown"}),
        ] {
            assert!(gateway_conflict(&value).is_err());
        }
    }

    #[test]
    fn generation_retirement_closes_new_admission() {
        let admission = Arc::new(AtomicBool::new(true));
        let retired = GenerationAdmissionGuard(admission.clone());
        assert!(admission.load(Ordering::SeqCst));
        drop(retired);
        assert!(!admission.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn current_generation_protocol_failure_propagates_to_runner() {
        let (input, receiver) = tokio::sync::mpsc::unbounded_channel();
        let receiver = Arc::new(tokio::sync::Mutex::new(receiver));
        let runner = tokio::spawn(async move {
            let mut requests = GatewayRequests::new();
            run_concurrent_generation(
                &mut requests,
                7,
                AgentCapacity::negotiated(Some(2)),
                move |_| {
                    let receiver = receiver.clone();
                    async move {
                        receiver
                            .lock()
                            .await
                            .recv()
                            .await
                            .context("protocol failure fixture input closed")
                    }
                },
                |_envelope| async { anyhow::bail!("unrecognized gateway response conflict") },
            )
            .await
        });
        input.send(request_envelope(1)).unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), runner)
            .await
            .expect("runner did not receive the protocol failure")
            .unwrap();
        assert!(result.is_err());
    }

    struct PollCancellationCounter {
        cancelled: Arc<std::sync::atomic::AtomicUsize>,
        completed: bool,
    }

    impl Drop for PollCancellationCounter {
        fn drop(&mut self) {
            if !self.completed {
                self.cancelled.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[tokio::test]
    async fn slow_gateway_request_does_not_block_fast_work_or_cancel_a_pending_poll() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (input, receiver) = tokio::sync::mpsc::unbounded_channel();
            let receiver = Arc::new(tokio::sync::Mutex::new(receiver));
            let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
            let (finished, mut finishes) = tokio::sync::mpsc::unbounded_channel();
            let slow = Arc::new(tokio::sync::Notify::new());
            let fast = Arc::new(tokio::sync::Notify::new());
            let heartbeat = Arc::new(tokio::sync::Notify::new());
            let cancelled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let poll_cancelled = cancelled.clone();
            let poll_heartbeat = heartbeat.clone();
            let worker_slow = slow.clone();
            let worker_fast = fast.clone();
            let runner = tokio::spawn(async move {
                let mut requests = GatewayRequests::new();
                let result = run_concurrent_generation(
                    &mut requests,
                    1,
                    AgentCapacity {
                        limit: 2,
                        heartbeat_supported: true,
                    },
                    move |accept| {
                        let receiver = receiver.clone();
                        let heartbeat = poll_heartbeat.clone();
                        let cancelled = poll_cancelled.clone();
                        async move {
                            if !accept {
                                heartbeat.notify_one();
                                return Ok(PollOutcome::Idle);
                            }
                            let mut guard = PollCancellationCounter {
                                cancelled,
                                completed: false,
                            };
                            let result = receiver
                                .lock()
                                .await
                                .recv()
                                .await
                                .context("fixture input closed");
                            guard.completed = true;
                            result
                        }
                    },
                    move |envelope| {
                        let slow = worker_slow.clone();
                        let fast = worker_fast.clone();
                        let started = started.clone();
                        let finished = finished.clone();
                        async move {
                            let id = envelope.request["id"].as_u64().unwrap();
                            started.send(id).unwrap();
                            if id == 1 {
                                slow.notified().await
                            } else {
                                fast.notified().await
                            }
                            finished.send(id).unwrap();
                            Ok(None)
                        }
                    },
                )
                .await;
                while let Some(completed) = requests.join_next().await {
                    completed.unwrap().1.unwrap();
                }
                result
            });
            input.send(request_envelope(1)).unwrap();
            input.send(request_envelope(2)).unwrap();
            let mut ids = [starts.recv().await.unwrap(), starts.recv().await.unwrap()];
            ids.sort();
            assert_eq!(ids, [1, 2]);
            heartbeat.notified().await;
            fast.notify_one();
            assert_eq!(finishes.recv().await, Some(2));
            slow.notify_one();
            assert_eq!(finishes.recv().await, Some(1));
            input
                .send(PollOutcome::Exit(GenerationExit::Disconnected))
                .unwrap();
            assert_eq!(runner.await.unwrap().unwrap(), GenerationExit::Disconnected);
            assert_eq!(cancelled.load(Ordering::SeqCst), 0);
        })
        .await
        .expect("bounded concurrent gateway scenario stalled");
    }

    #[tokio::test]
    async fn retired_generation_keeps_accepted_work_and_shares_capacity_with_its_replacement() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut requests = GatewayRequests::new();
            let capacity = AgentCapacity {
                limit: 2,
                heartbeat_supported: true,
            };
            let first = Arc::new(tokio::sync::Notify::new());
            let second = Arc::new(tokio::sync::Notify::new());
            let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let poll_started = started.clone();
            let worker_started = started.clone();
            let first_worker = first.clone();
            let second_worker = second.clone();
            let mut next_id = 0;
            let retired = run_concurrent_generation(
                &mut requests,
                1,
                capacity,
                move |accept| {
                    next_id += 1;
                    let id = next_id;
                    let started = poll_started.clone();
                    async move {
                        if accept {
                            return Ok(request_envelope(id));
                        }
                        while started.load(Ordering::SeqCst) < 2 {
                            tokio::task::yield_now().await;
                        }
                        Ok(PollOutcome::Exit(GenerationExit::Replaced))
                    }
                },
                move |envelope| {
                    let first = first_worker.clone();
                    let second = second_worker.clone();
                    let started = worker_started.clone();
                    async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        if envelope.request["id"] == 1 {
                            first.notified().await;
                            anyhow::bail!("unrecognized gateway response conflict")
                        } else {
                            second.notified().await;
                            Ok(None)
                        }
                    }
                },
            )
            .await
            .unwrap();
            assert_eq!(retired, GenerationExit::Replaced);
            assert_eq!(requests.len(), 2);

            let heartbeat = Arc::new(tokio::sync::Notify::new());
            let poll_heartbeat = heartbeat.clone();
            let (input, receiver) = tokio::sync::mpsc::unbounded_channel();
            let receiver = Arc::new(tokio::sync::Mutex::new(receiver));
            let (new_started, new_start) = tokio::sync::oneshot::channel();
            let mut new_started = Some(new_started);
            let runner = tokio::spawn(async move {
                let result = run_concurrent_generation(
                    &mut requests,
                    2,
                    capacity,
                    move |accept| {
                        let heartbeat = poll_heartbeat.clone();
                        let receiver = receiver.clone();
                        async move {
                            if !accept {
                                heartbeat.notify_one();
                                return Ok(PollOutcome::Idle);
                            }
                            receiver
                                .lock()
                                .await
                                .recv()
                                .await
                                .context("replacement fixture input closed")
                        }
                    },
                    move |_envelope| {
                        let started = new_started.take().unwrap();
                        async move {
                            started.send(()).unwrap();
                            Ok(None)
                        }
                    },
                )
                .await;
                (result, requests)
            });
            heartbeat.notified().await;
            input.send(request_envelope(3)).unwrap();
            first.notify_one();
            new_start.await.unwrap();
            // A protocol failure from generation 1 must not retire generation 2.
            input
                .send(PollOutcome::Exit(GenerationExit::Disconnected))
                .unwrap();
            let (result, mut requests) = runner.await.unwrap();
            assert_eq!(result.unwrap(), GenerationExit::Disconnected);
            assert!(
                !requests.is_empty(),
                "the other accepted old request must remain owned"
            );
            second.notify_one();
            while let Some(completed) = requests.join_next().await {
                completed.unwrap().1.unwrap();
            }
            assert_eq!(started.load(Ordering::SeqCst), 2);
        })
        .await
        .expect("generation replacement scenario stalled");
    }

    #[tokio::test]
    async fn response_retry_resends_the_same_envelope_and_stale_request_keeps_the_generation() {
        let received = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let observed = received.clone();
        let app = axum::Router::new().route(
            "/v1/hosts/respond",
            axum::routing::post(move |axum::Json(value): axum::Json<Value>| {
                let observed = observed.clone();
                async move {
                    let mut requests = observed.lock().await;
                    requests.push(value);
                    if requests.len() == 1 {
                        (
                            StatusCode::BAD_GATEWAY,
                            axum::Json(json!({"error":"temporary"})),
                        )
                    } else {
                        (
                            StatusCode::CONFLICT,
                            axum::Json(json!({"error":"stale_request"})),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, stopping) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopping.await;
                })
                .await
                .unwrap();
        });
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let context = AgentGeneration {
            gateway: GatewayClient {
                client: client.clone(),
                sync_client: client,
                base_url: format!("http://{address}"),
                host_token: Uuid::new_v4().to_string(),
                access_client_id: None,
                access_client_secret: None,
                browser_auth: None,
                browser_auth_blocked: None,
            },
            route: AgentRoute::Legacy {
                session_id: "upload-fixture".to_owned(),
            },
            instance_id: "same-agent".to_owned(),
            generation: 7,
            capacity: AgentCapacity::negotiated(Some(8)),
            admission: Arc::new(AtomicBool::new(true)),
            browser_authority: None,
            worker_auth: None,
        };
        let result = context
            .upload_response(
                "same-request",
                &json!({"jsonrpc":"2.0","id":42,"result":{}}),
                None,
                None,
            )
            .await;
        shutdown.send(()).unwrap();
        server.await.unwrap();
        assert_eq!(result.unwrap(), None);
        let received = received.lock().await;
        assert_eq!(received.len(), 2);
        assert_eq!(received[0], received[1]);
        assert_eq!(received[0]["generation"], 7);
        assert_eq!(received[0]["request_id"], "same-request");
    }

    #[test]
    fn gateway_url_requires_a_secure_origin() {
        assert_eq!(
            normalize_gateway_url("https://gateway.example.test/").unwrap(),
            "https://gateway.example.test"
        );
        assert_eq!(
            normalize_gateway_url("http://127.0.0.1:8787").unwrap(),
            "http://127.0.0.1:8787"
        );
        assert!(normalize_gateway_url("http://gateway.example.test").is_err());
        assert!(normalize_gateway_url("https://gateway.example.test/mcp").is_err());
        assert!(normalize_gateway_url("https://user@gateway.example.test").is_err());
    }

    #[test]
    fn access_service_token_is_all_or_nothing() {
        assert!(validate_access_service_token(None, None).is_ok());
        assert!(validate_access_service_token(Some("id"), Some("secret")).is_ok());
        assert!(validate_access_service_token(Some("id"), None).is_err());
        assert!(validate_access_service_token(None, Some("secret")).is_err());
    }

    #[test]
    fn generated_access_service_tokens_match_presence_and_nonempty_model() -> noprop::TestResult {
        test_support::run(0x4741_5445_544f_4b45, test_support::DEFAULT_CASES, |ctx| {
            let id = match noprop::sample_usize_in(ctx, 0..=2) {
                0 => None,
                1 => Some(String::new()),
                _ => Some(test_support::safe_component(ctx)),
            };
            let secret = match noprop::sample_usize_in(ctx, 0..=2) {
                0 => None,
                1 => Some("   ".to_owned()),
                _ => Some(test_support::safe_component(ctx)),
            };
            let expected = match (id.as_deref(), secret.as_deref()) {
                (None, None) => true,
                (Some(id), Some(secret)) => !id.trim().is_empty() && !secret.trim().is_empty(),
                _ => false,
            };
            assert_eq!(
                validate_access_service_token(id.as_deref(), secret.as_deref()).is_ok(),
                expected,
                "id={id:?} secret_present={}",
                secret.is_some()
            );
            Ok(())
        })
    }

    #[test]
    fn generated_gateway_urls_match_secure_origin_policy() -> noprop::TestResult {
        test_support::run(0x4741_5445_5741_5955, 512, |ctx| {
            let host = format!("{}.example.test", test_support::safe_component(ctx));
            let safe = format!("https://{host}/");
            assert_eq!(
                normalize_gateway_url(&safe).unwrap(),
                format!("https://{host}")
            );
            let local_port = 1 + noprop::sample_u16(ctx) % 65534;
            let local = format!("http://127.0.0.1:{local_port}");
            assert_eq!(normalize_gateway_url(&local).unwrap(), local);

            let unsafe_value = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => format!("http://{host}"),
                1 => format!("https://{host}/mcp"),
                2 => format!("https://{host}?q=1"),
                3 => format!("https://{host}#fragment"),
                _ => format!("https://user@{host}"),
            };
            assert!(
                normalize_gateway_url(&unsafe_value).is_err(),
                "accepted {unsafe_value:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_empty_poll_delay_enforces_minimum_interval() -> noprop::TestResult {
        test_support::run(0x4741_5445_504f_4c4c, 512, |ctx| {
            let elapsed_ms = noprop::sample_u64(ctx) % 1001;
            let elapsed = Duration::from_millis(elapsed_ms);
            let actual = empty_poll_delay(elapsed);
            let expected = if elapsed < MIN_EMPTY_POLL_INTERVAL {
                MIN_EMPTY_POLL_INTERVAL - elapsed
            } else {
                Duration::ZERO
            };
            assert_eq!(actual, expected, "elapsed_ms={elapsed_ms}");
            assert!(elapsed.saturating_add(actual) >= MIN_EMPTY_POLL_INTERVAL);
            Ok(())
        })
    }

    #[test]
    fn generated_gateway_body_budget_never_overreads() -> noprop::TestResult {
        test_support::run(0x4741_5445_424f_4459, 512, |ctx| {
            let limit = noprop::sample_usize_in(ctx, 0..=1024);
            let chunk_count = noprop::sample_usize_in(ctx, 0..=16);
            let mut body = Vec::new();
            let mut reference_len = 0usize;
            let mut rejected = false;
            for _ in 0..chunk_count {
                let len = noprop::sample_usize_in(ctx, 0..=256);
                let chunk = vec![noprop::sample_u8(ctx); len];
                let expected = reference_len
                    .checked_add(len)
                    .is_some_and(|next| next <= limit);
                let result = append_bounded_body_chunk(&mut body, &chunk, limit, "test");
                assert_eq!(result.is_ok(), expected);
                if expected {
                    reference_len += len;
                    assert_eq!(body.len(), reference_len);
                } else {
                    rejected = true;
                    assert_eq!(body.len(), reference_len);
                    break;
                }
            }
            assert!(body.len() <= limit);
            if rejected {
                assert!(reference_len <= limit);
            }
            Ok(())
        })
    }

    #[test]
    fn federated_platforms_are_limited_to_current_support_boundary() {
        for platform in ["macos", "linux", "wsl2"] {
            assert!(validate_federated_platform(platform).is_ok(), "{platform}");
        }
        assert!(validate_federated_platform("windows").is_err());
        assert!(validate_federated_platform("unknown").is_err());
    }

    #[test]
    fn refreshed_supervisor_metadata_changes_host_connect_payload_and_rejects_incompatible_protocol()
     {
        let before_status = json!({
            "status": "active",
            "version": "2026.9.0",
            "control_protocol": session_control::CONTROL_PROTOCOL_VERSION,
            "named_roots": ["src"]
        });
        let after_status = json!({
            "status": "active",
            "version": "2026.9.1",
            "control_protocol": session_control::CONTROL_PROTOCOL_VERSION,
            "named_roots": ["src", "work"]
        });
        let before = host_supervisor_metadata(&before_status).unwrap();
        let after = host_supervisor_metadata(&after_status).unwrap();
        assert_ne!(before, after);
        assert!(!supervisor_metadata_changed(&before_status, &before).unwrap());
        assert!(supervisor_metadata_changed(&after_status, &before).unwrap());

        let before_payload = serde_json::to_value(host_connect_request(
            "mac-main",
            "instance-a",
            "macos",
            &before,
            None,
        ))
        .unwrap();
        let after_payload = serde_json::to_value(host_connect_request(
            "mac-main",
            "instance-a",
            "macos",
            &after,
            None,
        ))
        .unwrap();
        assert_eq!(before_payload["runtime_version"], "2026.9.0");
        assert_eq!(before_payload["named_roots"], json!(["src"]));
        assert_eq!(after_payload["runtime_version"], "2026.9.1");
        assert_eq!(after_payload["named_roots"], json!(["src", "work"]));

        let incompatible = json!({
            "status": "active",
            "version": "2026.10.0",
            "control_protocol": session_control::CONTROL_PROTOCOL_VERSION + 1,
            "named_roots": ["src"]
        });
        assert!(host_supervisor_metadata(&incompatible).is_err());
        assert!(supervisor_metadata_changed(&incompatible, &before).is_err());
    }

    #[test]
    fn status_root_names_reject_paths_and_accept_names() {
        assert_eq!(
            status_named_roots(&json!({"named_roots": ["src", "work-tree"]})).unwrap(),
            ["src", "work-tree"]
        );
        assert!(status_named_roots(&json!({"named_roots": ["/tmp"]})).is_err());
        assert!(status_named_roots(&json!({})).is_err());
    }

    #[test]
    fn host_session_availability_classifies_live_inventory_without_paths() {
        let ready = HostSessionAvailability::Ready;
        let no_live = HostSessionAvailability::SessionUnavailable;
        assert_eq!(
            classify_host_session_availability(&[("active", false)]),
            ready
        );
        assert_eq!(
            classify_host_session_availability(&[("stopped", false), ("starting", false)]),
            no_live
        );
        assert_eq!(
            classify_host_session_availability(&[("stopped", false), ("failed", false)]),
            no_live
        );
        assert_eq!(
            classify_host_session_availability(&[("active", true)]),
            no_live
        );
        assert_eq!(classify_host_session_availability(&[]), no_live);
        assert_eq!(HostSessionAvailability::Ready.as_str(), "ready");
        assert_eq!(
            HostSessionAvailability::SessionUnavailable.as_str(),
            "session_unavailable"
        );
        assert_eq!(HostSessionAvailability::Unavailable.as_str(), "unavailable");
    }

    #[test]
    fn host_poll_request_serializes_bounded_session_availability() {
        let payload = serde_json::to_value(HostPollRequest {
            host_id: "mac-main",
            instance_id: "instance-a",
            generation: 2,
            session_availability: Some("session_unavailable"),
            accept_requests: Some(false),
            _fabric_auth: None,
        })
        .unwrap();
        assert_eq!(payload["session_availability"], "session_unavailable");
        assert_eq!(payload["generation"], 2);
        assert_eq!(payload["host_id"], "mac-main");
        assert_eq!(payload["accept_requests"], false);

        let omitted = serde_json::to_value(HostPollRequest {
            host_id: "mac-main",
            instance_id: "instance-a",
            generation: 2,
            session_availability: None,
            accept_requests: None,
            _fabric_auth: None,
        })
        .unwrap();
        assert!(omitted.get("session_availability").is_none());
        assert!(omitted.get("accept_requests").is_none());
    }

    #[tokio::test]
    async fn gateway_dispatch_preserves_json_rpc_ids_and_errors() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "request-1",
            "method": "missing/method"
        });
        let response = dispatch_response(&request).await;
        assert_eq!(response["id"], "request-1");
        assert_eq!(response["error"]["code"], -32000);
    }

    #[test]
    fn host_agent_connection_record_round_trips_and_is_removed_on_drop() {
        let state = tempfile::tempdir().unwrap();
        let host_id = format!("record-{}", Uuid::new_v4().simple());
        assert_eq!(read_host_agent_generation_in(state.path(), &host_id), None);

        let record = HostAgentConnectionRecordFile::create_in(state.path(), &host_id, 7).unwrap();
        assert_eq!(
            read_host_agent_generation_in(state.path(), &host_id),
            Some(7)
        );
        let path = host_agent_record_path_in(state.path(), &host_id).unwrap();
        assert!(path.is_file());

        drop(record);
        assert_eq!(read_host_agent_generation_in(state.path(), &host_id), None);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn host_agent_connection_record_read_rejects_public_mode_and_symlink() {
        use std::os::unix::fs::symlink;

        fn set_mode(path: &std::path::Path, mode: u32) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        let state = tempfile::tempdir().unwrap();
        let host_id = format!("record-{}", Uuid::new_v4().simple());
        let record = HostAgentConnectionRecordFile::create_in(state.path(), &host_id, 3).unwrap();
        let path = host_agent_record_path_in(state.path(), &host_id).unwrap();

        set_mode(&path, 0o644);
        assert_eq!(read_host_agent_generation_in(state.path(), &host_id), None);

        set_mode(&path, 0o600);
        drop(record);

        let target = path.with_extension("target");
        std::fs::write(
            &target,
            b"{\"schema\":1,\"host_id\":\"other\",\"generation\":1,\"updated_at\":0}",
        )
        .unwrap();
        set_mode(&target, 0o600);
        symlink(&target, &path).unwrap();
        assert_eq!(read_host_agent_generation_in(state.path(), &host_id), None);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&target).unwrap();
    }
}
