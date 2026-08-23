use std::sync::Arc;

use axum::extract::{FromRef, State};
use axum::response::IntoResponse;
use axum::{Extension, Json, Router};
use serde_json::{Value, json};
use vynil_core::hbs::HandleBars;

use crate::auth::{AuthPrincipal, MiryadAuthState};
use crate::mcp::format::{RenderShape, render};
use crate::mcp::protocol::{JsonRpcRequest, JsonRpcResponse};
use crate::mcp::registry::{McpOp, McpToolRegistry};

const PROTOCOL_VERSION: &str = "2024-11-05";
const OPERATIONS: &[(&str, McpOp)] = &[
    ("_list", McpOp::List),
    ("_get", McpOp::Get),
    ("_create", McpOp::Create),
    ("_update", McpOp::Update),
    ("_delete", McpOp::Delete),
];

/// Monte `POST /mcp` — dispatch JSON-RPC 2.0 (`initialize`, `tools/list`, `tools/call`).
/// Réutilise `MiryadAuthState` (dual-auth, db) comme REST/GraphQL.
pub fn mcp_router<S>(registry: McpToolRegistry) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new()
        .route("/mcp", axum::routing::post(mcp_handler))
        .layer(Extension(Arc::new(registry)))
}

async fn mcp_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Extension(registry): Extension<Arc<McpToolRegistry>>,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    // Notifications (pas d'id) : pas de réponse, HTTP 202.
    if req.id.is_none() {
        return axum::http::StatusCode::ACCEPTED.into_response();
    }

    let response = match req.method.as_str() {
        "initialize" => handle_initialize(req.id),
        "tools/list" => handle_tools_list(req.id, &registry),
        "tools/call" => handle_tools_call(req.id, req.params, &registry, &auth, &principal).await,
        other => JsonRpcResponse::err(req.id, -32601, format!("Method not found: {other}")),
    };

    Json(response).into_response()
}

fn handle_initialize(id: Option<Value>) -> JsonRpcResponse {
    JsonRpcResponse::ok(
        id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
            }
        }),
    )
}

fn handle_tools_list(id: Option<Value>, registry: &McpToolRegistry) -> JsonRpcResponse {
    let id_schema = json!({
        "type": "object",
        "properties": { "id": {"type": "integer"} },
        "required": ["id"],
    });

    let mut tools = Vec::new();
    for entity in registry.entities.values() {
        let name = entity.resource_name();
        tools.push(json!({
            "name": format!("{name}_list"),
            "description": format!("List {name}, paginated"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page": {"type": "integer"},
                    "per_page": {"type": "integer"},
                    "filter": {"type": "string"},
                },
            },
        }));
        tools.push(json!({
            "name": format!("{name}_get"),
            "description": format!("Get a single {name} by id"),
            "inputSchema": id_schema,
        }));
        tools.push(json!({
            "name": format!("{name}_create"),
            "description": format!("Create a new {name}"),
            "inputSchema": { "type": "object" },
        }));
        tools.push(json!({
            "name": format!("{name}_update"),
            "description": format!("Update an existing {name} by id"),
            "inputSchema": { "type": "object", "properties": { "id": {"type": "integer"} }, "required": ["id"] },
        }));
        tools.push(json!({
            "name": format!("{name}_delete"),
            "description": format!("Delete a {name} by id"),
            "inputSchema": id_schema,
        }));
    }

    JsonRpcResponse::ok(id, json!({ "tools": tools }))
}

#[derive(serde::Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

fn parse_tool_name(name: &str) -> Option<(&str, McpOp)> {
    OPERATIONS
        .iter()
        .find_map(|(suffix, op)| name.strip_suffix(suffix).map(|resource| (resource, *op)))
}

async fn handle_tools_call(
    id: Option<Value>,
    params: Value,
    registry: &McpToolRegistry,
    auth: &MiryadAuthState,
    principal: &AuthPrincipal,
) -> JsonRpcResponse {
    let call: ToolCallParams = match serde_json::from_value(params) {
        Ok(call) => call,
        Err(e) => return JsonRpcResponse::err(id, -32602, format!("Invalid params: {e}")),
    };

    let Some((resource_name, op)) = parse_tool_name(&call.name) else {
        return JsonRpcResponse::err(id, -32601, format!("Unknown tool: {}", call.name));
    };
    let Some(entity) = registry.entities.get(resource_name) else {
        return JsonRpcResponse::err(id, -32601, format!("Unknown tool: {}", call.name));
    };

    match entity.call(op, &auth.db, principal, call.arguments).await {
        Ok(data) => {
            let shape = if op == McpOp::List {
                RenderShape::List
            } else {
                RenderShape::Record
            };
            let mut engine = HandleBars::new();
            match render(&mut engine, &registry.format, shape, &data) {
                Ok(text) => JsonRpcResponse::ok(id, json!({ "content": [{ "type": "text", "text": text }] })),
                Err(e) => JsonRpcResponse::err_with_data(id, e.rpc_code(), e.to_string(), e.data()),
            }
        }
        Err(e) => JsonRpcResponse::err_with_data(id, e.rpc_code(), e.to_string(), e.data()),
    }
}
