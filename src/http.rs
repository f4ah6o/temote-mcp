//! Streamable HTTP MCP endpoint for remote clients such as ChatGPT.
//!
//! Authentication is selected by the active production profile. Cloudflare
//! uses Access JWT assertions; Tailscale uses Temote local OAuth. MCP dispatch
//! remains provider-neutral.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Form, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::local_oauth::{AuthorizeRequest, OAuthError, RegistrationRequest, TokenRequest};
use crate::provider::AuthProvider;
#[cfg(test)]
use crate::provider::PublicEndpoint;
use crate::supervisor::SessionSupervisor;

const MAX_OAUTH_REGISTER_BODY_BYTES: usize = 128 * 1024;
const MAX_OAUTH_TOKEN_BODY_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct Runtime {
    pub authenticator: AuthProvider,
    pub supervisor: Arc<SessionSupervisor>,
}

pub async fn serve(
    addr: SocketAddr,
    public_url: String,
    authenticator: AuthProvider,
    supervisor: Arc<SessionSupervisor>,
) -> Result<()> {
    let runtime = Runtime {
        authenticator,
        supervisor,
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    eprintln!("temote-mcp HTTP server listening on http://{addr}");
    eprintln!("MCP endpoint for remote clients: {public_url}/mcp");
    eprintln!("Authentication: {}", runtime.authenticator.name());
    axum::serve(listener, router(runtime))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

pub fn router(runtime: Runtime) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth_protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth_authorization_server),
        )
        .route(
            "/register",
            axum::routing::post(oauth_register)
                .layer(DefaultBodyLimit::max(MAX_OAUTH_REGISTER_BODY_BYTES)),
        )
        .route("/authorize", get(oauth_authorize))
        .route(
            "/token",
            axum::routing::post(oauth_token)
                .layer(DefaultBodyLimit::max(MAX_OAUTH_TOKEN_BODY_BYTES)),
        )
        .route(
            "/mcp",
            get(mcp_get)
                .post(mcp_post)
                .delete(mcp_delete)
                .options(mcp_options),
        )
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(runtime)
}

#[cfg(test)]
pub fn normalize_public_url(value: &str) -> Result<String> {
    PublicEndpoint::parse(value).map(PublicEndpoint::into_string)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            let _ = signal.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

async fn healthz() -> Response {
    Json(json!({"status": "ok", "service": "temote-mcp"})).into_response()
}

async fn oauth_protected_resource(State(runtime): State<Runtime>) -> Response {
    match runtime.authenticator.local_oauth() {
        Some(local) => no_store(Json(local.protected_resource_metadata()).into_response()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn oauth_authorization_server(State(runtime): State<Runtime>) -> Response {
    match runtime.authenticator.local_oauth() {
        Some(local) => no_store(Json(local.authorization_server_metadata()).into_response()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn oauth_register(
    State(runtime): State<Runtime>,
    Json(request): Json<RegistrationRequest>,
) -> Response {
    let Some(local) = runtime.authenticator.local_oauth() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match local.register(request).await {
        Ok(value) => no_store((StatusCode::CREATED, Json(value)).into_response()),
        Err(error) => oauth_error(error),
    }
}

async fn oauth_authorize(
    State(runtime): State<Runtime>,
    Query(request): Query<AuthorizeRequest>,
) -> Response {
    let Some(local) = runtime.authenticator.local_oauth() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match local.authorize(request).await {
        Ok(location) => {
            let Ok(location) = HeaderValue::from_str(&location) else {
                return oauth_error(OAuthError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    code: "server_error",
                    description: "generated redirect URI is invalid".to_owned(),
                });
            };
            let mut response = StatusCode::FOUND.into_response();
            response.headers_mut().insert(header::LOCATION, location);
            no_store(response)
        }
        Err(error) => oauth_error(error),
    }
}

async fn oauth_token(
    State(runtime): State<Runtime>,
    Form(request): Form<TokenRequest>,
) -> Response {
    let Some(local) = runtime.authenticator.local_oauth() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match local.token(request).await {
        Ok(value) => no_store(Json(value).into_response()),
        Err(error) => oauth_error(error),
    }
}

fn oauth_error(error: OAuthError) -> Response {
    no_store((error.status, Json(error.json())).into_response())
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Streamable HTTP uses POST for this stateless endpoint. SSE is deliberately
/// not exposed.
async fn mcp_get() -> Response {
    with_cors((
        StatusCode::METHOD_NOT_ALLOWED,
        "SSE streams are not supported; use Streamable HTTP POST",
    ))
}

async fn mcp_delete(headers: HeaderMap, State(runtime): State<Runtime>) -> Response {
    if let Err(error) = runtime.authenticator.authenticate(&headers).await {
        return unauthorized(&runtime, error);
    }
    with_cors(StatusCode::OK)
}

async fn mcp_options() -> Response {
    with_cors(StatusCode::NO_CONTENT)
}

async fn mcp_post(headers: HeaderMap, State(runtime): State<Runtime>, body: Bytes) -> Response {
    if let Err(error) = runtime.authenticator.authenticate(&headers).await {
        return unauthorized(&runtime, error);
    }
    let request: Value = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return with_cors(Json(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": error.to_string()}
            })));
        }
    };
    let started = Instant::now();
    let audit = AuditContext::from_request(&request);
    if let Some(response) = validate_modern_http_request(&headers, &request) {
        audit.finish(response.status(), started);
        return response;
    }

    // Notifications and responses carry no id and expect no reply.
    let Some(id) = request.get("id").cloned() else {
        let response = with_cors(StatusCode::ACCEPTED);
        audit.finish(response.status(), started);
        return response;
    };
    let response_value =
        match crate::mcp::dispatch_public(&request, Some(&runtime.supervisor)).await {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": format!("{error:#}")}
            }),
        };
    let response = with_cors(Json(response_value));
    audit.finish(response.status(), started);
    response
}

fn validate_modern_http_request(headers: &HeaderMap, request: &Value) -> Option<Response> {
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let header_version = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok());
    let modern_meta = request
        .pointer("/params/_meta")
        .and_then(Value::as_object)
        .is_some_and(|meta| {
            [
                "io.modelcontextprotocol/protocolVersion",
                "io.modelcontextprotocol/clientCapabilities",
                "io.modelcontextprotocol/clientInfo",
                "io.modelcontextprotocol/logLevel",
            ]
            .iter()
            .any(|key| meta.contains_key(*key))
        });
    let modern = method == "server/discover"
        || modern_meta
        || header_version == Some(crate::mcp::MODERN_PROTOCOL_VERSION);
    if !modern {
        return None;
    }

    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let meta = request.pointer("/params/_meta").and_then(Value::as_object);
    let Some(meta) = meta else {
        return Some(modern_error(
            StatusCode::BAD_REQUEST,
            id,
            -32602,
            "modern MCP requests require params._meta",
            None,
        ));
    };
    let Some(version) = meta
        .get("io.modelcontextprotocol/protocolVersion")
        .and_then(Value::as_str)
    else {
        return Some(modern_error(
            StatusCode::BAD_REQUEST,
            id,
            -32602,
            "missing io.modelcontextprotocol/protocolVersion",
            None,
        ));
    };
    if !meta
        .get("io.modelcontextprotocol/clientCapabilities")
        .is_some_and(Value::is_object)
    {
        return Some(modern_error(
            StatusCode::BAD_REQUEST,
            id,
            -32602,
            "missing or invalid io.modelcontextprotocol/clientCapabilities",
            None,
        ));
    }
    if header_version != Some(version) {
        return Some(modern_error(
            StatusCode::BAD_REQUEST,
            id,
            -32020,
            "MCP-Protocol-Version header must match request _meta",
            None,
        ));
    }
    if version != crate::mcp::MODERN_PROTOCOL_VERSION {
        return Some(modern_error(
            StatusCode::BAD_REQUEST,
            id,
            -32022,
            "unsupported MCP protocol version",
            Some(json!({
                "supported": [crate::mcp::MODERN_PROTOCOL_VERSION],
                "requested": version
            })),
        ));
    }
    let header_method = headers
        .get("mcp-method")
        .and_then(|value| value.to_str().ok());
    if header_method != Some(method) {
        return Some(modern_error(
            StatusCode::BAD_REQUEST,
            id,
            -32020,
            "Mcp-Method header must match the JSON-RPC method",
            None,
        ));
    }
    if method == "tools/call" {
        let name = request.pointer("/params/name").and_then(Value::as_str);
        let header_name = headers
            .get("mcp-name")
            .and_then(|value| value.to_str().ok());
        if name.is_none() || header_name != name {
            return Some(modern_error(
                StatusCode::BAD_REQUEST,
                id,
                -32020,
                "Mcp-Name header must match params.name",
                None,
            ));
        }
    }
    None
}

fn modern_error(
    status: StatusCode,
    id: Value,
    code: i64,
    message: &str,
    data: Option<Value>,
) -> Response {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    with_cors((
        status,
        Json(json!({"jsonrpc": "2.0", "id": id, "error": error})),
    ))
}

struct AuditContext {
    method: String,
    tool: String,
    session_id: String,
}

impl AuditContext {
    fn from_request(request: &Value) -> Self {
        Self {
            method: request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            tool: request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_owned(),
            session_id: request
                .pointer("/params/arguments/session_id")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_owned(),
        }
    }

    fn finish(&self, status: StatusCode, started: Instant) {
        eprintln!(
            "[audit] method={} tool={} session_id={} status={} duration_ms={}",
            self.method,
            self.tool,
            self.session_id,
            status.as_u16(),
            started.elapsed().as_millis()
        );
    }
}

fn unauthorized(runtime: &Runtime, error: anyhow::Error) -> Response {
    eprintln!("[auth] rejected HTTP request: {error:#}");
    let (code, description) = if runtime.authenticator.local_oauth().is_some() {
        (
            "invalid_token",
            "valid Temote OAuth Bearer authentication is required",
        )
    } else {
        (
            "access_unauthorized",
            "valid Cloudflare Access authentication is required",
        )
    };
    let mut response = with_cors((
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": code,
            "error_description": description
        })),
    ));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(local) = runtime.authenticator.local_oauth() {
        let challenge = format!(
            "Bearer resource_metadata=\"{}\", scope=\"mcp\"",
            local.resource_metadata_url()
        );
        if let Ok(value) = HeaderValue::from_str(&challenge) {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, value);
        }
    }
    response
}

fn with_cors(response: impl IntoResponse) -> Response {
    let (mut parts, body) = response.into_response().into_parts();
    parts
        .headers
        .insert("access-control-allow-origin", HeaderValue::from_static("*"));
    parts.headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET,POST,DELETE,OPTIONS"),
    );
    parts.headers.insert(
        "access-control-allow-headers",
        HeaderValue::from_static(
            "accept,authorization,content-type,mcp-protocol-version,mcp-method,mcp-name,mcp-session-id",
        ),
    );
    parts.headers.insert(
        "access-control-expose-headers",
        HeaderValue::from_static("mcp-session-id"),
    );
    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::{AccessAuthenticator, AccessIdentity};
    use crate::local_oauth::LocalOAuth;
    use crate::test_support;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use url::Url;

    fn runtime() -> Runtime {
        let (supervisor, _approvals) =
            SessionSupervisor::new(crate::named_roots::NamedRoots::default());
        Runtime {
            authenticator: AuthProvider::Cloudflare(AccessAuthenticator::test(
                "test-token",
                AccessIdentity {
                    email: "test@example.com".to_owned(),
                    subject: "test-subject".to_owned(),
                },
            )),
            supervisor,
        }
    }

    fn local_runtime() -> (Runtime, crate::approvals::ApprovalReceiver) {
        let (supervisor, approvals) =
            SessionSupervisor::new(crate::named_roots::NamedRoots::default());
        let local = LocalOAuth::new(
            "https://node.example.ts.net".to_owned(),
            supervisor.approval_sender(),
        );
        (
            Runtime {
                authenticator: AuthProvider::Local(local),
                supervisor,
            },
            approvals,
        )
    }

    async fn body_json(response: Response) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn oauth_routes_enforce_local_body_limits() {
        let (runtime, _approvals) = local_runtime();
        let register_body = serde_json::to_vec(&json!({
            "redirect_uris": ["http://127.0.0.1:9876/callback"],
            "client_name": "x".repeat(MAX_OAUTH_REGISTER_BODY_BYTES),
            "application_type": "native",
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        }))
        .unwrap();
        assert!(register_body.len() > MAX_OAUTH_REGISTER_BODY_BYTES);
        let register = router(runtime.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("content-type", "application/json")
                    .body(Body::from(register_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(register.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair("grant_type", "authorization_code")
            .append_pair("code", &"x".repeat(MAX_OAUTH_TOKEN_BODY_BYTES))
            .append_pair("client_id", "client")
            .append_pair("redirect_uri", "http://127.0.0.1:9876/callback")
            .append_pair("code_verifier", &"a".repeat(43))
            .append_pair("resource", "https://node.example.ts.net/mcp");
        let token_body = form.finish();
        assert!(token_body.len() > MAX_OAUTH_TOKEN_BODY_BYTES);
        let token = router(runtime)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/token")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(token_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(token.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn normalizes_only_https_origins() {
        assert_eq!(
            normalize_public_url("https://localmcp.example.test/").unwrap(),
            "https://localmcp.example.test"
        );
        assert!(normalize_public_url("http://localmcp.example.test").is_err());
        assert!(normalize_public_url("https://localmcp.example.test/mcp").is_err());
    }

    #[test]
    fn generated_public_urls_accept_only_https_origins() -> noprop::TestResult {
        test_support::run(0x4854_5450_4f52_4947, 512, |ctx| {
            let host = format!("{}.example.test", test_support::safe_component(ctx));
            let safe = format!("  https://{host}/  ");
            assert_eq!(
                normalize_public_url(&safe).unwrap(),
                format!("https://{host}")
            );

            let unsafe_value = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => format!("http://{host}"),
                1 => format!("https://{host}/mcp"),
                2 => format!("https://{host}?q=1"),
                3 => format!("https://{host}#fragment"),
                _ => format!("https://user@{host}"),
            };
            assert!(
                normalize_public_url(&unsafe_value).is_err(),
                "accepted {unsafe_value:?}"
            );
            Ok(())
        })
    }

    #[tokio::test]
    async fn unauthenticated_mcp_calls_are_rejected() {
        let response = router(runtime())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_json(response).await["error"], "access_unauthorized");
    }

    #[tokio::test]
    async fn local_oauth_http_flow_discovers_authorizes_and_authenticates_mcp() {
        let (runtime, mut approvals) = local_runtime();

        let metadata = router(runtime.clone())
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(metadata.status(), StatusCode::OK);
        assert_eq!(
            body_json(metadata).await["resource"],
            "https://node.example.ts.net/mcp"
        );

        let unauthorized = router(runtime.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert!(
            unauthorized
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("oauth-protected-resource")
        );

        let registration = router(runtime.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "client_name": "HTTP test client",
                            "application_type": "native",
                            "redirect_uris": ["http://127.0.0.1:9876/callback"],
                            "grant_types": ["authorization_code"],
                            "response_types": ["code"],
                            "token_endpoint_auth_method": "none"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registration.status(), StatusCode::CREATED);
        let client_id = body_json(registration).await["client_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", "http://127.0.0.1:9876/callback")
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", "https://node.example.ts.net/mcp")
            .append_pair("scope", "mcp")
            .append_pair("state", "state-123");
        let authorize_uri = format!("/authorize?{}", query.finish());
        let authorize_task = {
            let runtime = runtime.clone();
            tokio::spawn(async move {
                router(runtime)
                    .oneshot(
                        Request::builder()
                            .uri(authorize_uri)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            })
        };
        let prompt = approvals.recv().await.unwrap();
        assert_eq!(prompt.request.operation, "oauth_authorize");
        prompt.respond(true);
        let authorize = authorize_task.await.unwrap();
        assert_eq!(authorize.status(), StatusCode::FOUND);
        let location = authorize
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        let location = Url::parse(location).unwrap();
        let params = location
            .query_pairs()
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(params.get("state").map(String::as_str), Some("state-123"));
        let code = params.get("code").unwrap();

        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", "http://127.0.0.1:9876/callback")
            .append_pair("code_verifier", verifier)
            .append_pair("resource", "https://node.example.ts.net/mcp");
        let token = router(runtime.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/token")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form.finish()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(token.status(), StatusCode::OK);
        let access_token = body_json(token).await["access_token"]
            .as_str()
            .unwrap()
            .to_owned();

        let mcp = router(runtime)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {access_token}"))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mcp.status(), StatusCode::OK);
        let tools = body_json(mcp).await["result"]["tools"]
            .as_array()
            .unwrap()
            .clone();
        assert!(!tools.iter().any(|tool| tool["name"] == "without_sandbox"));
    }

    #[tokio::test]
    async fn authenticated_tool_list_is_sandbox_only() {
        let response = router(runtime())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("cf-access-jwt-assertion", "test-token")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let tools = body_json(response).await["result"]["tools"]
            .as_array()
            .unwrap()
            .clone();
        assert!(tools.iter().any(|tool| tool["name"] == "session_list"));
        assert!(!tools.iter().any(|tool| tool["name"] == "without_sandbox"));
        assert_eq!(
            tools
                .iter()
                .find(|tool| tool["name"] == "write_file")
                .unwrap()["annotations"]["readOnlyHint"],
            false
        );
        assert_eq!(
            tools
                .iter()
                .find(|tool| tool["name"] == "read_file")
                .unwrap()["annotations"]["readOnlyHint"],
            true
        );
    }

    #[tokio::test]
    async fn modern_discovery_requires_and_accepts_standard_headers() {
        let response = router(runtime())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("cf-access-jwt-assertion", "test-token")
                    .header("mcp-protocol-version", crate::mcp::MODERN_PROTOCOL_VERSION)
                    .header("mcp-method", "server/discover")
                    .body(Body::from(format!(
                        r#"{{"jsonrpc":"2.0","id":"discover-1","method":"server/discover","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"{}","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#,
                        crate::mcp::MODERN_PROTOCOL_VERSION
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let result = body_json(response).await["result"].clone();
        assert_eq!(result["resultType"], "complete");
        assert_eq!(
            result["supportedVersions"],
            json!([crate::mcp::MODERN_PROTOCOL_VERSION])
        );
    }

    #[tokio::test]
    async fn modern_tool_list_rejects_missing_method_header() {
        let response = router(runtime())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("cf-access-jwt-assertion", "test-token")
                    .header("mcp-protocol-version", crate::mcp::MODERN_PROTOCOL_VERSION)
                    .body(Body::from(format!(
                        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"{}","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#,
                        crate::mcp::MODERN_PROTOCOL_VERSION
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error = body_json(response).await["error"].clone();
        assert_eq!(error["code"], -32020);
    }

    #[tokio::test]
    async fn modern_tool_list_returns_cacheable_result_shape() {
        let response = router(runtime())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("cf-access-jwt-assertion", "test-token")
                    .header("mcp-protocol-version", crate::mcp::MODERN_PROTOCOL_VERSION)
                    .header("mcp-method", "tools/list")
                    .body(Body::from(format!(
                        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"{}","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#,
                        crate::mcp::MODERN_PROTOCOL_VERSION
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let result = body_json(response).await["result"].clone();
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["ttlMs"], 0);
        assert_eq!(result["cacheScope"], "private");
        assert!(result["tools"].is_array());
    }

    #[tokio::test]
    async fn authenticated_notifications_are_accepted() {
        let response = router(runtime())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("cf-access-jwt-assertion", "test-token")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }
    fn runtime_with_root(
        root: std::path::PathBuf,
    ) -> (
        Runtime,
        Arc<SessionSupervisor>,
        crate::approvals::ApprovalReceiver,
    ) {
        let roots =
            crate::named_roots::NamedRoots::from_canonical_roots(std::collections::BTreeMap::from(
                [("src".to_owned(), std::fs::canonicalize(root).unwrap())],
            ))
            .unwrap();
        let (supervisor, approvals) = SessionSupervisor::new(roots);
        let runtime = Runtime {
            authenticator: AuthProvider::Cloudflare(AccessAuthenticator::test(
                "test-token",
                AccessIdentity {
                    email: "test@example.com".to_owned(),
                    subject: "test-subject".to_owned(),
                },
            )),
            supervisor: Arc::clone(&supervisor),
        };
        (runtime, supervisor, approvals)
    }

    async fn call_public_tool(runtime: &Runtime, name: &str, arguments: Value) -> Value {
        let response = router(runtime.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("cf-access-jwt-assertion", "test-token")
                    .body(Body::from(
                        json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": "tools/call",
                            "params": {"name": name, "arguments": arguments}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await
    }

    fn tool_text(response: &Value) -> &str {
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool result text")
    }

    #[tokio::test]
    async fn public_managed_session_lifecycle_uses_named_root_and_existing_tools() {
        let fixture = tempfile::tempdir().unwrap();
        let volume = fixture.path().join("volume");
        std::fs::create_dir_all(volume.join("repo-a")).unwrap();
        std::fs::create_dir_all(volume.join("repo-b")).unwrap();
        std::fs::write(volume.join("repo-a/note.txt"), "hello managed session\n").unwrap();
        let (runtime, supervisor, _approvals) = runtime_with_root(volume.clone());
        let first_id = format!("http-a-{}", uuid::Uuid::new_v4());
        let second_id = format!("http-b-{}", uuid::Uuid::new_v4());

        let first = call_public_tool(
            &runtime,
            "session_start",
            json!({"path": "src/repo-a", "session_id": first_id}),
        )
        .await;
        let first_info: Value = serde_json::from_str(tool_text(&first)).unwrap();
        assert_eq!(first_info["status"], "active");
        assert_eq!(first_info["yolo"], false);
        assert_eq!(
            first_info["cwd"],
            std::fs::canonicalize(volume.join("repo-a"))
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );

        let second = call_public_tool(
            &runtime,
            "session_start",
            json!({"path": "src/repo-b", "session_id": second_id}),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(tool_text(&second)).unwrap()["status"],
            "active"
        );

        let listed = call_public_tool(&runtime, "session_list", json!({})).await;
        let sessions: Vec<Value> = serde_json::from_str(tool_text(&listed)).unwrap();
        assert!(
            sessions
                .iter()
                .any(|session| session["session_id"] == first_id)
        );
        assert!(
            sessions
                .iter()
                .any(|session| session["session_id"] == second_id)
        );

        let info =
            call_public_tool(&runtime, "session_info", json!({"session_id": first_id})).await;
        let info: Value = serde_json::from_str(tool_text(&info)).unwrap();
        assert_eq!(info["yolo"], false);

        let read = call_public_tool(
            &runtime,
            "read_file",
            json!({"session_id": first_id, "path": "note.txt"}),
        )
        .await;
        assert_eq!(tool_text(&read), "hello managed session\n");

        let executed = call_public_tool(
            &runtime,
            "execute",
            json!({"session_id": first_id, "command": ["/bin/pwd"]}),
        )
        .await;
        let executed: Value = serde_json::from_str(tool_text(&executed)).unwrap();
        assert_eq!(executed["exit_code"], 0);
        assert_eq!(
            executed["stdout"].as_str().unwrap().trim(),
            std::fs::canonicalize(volume.join("repo-a"))
                .unwrap()
                .to_string_lossy()
        );

        let root_id = format!("http-root-{}", uuid::Uuid::new_v4());
        let root_session = call_public_tool(
            &runtime,
            "session_start",
            json!({"path": "src", "session_id": root_id}),
        )
        .await;
        let root_info: Value = serde_json::from_str(tool_text(&root_session)).unwrap();
        assert_eq!(
            root_info["cwd"],
            std::fs::canonicalize(&volume)
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
        let _ = call_public_tool(&runtime, "session_stop", json!({"session_id": root_id})).await;

        let duplicate = call_public_tool(
            &runtime,
            "session_start",
            json!({"path": "src/repo-a", "session_id": first_id}),
        )
        .await;
        assert!(
            duplicate["error"]["message"]
                .as_str()
                .unwrap()
                .contains("already")
        );

        let stopped =
            call_public_tool(&runtime, "session_stop", json!({"session_id": first_id})).await;
        assert_eq!(
            serde_json::from_str::<Value>(tool_text(&stopped)).unwrap()["status"],
            "stopped"
        );
        supervisor.shutdown().await.unwrap();
        assert!(!crate::config::session_is_active(&second_id).await.unwrap());
        assert!(!crate::config::socket_path(&second_id).unwrap().exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn public_session_start_rejects_path_escape_yolo_and_missing_roots() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let volume = fixture.path().join("volume");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(volume.join("repo-a")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, volume.join("outside-link")).unwrap();
        let (configured_runtime, supervisor, _approvals) = runtime_with_root(volume);

        for (path, needle) in [
            (outside.to_string_lossy().to_string(), "absolute"),
            ("unknown/repo-a".to_owned(), "unknown named root"),
            ("src/../outside".to_owned(), "escapes named root"),
            ("src/outside-link".to_owned(), "escapes named root"),
            ("src/missing".to_owned(), "cannot resolve session path"),
        ] {
            let response = call_public_tool(
                &configured_runtime,
                "session_start",
                json!({"path": path, "session_id": format!("reject-{}", uuid::Uuid::new_v4())}),
            )
            .await;
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(needle),
                "{response}"
            );
        }

        let yolo = call_public_tool(
            &configured_runtime,
            "session_start",
            json!({"path": "src/repo-a", "session_id": "reject-yolo", "yolo": true}),
        )
        .await;
        assert!(
            yolo["error"]["message"]
                .as_str()
                .unwrap()
                .contains("only path and session_id")
        );
        supervisor.shutdown().await.unwrap();

        let no_roots = runtime();
        let response = call_public_tool(
            &no_roots,
            "session_start",
            json!({"path": "src/repo-a", "session_id": "reject-no-roots"}),
        )
        .await;
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not configured")
        );
    }

    #[tokio::test]
    async fn public_session_stop_cannot_stop_unmanaged_cli_runtime() {
        let runtime = runtime();
        let cwd = tempfile::tempdir().unwrap();
        let id = format!("cli-owned-{}", uuid::Uuid::new_v4());
        let (sender, _approvals) = crate::approvals::approval_channel();
        let handle = crate::approvals::spawn_runtime(cwd.path(), Some(&id), false, sender)
            .await
            .unwrap();

        let response = call_public_tool(&runtime, "session_stop", json!({"session_id": id})).await;
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not managed")
        );
        assert!(crate::config::session_is_active(&id).await.unwrap());
        handle.shutdown().await.unwrap();
    }
}
