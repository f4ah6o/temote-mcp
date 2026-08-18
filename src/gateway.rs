use std::time::Duration;

use anyhow::{Context, Result};
use clap::ValueEnum;
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

use crate::{approvals, config, mcp};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(40);
const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Platform {
    Auto,
    Macos,
    Linux,
    Wsl2,
    Windows,
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
    pub session_id: String,
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
struct ConnectRequest<'a> {
    session_id: &'a str,
    instance_id: &'a str,
    platform: &'a str,
}

#[derive(Deserialize)]
struct ConnectResponse {
    session_id: String,
    generation: u64,
    lease_seconds: u64,
}

#[derive(Serialize)]
struct GenerationRequest<'a> {
    session_id: &'a str,
    instance_id: &'a str,
    generation: u64,
}

#[derive(Deserialize)]
struct PollEnvelope {
    request_id: String,
    request: Value,
}

#[derive(Serialize)]
struct ResponseRequest<'a> {
    session_id: &'a str,
    instance_id: &'a str,
    generation: u64,
    request_id: &'a str,
    response: &'a Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenerationExit {
    Replaced,
    Disconnected,
}

pub async fn run_agent(options: AgentOptions) -> Result<()> {
    config::validate_session_id(&options.session_id)?;
    anyhow::ensure!(
        !options.host_token.trim().is_empty(),
        "gateway host token must not be empty"
    );
    validate_access_service_token(
        options.access_client_id.as_deref(),
        options.access_client_secret.as_deref(),
    )?;

    let session = config::load_session(&options.session_id).await?;
    let base_url = normalize_gateway_url(&options.gateway_url)?;
    let platform = options.platform.resolve();
    let instance_id = Uuid::new_v4().to_string();
    let approved = approvals::request(
        &session.id,
        "gateway_connect",
        format!("gateway={base_url} platform={platform} instance_id={instance_id}"),
        session.cwd.clone(),
    )
    .await?;
    anyhow::ensure!(approved, "gateway connection was denied at the endpoint");

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

    eprintln!(
        "temote-mcp gateway agent approved\nsession_id: {}\nplatform: {}\ninstance_id: {}\ngateway: {}",
        session.id, platform, instance_id, gateway.base_url
    );

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let connected = tokio::select! {
            result = connect(&gateway, &session.id, &instance_id, platform) => result,
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
            "gateway connected: session_id={} generation={} lease_seconds={}",
            connection.session_id, connection.generation, connection.lease_seconds
        );

        let outcome = tokio::select! {
            result = run_generation(
                &gateway,
                &session.id,
                &instance_id,
                connection.generation,
            ) => result,
            signal = &mut ctrl_c => {
                signal.context("failed to receive Ctrl-C")?;
                disconnect(
                    &gateway,
                    &session.id,
                    &instance_id,
                    connection.generation,
                ).await;
                eprintln!("Stopping gateway agent for session {}", session.id);
                return Ok(());
            }
        };

        match outcome {
            Ok(GenerationExit::Replaced) => {
                eprintln!(
                    "gateway generation {} was replaced; reconnecting",
                    connection.generation
                );
            }
            Ok(GenerationExit::Disconnected) => {
                eprintln!("gateway disconnected; reconnecting");
            }
            Err(error) => {
                eprintln!("gateway generation ended: {error:#}");
            }
        }
        if wait_or_stop(reconnect_delay, &mut ctrl_c, &session.id).await? {
            return Ok(());
        }
    }
}

async fn wait_or_stop<F>(
    delay: Duration,
    ctrl_c: &mut std::pin::Pin<&mut F>,
    session_id: &str,
) -> Result<bool>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(false),
        signal = ctrl_c => {
            signal.context("failed to receive Ctrl-C")?;
            eprintln!("Stopping gateway agent for session {session_id}");
            Ok(true)
        }
    }
}

async fn connect(
    gateway: &GatewayClient,
    session_id: &str,
    instance_id: &str,
    platform: &str,
) -> Result<ConnectResponse> {
    let response = gateway
        .request(Method::POST, "/v1/hosts/connect")
        .json(&ConnectRequest {
            session_id,
            instance_id,
            platform,
        })
        .send()
        .await
        .context("gateway connect request failed")?;
    let response = require_success(response, "gateway connect").await?;
    let body: ConnectResponse = response
        .json()
        .await
        .context("gateway connect returned invalid JSON")?;
    anyhow::ensure!(
        body.session_id == session_id,
        "gateway returned a different session_id"
    );
    Ok(body)
}

async fn run_generation(
    gateway: &GatewayClient,
    session_id: &str,
    instance_id: &str,
    generation: u64,
) -> Result<GenerationExit> {
    loop {
        if !config::session_is_active(session_id).await? {
            disconnect(gateway, session_id, instance_id, generation).await;
            return Ok(GenerationExit::Disconnected);
        }

        let response = gateway
            .request(Method::POST, "/v1/hosts/poll")
            .json(&GenerationRequest {
                session_id,
                instance_id,
                generation,
            })
            .send()
            .await
            .context("gateway poll request failed")?;

        if response.status() == StatusCode::NO_CONTENT {
            continue;
        }
        if response.status() == StatusCode::CONFLICT {
            return Ok(GenerationExit::Replaced);
        }
        let response = require_success(response, "gateway poll").await?;
        let envelope: PollEnvelope = response
            .json()
            .await
            .context("gateway poll returned invalid JSON")?;
        let rpc_response = dispatch_response(&envelope.request).await;

        let response = gateway
            .request(Method::POST, "/v1/hosts/respond")
            .json(&ResponseRequest {
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

async fn dispatch_response(request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    match mcp::dispatch_public(request).await {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": format!("{error:#}")}
        }),
    }
}

async fn disconnect(gateway: &GatewayClient, session_id: &str, instance_id: &str, generation: u64) {
    let result = gateway
        .request(Method::POST, "/v1/hosts/disconnect")
        .json(&GenerationRequest {
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

impl GatewayClient {
    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(&self.host_token);
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
    let body = response.text().await.unwrap_or_default();
    anyhow::bail!("{operation} failed with HTTP {status}: {body}")
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
    anyhow::ensure!(
        client_id.is_some() == client_secret.is_some(),
        "Cloudflare Access client ID and secret must be provided together"
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
