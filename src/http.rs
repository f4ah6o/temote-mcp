//! Streamable HTTP MCP endpoint for remote clients such as ChatGPT.
//!
//! Cloudflare Access terminates Managed OAuth at the edge. This origin never
//! implements its own public OAuth server; it accepts only requests carrying a
//! valid Cloudflare Access JWT assertion.

use std::net::SocketAddr;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use url::Url;

use crate::access::AccessAuthenticator;

#[derive(Clone)]
pub struct Runtime {
    pub authenticator: AccessAuthenticator,
}

pub async fn serve(
    addr: SocketAddr,
    public_url: String,
    authenticator: AccessAuthenticator,
) -> Result<()> {
    let runtime = Runtime { authenticator };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    eprintln!("local-mcp HTTP server listening on http://{addr}");
    eprintln!("MCP endpoint for remote clients: {public_url}/mcp");
    eprintln!("Authentication: Cloudflare Access Managed OAuth");
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
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(runtime)
}

pub fn normalize_public_url(value: &str) -> Result<String> {
    let parsed = Url::parse(value.trim()).context("LOCAL_MCP_PUBLIC_URL is invalid")?;
    anyhow::ensure!(
        parsed.scheme() == "https"
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.path().trim_matches('/').is_empty(),
        "LOCAL_MCP_PUBLIC_URL must be an HTTPS origin without a path"
    );
    anyhow::ensure!(
        parsed.host_str().is_some(),
        "LOCAL_MCP_PUBLIC_URL has no host"
    );
    Ok(parsed
        .origin()
        .ascii_serialization()
        .trim_end_matches('/')
        .to_owned())
}

async fn healthz() -> Response {
    Json(json!({"status": "ok", "service": "local-mcp"})).into_response()
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
        return unauthorized(error);
    }
    with_cors(StatusCode::OK)
}

async fn mcp_options() -> Response {
    with_cors(StatusCode::NO_CONTENT)
}

async fn mcp_post(headers: HeaderMap, State(runtime): State<Runtime>, body: Bytes) -> Response {
    if let Err(error) = runtime.authenticator.authenticate(&headers).await {
        return unauthorized(error);
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

    // Notifications and responses carry no id and expect no reply.
    let Some(id) = request.get("id").cloned() else {
        let response = with_cors(StatusCode::ACCEPTED);
        audit.finish(response.status(), started);
        return response;
    };
    let response_value = match crate::mcp::dispatch_public(&request).await {
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

fn unauthorized(error: anyhow::Error) -> Response {
    eprintln!("[auth] rejected HTTP request: {error:#}");
    let mut response = with_cors((
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "access_unauthorized",
            "error_description": "valid Cloudflare Access authentication is required"
        })),
    ));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
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
            "accept,authorization,content-type,mcp-protocol-version,mcp-session-id",
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
    use crate::access::AccessIdentity;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn runtime() -> Runtime {
        Runtime {
            authenticator: AccessAuthenticator::test(
                "test-token",
                AccessIdentity {
                    email: "test@example.com".to_owned(),
                    subject: "test-subject".to_owned(),
                },
            ),
        }
    }

    async fn body_json(response: Response) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
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
}
