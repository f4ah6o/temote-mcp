//! HTTP MCP endpoint with OAuth, for remote clients such as ChatGPT.
//!
//! The routing and the shape of the OAuth responses follow `shuttle-rs`
//! (`src/app.rs`, MIT OR Apache-2.0). Tool handling is shared with the stdio
//! server through [`crate::mcp::dispatch`].

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::oauth::{self, OAuthConfig, OAuthStore};

#[derive(Clone)]
pub struct Runtime {
    pub config: Arc<OAuthConfig>,
    pub store: OAuthStore,
}

pub async fn serve(addr: SocketAddr, config: OAuthConfig, store: OAuthStore) -> Result<()> {
    let public_url = config.public_url.clone();
    let runtime = Runtime {
        config: Arc::new(config),
        store,
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    eprintln!("local-mcp HTTP server listening on http://{addr}");
    eprintln!("MCP endpoint for remote clients: {public_url}/mcp");
    axum::serve(listener, router(runtime)).await?;
    Ok(())
}

pub fn router(runtime: Runtime) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/mcp",
            get(mcp_get)
                .post(mcp_post)
                .delete(mcp_delete)
                .options(mcp_options),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route(
            "/.well-known/oauth-authorization-server/mcp",
            get(authorization_server),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize_page).post(authorize_submit))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
        .with_state(runtime)
}

async fn healthz() -> Response {
    Json(json!({"status": "ok", "service": "local-mcp"})).into_response()
}

/// The server answers every request on a single POST, so it never opens the
/// optional server-to-client stream (MCP Streamable HTTP).
async fn mcp_get() -> Response {
    with_cors((StatusCode::METHOD_NOT_ALLOWED, "SSE streams are not supported"))
}

async fn mcp_delete(headers: HeaderMap, State(runtime): State<Runtime>) -> Response {
    if let Some(response) = unauthorized(&runtime, &headers).await {
        return response;
    }
    with_cors(StatusCode::OK)
}

async fn mcp_options() -> Response {
    with_cors(StatusCode::NO_CONTENT)
}

async fn mcp_post(
    headers: HeaderMap,
    State(runtime): State<Runtime>,
    body: Bytes,
) -> Response {
    if let Some(response) = unauthorized(&runtime, &headers).await {
        return response;
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
    // Notifications and responses carry no id and expect no reply.
    let Some(id) = request.get("id").cloned() else {
        return with_cors(StatusCode::ACCEPTED);
    };
    let response = match crate::mcp::dispatch(&request).await {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": format!("{error:#}")}
        }),
    };
    with_cors(Json(response))
}

async fn protected_resource(State(runtime): State<Runtime>) -> Response {
    with_cors(Json(oauth::protected_resource_metadata(&runtime.config)))
}

async fn authorization_server(State(runtime): State<Runtime>) -> Response {
    with_cors(Json(oauth::authorization_server_metadata(&runtime.config)))
}

async fn register(
    State(runtime): State<Runtime>,
    Json(request): Json<oauth::RegisterRequest>,
) -> Response {
    match runtime
        .store
        .register_client(request, &runtime.config)
        .await
    {
        Ok(client) => with_cors((
            StatusCode::CREATED,
            Json(json!({
                "client_id": client.client_id,
                "redirect_uris": client.redirect_uris,
                "client_name": client.client_name,
                "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
            })),
        )),
        Err(error) => {
            eprintln!("[oauth] client registration rejected: {error:#}");
            oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_redirect_uri",
                "client registration request is invalid",
            )
        }
    }
}

async fn authorize_page(
    State(runtime): State<Runtime>,
    Query(request): Query<oauth::AuthorizeRequest>,
) -> Response {
    if request.response_type != "code" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_response_type",
            "response_type must be code",
        );
    }
    if !runtime
        .store
        .client_allows_redirect(&request.client_id, &request.redirect_uri)
        .await
    {
        if request.client_id.starts_with("https://") {
            eprintln!(
                "[oauth] client_id {} looks like a client ID metadata document; this server only supports dynamic client registration",
                request.client_id
            );
        }
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "unknown client_id or redirect_uri",
        );
    }
    Html(authorize_html(&request)).into_response()
}

async fn authorize_submit(
    State(runtime): State<Runtime>,
    Form(form): Form<oauth::AuthorizeForm>,
) -> Response {
    if !constant_time_eq(
        form.admin_token.as_bytes(),
        runtime.config.admin_token.as_bytes(),
    ) {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "access_denied",
            "invalid admin token",
        );
    }
    let request = oauth::AuthorizeRequest::from(form);
    match runtime.store.create_code(request.clone()).await {
        Ok(code) => Redirect::to(&oauth::authorize_redirect(
            &request.redirect_uri,
            &code,
            request.state.as_deref(),
        ))
        .into_response(),
        Err(error) => {
            eprintln!("[oauth] authorization rejected: {error:#}");
            oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "authorization request is invalid",
            )
        }
    }
}

async fn token(
    State(runtime): State<Runtime>,
    Form(request): Form<oauth::TokenRequest>,
) -> Response {
    let result = match request.grant_type.as_str() {
        "authorization_code" => runtime.store.exchange_code(request).await,
        "refresh_token" => runtime.store.refresh(request).await,
        _ => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "grant_type must be authorization_code or refresh_token",
            );
        }
    };
    match result {
        Ok(token) => Json(token).into_response(),
        Err(error) => {
            eprintln!("[oauth] token request rejected: {error:#}");
            oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the grant is invalid, expired, or already used",
            )
        }
    }
}

async fn revoke(
    State(runtime): State<Runtime>,
    Form(request): Form<oauth::RevokeRequest>,
) -> Response {
    if request.token.trim().is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "token is required",
        );
    }
    // RFC 7009 deliberately returns success for unknown tokens so this endpoint
    // cannot be used to enumerate credentials.
    match runtime.store.revoke_token(&request.token).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(error) => {
            eprintln!("[oauth] revocation failed: {error:#}");
            oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "revocation failed",
            )
        }
    }
}

/// Returns the 401 response to send when the request may not touch `/mcp`.
async fn unauthorized(runtime: &Runtime, headers: &HeaderMap) -> Option<Response> {
    match bearer_token(headers) {
        Some(token) if runtime.store.validate_access_token(token).await => None,
        _ => Some(unauthorized_response(&runtime.config)),
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("Bearer").then_some(token.trim())
        })
}

fn unauthorized_response(config: &OAuthConfig) -> Response {
    let mut response = with_cors((
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "unauthorized",
            "error_description": "authentication required"
        })),
    ));
    let header_value = format!(
        r#"Bearer resource_metadata="{}/.well-known/oauth-protected-resource/mcp", scope="mcp""#,
        config.public_url
    );
    if let Ok(value) = HeaderValue::from_str(&header_value) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

fn oauth_error(status: StatusCode, code: &str, description: &str) -> Response {
    (
        status,
        Json(json!({"error": code, "error_description": description})),
    )
        .into_response()
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
            "accept,authorization,content-type,mcp-protocol-version,mcp-session-id",
        ),
    );
    parts.headers.insert(
        "access-control-expose-headers",
        HeaderValue::from_static("mcp-session-id"),
    );
    Response::from_parts(parts, body)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left = *left.get(index).unwrap_or(&0);
        let right = *right.get(index).unwrap_or(&0);
        diff |= (left ^ right) as usize;
    }
    diff == 0
}

fn authorize_html(request: &oauth::AuthorizeRequest) -> String {
    format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Authorize local-mcp</title>
  <style>
    body {{ font-family: system-ui, sans-serif; margin: 2rem; color: #1f2937; }}
    form {{ display: grid; gap: 1rem; max-width: 32rem; }}
    input, button {{ font: inherit; padding: .6rem; }}
    label {{ display: grid; gap: .35rem; }}
    p.warning {{ max-width: 32rem; color: #b91c1c; }}
  </style>
</head>
<body>
  <h1>Authorize local-mcp</h1>
  <p>{client_id} is requesting access to this machine through local-mcp.</p>
  <p class="warning">Approving grants file access and sandboxed command execution. Only continue if you started this connection yourself.</p>
  <form method="post" action="/oauth/authorize">
    <label>Admin token <input name="admin_token" type="password" autocomplete="current-password" required></label>
    <input type="hidden" name="response_type" value="{response_type}">
    <input type="hidden" name="client_id" value="{client_id}">
    <input type="hidden" name="redirect_uri" value="{redirect_uri}">
    <input type="hidden" name="state" value="{state}">
    <input type="hidden" name="scope" value="{scope}">
    <input type="hidden" name="code_challenge" value="{code_challenge}">
    <input type="hidden" name="code_challenge_method" value="{code_challenge_method}">
    <button type="submit">Authorize</button>
  </form>
</body>
</html>"#,
        response_type = html_escape(&request.response_type),
        client_id = html_escape(&request.client_id),
        redirect_uri = html_escape(&request.redirect_uri),
        state = html_escape(request.state.as_deref().unwrap_or("")),
        scope = html_escape(request.scope.as_deref().unwrap_or("mcp")),
        code_challenge = html_escape(request.code_challenge.as_deref().unwrap_or("")),
        code_challenge_method = html_escape(request.code_challenge_method.as_deref().unwrap_or("")),
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const VERIFIER: &str = "abc123abc123abc123abc123abc123abc123abc123abc123";
    const REDIRECT_URI: &str = "https://chatgpt.com/connector/oauth/test";

    async fn runtime(directory: &tempfile::TempDir) -> Runtime {
        Runtime {
            config: Arc::new(OAuthConfig {
                public_url: "https://localmcp.example.test".to_owned(),
                admin_token: "admin-token".to_owned(),
                allowed_redirect_prefixes: oauth::DEFAULT_REDIRECT_PREFIXES
                    .iter()
                    .map(|prefix| (*prefix).to_owned())
                    .collect(),
            }),
            store: OAuthStore::open(directory.path().join("oauth.json"))
                .await
                .unwrap(),
        }
    }

    async fn body_json(response: Response) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn pkce_challenge() -> String {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use sha2::{Digest, Sha256};
        URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()))
    }

    #[tokio::test]
    async fn unauthenticated_mcp_calls_point_at_the_resource_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let response = router(runtime(&directory).await)
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
        let challenge = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(challenge.contains(
            "https://localmcp.example.test/.well-known/oauth-protected-resource/mcp"
        ));
    }

    #[tokio::test]
    async fn protected_resource_metadata_is_public() {
        let directory = tempfile::tempdir().unwrap();
        let response = router(runtime(&directory).await)
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await["resource"],
            "https://localmcp.example.test/mcp"
        );
    }

    #[tokio::test]
    async fn full_authorization_code_flow_reaches_the_tool_list() {
        let directory = tempfile::tempdir().unwrap();
        let app = router(runtime(&directory).await);

        let registered = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"redirect_uris":["{REDIRECT_URI}"],"client_name":"test","client_uri":"https://chatgpt.com"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registered.status(), StatusCode::CREATED);
        let client_id = body_json(registered).await["client_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let challenge = pkce_challenge();
        let form = format!(
            "admin_token=admin-token&response_type=code&client_id={client_id}&redirect_uri={redirect}&scope=mcp&code_challenge={challenge}&code_challenge_method=S256",
            redirect = REDIRECT_URI.replace(':', "%3A").replace('/', "%2F"),
        );
        let authorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/authorize")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::SEE_OTHER);
        let location = authorized
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let code = location.split("code=").nth(1).unwrap().to_owned();

        let issued = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/token")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "grant_type=authorization_code&client_id={client_id}&redirect_uri={redirect}&code={code}&code_verifier={VERIFIER}&resource=https%3A%2F%2Flocalmcp.example.test%2Fmcp",
                        redirect = REDIRECT_URI.replace(':', "%3A").replace('/', "%2F"),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(issued.status(), StatusCode::OK);
        let tokens = body_json(issued).await;
        let access_token = tokens["access_token"].as_str().unwrap().to_owned();
        assert!(tokens["refresh_token"].as_str().is_some());

        let listed = app
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
        assert_eq!(listed.status(), StatusCode::OK);
        let tools = body_json(listed).await;
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn notifications_are_accepted_without_a_body() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let store = runtime.store.clone();
        let app = router(runtime);

        // Mint a token directly so the test focuses on notification handling.
        let client = store
            .register_client(
                oauth::RegisterRequest {
                    redirect_uris: vec![REDIRECT_URI.to_owned()],
                    client_name: None,
                },
                &OAuthConfig {
                    public_url: "https://localmcp.example.test".to_owned(),
                    admin_token: "admin-token".to_owned(),
                    allowed_redirect_prefixes: oauth::DEFAULT_REDIRECT_PREFIXES
                        .iter()
                        .map(|prefix| (*prefix).to_owned())
                        .collect(),
                },
            )
            .await
            .unwrap();
        let code = store
            .create_code(oauth::AuthorizeRequest {
                response_type: "code".to_owned(),
                client_id: client.client_id.clone(),
                redirect_uri: REDIRECT_URI.to_owned(),
                state: None,
                scope: None,
                code_challenge: Some(pkce_challenge()),
                code_challenge_method: Some("S256".to_owned()),
            })
            .await
            .unwrap();
        let tokens = store
            .exchange_code(oauth::TokenRequest {
                grant_type: "authorization_code".to_owned(),
                client_id: Some(client.client_id),
                redirect_uri: Some(REDIRECT_URI.to_owned()),
                code: Some(code),
                code_verifier: Some(VERIFIER.to_owned()),
                refresh_token: None,
            })
            .await
            .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", tokens.access_token))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }
}
