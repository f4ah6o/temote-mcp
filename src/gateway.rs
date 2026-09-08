use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

use crate::{approvals, config, host_identity, mcp, session_control};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(40);
const MAX_GATEWAY_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_GATEWAY_POLL_ENVELOPE_BYTES: usize = 64 * 1024;
const MAX_GATEWAY_POLL_RESPONSE_BYTES: usize =
    MAX_GATEWAY_RESPONSE_BYTES + MAX_GATEWAY_POLL_ENVELOPE_BYTES;
const _: () = assert!(MAX_GATEWAY_POLL_ENVELOPE_BYTES >= 64 * 1024);
const MAX_GATEWAY_ERROR_BYTES: usize = 64 * 1024;
const MAX_GATEWAY_ERROR_DISPLAY_CHARS: usize = 4096;
const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_secs(2);
const MIN_EMPTY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const HOST_AGENT_PROTOCOL_VERSION: u64 = 1;
const HOST_CAPABILITIES: &[&str] = &["session_lifecycle", "session_tools", "named_roots"];

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

#[derive(Clone)]
struct GatewayClient {
    client: Client,
    base_url: String,
    host_token: String,
    access_client_id: Option<String>,
    access_client_secret: Option<String>,
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
}

#[derive(Serialize)]
struct LegacyGenerationRequest<'a> {
    session_id: &'a str,
    instance_id: &'a str,
    generation: u64,
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
}

#[derive(Serialize)]
struct HostGenerationRequest<'a> {
    host_id: &'a str,
    instance_id: &'a str,
    generation: u64,
}

#[derive(Deserialize)]
struct PollEnvelope {
    request_id: String,
    request: Value,
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
        base_url,
        host_token: options.host_token,
        access_client_id: options.access_client_id,
        access_client_secret: options.access_client_secret,
    };
    let reconnect_delay = if options.reconnect_delay.is_zero() {
        DEFAULT_RECONNECT_DELAY
    } else {
        options.reconnect_delay
    };
    let platform = options.platform.resolve();
    let instance_id = Uuid::new_v4().to_string();

    if let Some(host_id) = options.host_id {
        return run_host_agent(&gateway, &host_id, &instance_id, platform, reconnect_delay).await;
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
) -> Result<()> {
    validate_federated_platform(platform)?;
    let host_id = host_identity::validate(host_id)?;
    let sessions = session_control::SessionBackend::local_control().await?;

    eprintln!(
        "temote-mcp federated gateway agent\nhost_id: {}\nplatform: {}\ninstance_id: {}\ngateway: {}",
        host_id, platform, instance_id, gateway.base_url,
    );

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
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
            if metadata.named_roots.is_empty() {
                "(none)".to_owned()
            } else {
                metadata.named_roots.join(",")
            }
        );

        let outcome = tokio::select! {
            result = run_host_generation(
                gateway,
                &sessions,
                &host_id,
                instance_id,
                connection.generation,
                &metadata,
            ) => result,
            signal = &mut ctrl_c => {
                signal.context("failed to receive Ctrl-C")?;
                disconnect_host(
                    gateway,
                    &host_id,
                    instance_id,
                    connection.generation,
                ).await;
                eprintln!("Stopping gateway agent for host {host_id}");
                return Ok(());
            }
        };

        log_generation_outcome(outcome, connection.generation);
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
) -> Result<HostConnectResponse> {
    let response = gateway
        .request(Method::POST, "/v1/hosts/connect", Some(host_id))
        .json(&host_connect_request(
            host_id,
            instance_id,
            platform,
            metadata,
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
) -> Result<GenerationExit> {
    loop {
        if !config::session_is_active(session_id).await? {
            disconnect_legacy(gateway, session_id, instance_id, generation).await;
            return Ok(GenerationExit::Disconnected);
        }

        let poll_started = Instant::now();
        let response = gateway
            .request(Method::POST, "/v1/hosts/poll", None)
            .json(&LegacyGenerationRequest {
                session_id,
                instance_id,
                generation,
            })
            .send()
            .await
            .context("gateway poll request failed")?;

        let Some(envelope) = poll_envelope(response, poll_started).await? else {
            continue;
        };
        if envelope.1 {
            return Ok(GenerationExit::Replaced);
        }
        let envelope = envelope.0.context("gateway poll returned no envelope")?;
        let rpc_response = dispatch_response(&envelope.request).await;

        let response = gateway
            .request(Method::POST, "/v1/hosts/respond", None)
            .json(&LegacyResponseRequest {
                session_id,
                instance_id,
                generation,
                request_id: &envelope.request_id,
                response: &rpc_response,
            })
            .send()
            .await
            .context("gateway response upload failed")?;
        if response.status() == StatusCode::CONFLICT {
            return Ok(GenerationExit::Replaced);
        }
        require_success(response, "gateway response upload").await?;
    }
}

async fn run_host_generation(
    gateway: &GatewayClient,
    sessions: &session_control::SessionBackend,
    host_id: &str,
    instance_id: &str,
    generation: u64,
    connected_metadata: &HostSupervisorMetadata,
) -> Result<GenerationExit> {
    loop {
        let status = match sessions.status().await {
            Ok(status) => status,
            Err(error) => {
                disconnect_host(gateway, host_id, instance_id, generation).await;
                return Err(error).context("local supervisor is unavailable");
            }
        };
        let current_metadata = match host_supervisor_metadata(&status) {
            Ok(metadata) => metadata,
            Err(error) => {
                disconnect_host(gateway, host_id, instance_id, generation).await;
                return Err(error);
            }
        };
        if &current_metadata != connected_metadata {
            disconnect_host(gateway, host_id, instance_id, generation).await;
            return Ok(GenerationExit::SupervisorChanged);
        }

        let poll_started = Instant::now();
        let response = gateway
            .request(Method::POST, "/v1/hosts/poll", Some(host_id))
            .json(&HostGenerationRequest {
                host_id,
                instance_id,
                generation,
            })
            .send()
            .await
            .context("gateway host poll request failed")?;

        let Some(envelope) = poll_envelope(response, poll_started).await? else {
            continue;
        };
        if envelope.1 {
            return Ok(GenerationExit::Replaced);
        }
        let envelope = envelope
            .0
            .context("gateway host poll returned no envelope")?;
        let rpc_response = dispatch_host_response(&envelope.request, sessions).await;

        let response = gateway
            .request(Method::POST, "/v1/hosts/respond", Some(host_id))
            .json(&HostResponseRequest {
                host_id,
                instance_id,
                generation,
                request_id: &envelope.request_id,
                response: &rpc_response,
            })
            .send()
            .await
            .context("gateway host response upload failed")?;
        if response.status() == StatusCode::CONFLICT {
            return Ok(GenerationExit::Replaced);
        }
        require_success(response, "gateway host response upload").await?;
    }
}

async fn poll_envelope(
    response: Response,
    poll_started: Instant,
) -> Result<Option<(Option<PollEnvelope>, bool)>> {
    if response.status() == StatusCode::NO_CONTENT {
        let delay = empty_poll_delay(poll_started.elapsed());
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        return Ok(None);
    }
    if response.status() == StatusCode::CONFLICT {
        return Ok(Some((None, true)));
    }
    let response = require_success(response, "gateway poll").await?;
    let bytes =
        read_bounded_body(response, MAX_GATEWAY_POLL_RESPONSE_BYTES, "gateway poll").await?;
    let envelope: PollEnvelope =
        serde_json::from_slice(&bytes).context("gateway poll returned invalid JSON")?;
    Ok(Some((Some(envelope), false)))
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
) {
    let result = gateway
        .request(Method::POST, "/v1/hosts/disconnect", Some(host_id))
        .json(&HostGenerationRequest {
            host_id,
            instance_id,
            generation,
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
) -> HostConnectRequest<'a> {
    HostConnectRequest {
        host_id,
        instance_id,
        platform,
        agent_protocol: HOST_AGENT_PROTOCOL_VERSION,
        runtime_version: &metadata.runtime_version,
        control_protocol: metadata.control_protocol,
        capabilities: HOST_CAPABILITIES,
        named_roots: &metadata.named_roots,
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
    fn request(&self, method: Method, path: &str, host_id: Option<&str>) -> RequestBuilder {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(&self.host_token);
        if let Some(host_id) = host_id {
            request = request.header("X-Temote-Host-Id", host_id);
        }
        if let (Some(client_id), Some(client_secret)) = (
            self.access_client_id.as_deref(),
            self.access_client_secret.as_deref(),
        ) {
            request = request
                .header("CF-Access-Client-Id", client_id)
                .header("CF-Access-Client-Secret", client_secret);
        }
        request
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

async fn read_bounded_body(
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

fn normalize_gateway_url(value: &str) -> Result<String> {
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
        let before = host_supervisor_metadata(&json!({
            "status": "active",
            "version": "2026.9.0",
            "control_protocol": session_control::CONTROL_PROTOCOL_VERSION,
            "named_roots": ["src"]
        }))
        .unwrap();
        let after = host_supervisor_metadata(&json!({
            "status": "active",
            "version": "2026.9.1",
            "control_protocol": session_control::CONTROL_PROTOCOL_VERSION,
            "named_roots": ["src", "work"]
        }))
        .unwrap();
        assert_ne!(before, after);

        let before_payload = serde_json::to_value(host_connect_request(
            "mac-main",
            "instance-a",
            "macos",
            &before,
        ))
        .unwrap();
        let after_payload = serde_json::to_value(host_connect_request(
            "mac-main",
            "instance-a",
            "macos",
            &after,
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
}
