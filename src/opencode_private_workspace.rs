//! Per-task, loopback-only MCP tool for a Host-opted-in managed OpenCode task.
//! The tool has no caller-supplied command, path, environment, or task text.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{config, observation, opencode_server, orchestration};

const TOOL: &str = "workspace_check";
const MAX_REPLY: usize = 4096;

#[derive(Clone)]
pub(crate) struct Authority {
    pub session: config::Session,
    pub parent_task_id: Uuid,
    pub workspace_id: Uuid,
    pub model: String,
    pub effort: String,
    pub producer_epoch: u64,
    pub runtime_instance_id: Uuid,
}

struct StateData {
    token: String,
    authority: Authority,
    catalog_observed: Arc<AtomicBool>,
}

pub(crate) struct Bridge {
    pub port: u16,
    pub token: String,
    pub runtime_instance_id: Uuid,
    catalog_observed: Arc<AtomicBool>,
    server: tokio::task::JoinHandle<()>,
}

impl Bridge {
    pub(crate) fn catalog_observed(&self) -> bool {
        self.catalog_observed.load(Ordering::Acquire)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub(crate) fn start(authority: Authority) -> Result<Arc<Bridge>> {
    let runtime_instance_id = authority.runtime_instance_id;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("cannot bind private workspace bridge")?;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let port = listener.local_addr()?.port();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let catalog_observed = Arc::new(AtomicBool::new(false));
    let state = Arc::new(StateData {
        token: token.clone(),
        authority,
        catalog_observed: Arc::clone(&catalog_observed),
    });
    let app = Router::new()
        .route("/mcp", post(request))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(state);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(Arc::new(Bridge {
        port,
        token,
        runtime_instance_id,
        catalog_observed,
        server,
    }))
}

pub(crate) fn configure(config: &mut Value, bridge: &Bridge) {
    config["mcp"] = json!({
        "servers": {"temote_workspace": {
            "type": "remote",
            "url": format!("http://127.0.0.1:{}/mcp", bridge.port),
            "disabled": false,
            "codemode": false,
            "headers": {"Authorization": format!("Bearer {}", bridge.token)},
            "oauth": false,
            "timeout": {"startup":30000,"catalog":30000,"execution":120000}
        }}
    });
}

async fn request(
    State(state): State<Arc<StateData>>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let expected = format!("Bearer {}", state.token);
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(expected.as_str()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        );
    }
    let id = input.get("id").cloned().unwrap_or(Value::Null);
    let method = input.get("method").and_then(Value::as_str).unwrap_or("");
    if method == "notifications/initialized" {
        return (StatusCode::ACCEPTED, Json(Value::Null));
    }
    if method == "tools/list" {
        state.catalog_observed.store(true, Ordering::Release);
    }
    let response = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "temote-private-workspace", "version": "1"}
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": [{
            "name": TOOL,
            "description": "Run a fixed workspace inspection or package check through the Host's sandboxed Codex task. Supply a stable operation_id for retries; status also needs target.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type":"string", "enum":["inspect","build","test","lint","status"]},
                    "operation_id": {"type":"string", "format":"uuid"},
                    "target": {"type":"string", "enum":["inspect","build","test","lint"]}
                },
                "required": ["action","operation_id"],
                "additionalProperties": false
            }
        }]})),
        "tools/call" => call(&state, input.get("params").unwrap_or(&Value::Null)).await,
        _ => Err(anyhow::anyhow!("unknown MCP method")),
    };
    match response {
        Ok(result) => (
            StatusCode::OK,
            Json(json!({"jsonrpc":"2.0","id":id,"result":result})),
        ),
        Err(_) => (
            StatusCode::OK,
            Json(json!({"jsonrpc":"2.0","id":id,
            "error":{"code":-32602,"message":"private workspace request rejected"}})),
        ),
    }
}

async fn call(state: &StateData, params: &Value) -> Result<Value> {
    anyhow::ensure!(params.get("name").and_then(Value::as_str) == Some(TOOL));
    let parsed = crate::opencode_workspace::parse_check_request(
        params.get("arguments").context("arguments missing")?,
    )
    .context("invalid workspace check request")?;
    let operation_id = parsed.operation_id;
    let target = parsed.action.name();
    let task = parsed.action.task();
    opencode_server::validate_private_workspace_bridge(&state.authority).await?;
    let task_id = opencode_server::private_check_receipt(
        &state.authority,
        operation_id,
        target,
        !parsed.status,
    )?;
    let origin = crate::codex_app_server::TaskStartOrigin::opencode_workspace_check(
        state.authority.parent_task_id,
        state.authority.workspace_id,
        target,
        state.authority.producer_epoch,
    );
    let actor = observation::ActorRef {
        transport: "opencode-private-workspace".to_owned(),
        principal: None,
    };
    let request = json!({"operation_id":operation_id,"task":task,
        "model":state.authority.model,"effort":state.authority.effort});
    let view = if parsed.status {
        let Some(receipt) = crate::codex_app_server::task_start_receipt_if_retained(
            &request,
            &state.authority.session,
            &origin,
        )?
        else {
            anyhow::bail!("private check is not retained in Codex");
        };
        anyhow::ensure!(
            receipt.get("task_id") == Some(&json!(task_id)),
            "receipt task mismatch"
        );
        Box::pin(orchestration::invoke(
            orchestration::Backend::Codex,
            orchestration::Operation::TaskGet,
            &json!({"task_id":task_id}),
            &state.authority.session,
            &actor,
            None,
        ))
        .await?
    } else {
        // A previously started OpenCode parent cannot lend its bridge to a
        // workspace subsequently reserved for a Change writer.
        crate::change_cli::authorize_bound_start(&state.authority.session).await?;
        orchestration::invoke_codex_task_start_with_admission(
            &request,
            &state.authority.session,
            &origin,
            &actor,
            None,
            || {
                crate::change_cli::authorize_unbound_start(&state.authority.session)?;
                opencode_server::validate_private_workspace_bridge_sync(&state.authority)
            },
        )
        .await?
    };
    anyhow::ensure!(
        view.get("task_id") == Some(&json!(task_id)),
        "private task ID mismatch"
    );
    let mut projection = json!({
        "operation_id": operation_id,
        "target": target,
        "task_id": view.get("task_id"),
        "status": view.get("status"),
        "revision": view.get("revision"),
        "evidence": view.get("evidence")
    });
    if serde_json::to_vec(&projection)?.len() > MAX_REPLY {
        projection["evidence"] = Value::Null;
    }
    Ok(json!({"content":[{"type":"text","text":serde_json::to_string(&projection)?}]}))
}
