//! Stateless native Streamable HTTP MCP; no stdio or inference proxy.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::prompt_dump::TeamPromptDumpConfig;
use crate::scheduler::TeammateManager;
use crate::service::TeamSessionService;

use super::protocol::{INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, JsonRpcRequest, SERVER_NAME, SERVER_VERSION};

const HTTP_PROTOCOL_VERSION: &str = "2025-06-18";

pub(super) type SlotCredentials = Arc<Mutex<HashMap<String, String>>>;

#[derive(Clone)]
pub(super) struct HttpState {
    pub credentials: SlotCredentials,
    pub scheduler: Arc<TeammateManager>,
    pub service: Weak<TeamSessionService>,
    pub team_id: String,
    pub prompt_dump: TeamPromptDumpConfig,
    pub host: String,
}

pub(super) async fn serve(listener: TcpListener, state: HttpState, mut shutdown: watch::Receiver<bool>) {
    let router = Router::new()
        .route("/mcp", post(rpc))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state);
    let stopped = async move {
        while !*shutdown.borrow() {
            if shutdown.changed().await.is_err() {
                break;
            }
        }
    };
    if let Err(error) = axum::serve(listener, router).with_graceful_shutdown(stopped).await {
        tracing::warn!(%error, "native Team MCP HTTP server stopped with an I/O error");
    }
}

fn one_header<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    let mut values = headers.get_all(key).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn error(status: StatusCode, id: Value, code: i64, message: &str) -> Response {
    (
        status,
        Json(json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message}})),
    )
        .into_response()
}

async fn rpc(State(state): State<HttpState>, headers: HeaderMap, body: Bytes) -> Response {
    // No browser callers or DNS-rebinding hosts. The native descriptor always
    // uses this literal IPv4 loopback authority.
    if headers.contains_key("origin") || one_header(&headers, "host") != Some(state.host.as_str()) {
        return error(
            StatusCode::FORBIDDEN,
            Value::Null,
            INVALID_REQUEST,
            "Local MCP origin/host rejected",
        );
    }
    let slot = one_header(&headers, "x-slot-id");
    let bearer = one_header(&headers, "authorization").and_then(|value| value.strip_prefix("Bearer "));
    let authenticated = match (slot, bearer) {
        (Some(slot), Some(bearer)) => state
            .credentials
            .lock()
            .expect("MCP credentials poisoned")
            .get(slot)
            .is_some_and(|expected| expected == bearer),
        _ => false,
    };
    let Some(slot) = slot.filter(|_| authenticated) else {
        return error(
            StatusCode::UNAUTHORIZED,
            Value::Null,
            INVALID_REQUEST,
            "Authentication failed",
        );
    };
    if state.scheduler.get_agent(slot).await.is_err() {
        return error(
            StatusCode::UNAUTHORIZED,
            Value::Null,
            INVALID_REQUEST,
            "Authentication failed",
        );
    }
    if one_header(&headers, "content-type")
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        != Some("application/json")
    {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Value::Null,
            INVALID_REQUEST,
            "Expected application/json",
        );
    }
    if headers.contains_key("mcp-protocol-version")
        && !matches!(
            one_header(&headers, "mcp-protocol-version"),
            Some("2025-06-18" | "2025-03-26")
        )
    {
        return error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            INVALID_REQUEST,
            "Unsupported MCP protocol version",
        );
    }
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, Value::Null, -32700, "Parse error"),
    };
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    let method = value.get("method").and_then(Value::as_str);
    if value.get("id").is_some_and(|id| !id.is_string() && !id.is_number()) {
        return error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            INVALID_REQUEST,
            "Invalid MCP request id",
        );
    }
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_none() {
        return error(StatusCode::BAD_REQUEST, id, INVALID_REQUEST, "Invalid JSON-RPC request");
    }
    if value.get("params").is_some_and(|params| !params.is_object()) {
        return error(
            StatusCode::BAD_REQUEST,
            id,
            INVALID_PARAMS,
            "MCP params must be an object",
        );
    }
    let method = method.expect("checked");
    // MCP notifications have no JSON-RPC response, including initialized and
    // cancelled. Stateless requests do not require an MCP session-id header.
    if value.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    if method == "initialize" {
        let Some(version) = value.pointer("/params/protocolVersion").and_then(Value::as_str) else {
            return error(StatusCode::OK, id, INVALID_PARAMS, "protocolVersion is required");
        };
        // Official MCP schema/2025-06-18: unsupported client versions receive
        // a version implemented by this HTTP transport, not the legacy TCP one.
        let version = match version {
            "2025-06-18" | "2025-03-26" => version,
            _ => HTTP_PROTOCOL_VERSION,
        };
        tracing::info!(team_id = %state.team_id, slot_id = slot, "native Team MCP initialized");
        return Json(json!({"jsonrpc":"2.0","id":id,"result":{
            "protocolVersion":version,"capabilities":{"tools":{}},
            "serverInfo":{"name":SERVER_NAME,"version":SERVER_VERSION}
        }}))
        .into_response();
    }
    if method == "ping" {
        return Json(json!({"jsonrpc":"2.0","id":id,"result":{}})).into_response();
    }
    // Reuse the existing business dispatch/role checks. Its numeric internal
    // id is transport-local; native HTTP preserves the caller's string/number id.
    let request = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(0),
        method: method.into(),
        params: value.get("params").cloned(),
    };
    let mut response = super::server::handle_method(
        &request,
        &state.scheduler,
        &state.service,
        &state.team_id,
        slot,
        &state.prompt_dump,
    )
    .await;
    if method == "tools/list" && response.error.is_none() {
        // The legacy TCP transport exposes internal descriptors to its own
        // adapter. Native MCP clients require the official Tool wire shape.
        let tools = response
            .result
            .as_ref()
            .and_then(|result| result.get("tools"))
            .cloned()
            .and_then(|tools| serde_json::from_value::<Vec<super::tools::ToolDescriptor>>(tools).ok());
        let Some(tools) = tools else {
            tracing::error!(team_id = %state.team_id, slot_id = slot, "native Team MCP tool descriptor contract failed");
            return error(StatusCode::OK, id, INTERNAL_ERROR, "Tool schema unavailable");
        };
        response.result = Some(json!({"tools": super::server::tool_descriptors_to_mcp_json(&tools)}));
    }
    let mut response = serde_json::to_value(response).expect("JSON-RPC response serializable");
    response["id"] = id;
    if method == "tools/list" {
        tracing::info!(team_id = %state.team_id, slot_id = slot, "native Team MCP tools listed");
    }
    Json(response).into_response()
}
