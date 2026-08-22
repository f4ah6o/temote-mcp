//! Streamable HTTP MCP endpoint for remote clients such as ChatGPT.
//!
//! Cloudflare Access terminates Managed OAuth at the edge. This origin never
//! implements its own public OAuth server; it accepts only requests carrying a
//! valid Cloudflare Access JWT assertion.

use std::net::SocketAddr;
use std::sync::Arc;
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
use crate::supervisor::SessionSupervisor;

#[derive(Clone)]
pub struct Runtime {
    pub authenticator: AccessAuthenticator,
    pub supervisor: Arc<SessionSupervisor>,
}

pub async fn serve(
    addr: SocketAddr,
    public_url: String,
    authenticator: AccessAuthenticator,
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
    eprintln!("Authentication: Cloudflare Access Managed OAuth");
    axum::serve(listener, router(runtime))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
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
    let parsed = Url::parse(value.trim()).context("TEMOTE_MCP_PUBLIC_URL is invalid")?;
    anyhow::ensure!(
        parsed.scheme() == "https"
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.path().trim_matches('/').is_empty(),
        "TEMOTE_MCP_PUBLIC_URL must be an HTTPS origin without a path"
    );
    anyhow::ensure!(
        parsed.host_str().is_some(),
        "TEMOTE_MCP_PUBLIC_URL has no host"
    );
    Ok(parsed
        .origin()
        .ascii_serialization()
        .trim_end_matches('/')
        .to_owned())
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
    use crate::access::AccessIdentity;
    use crate::test_support;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn runtime() -> Runtime {
        let (supervisor, _approvals) =
            SessionSupervisor::new(crate::named_roots::NamedRoots::default());
        Runtime {
            authenticator: AccessAuthenticator::test(
                "test-token",
                AccessIdentity {
                    email: "test@example.com".to_owned(),
                    subject: "test-subject".to_owned(),
                },
            ),
            supervisor,
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
            authenticator: AccessAuthenticator::test(
                "test-token",
                AccessIdentity {
                    email: "test@example.com".to_owned(),
                    subject: "test-subject".to_owned(),
                },
            ),
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
