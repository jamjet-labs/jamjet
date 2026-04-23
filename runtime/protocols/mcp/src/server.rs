//! MCP server — exposes agent tools and resources to external MCP clients.
//!
//! Implements the MCP HTTP+SSE transport (Streamable HTTP):
//!   POST /mcp  — JSON-RPC endpoint (initialize, tools/list, tools/call,
//!                resources/list, resources/read, prompts/list)
//!   GET  /mcp/sse — SSE stream for server-initiated messages
//!
//! Tools are registered via `ToolHandler` callbacks. The server
//! handles protocol negotiation (initialize/initialized) and routes
//! `tools/call` to the matching handler.

use crate::types::{McpContent, McpResource, McpTool, McpToolCallResponse};
use axum::{
    extract::State,
    http::StatusCode,
    response::{sse::Event as SseEvent, sse::Sse, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{collections::HashMap, convert::Infallible, sync::Arc};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::{debug, info, warn};

// ── Tool handler ──────────────────────────────────────────────────────────────

/// A registered tool with its definition and async handler.
pub struct RegisteredTool {
    pub definition: McpTool,
    pub handler: Box<dyn ToolFn>,
}

pub trait ToolFn: Send + Sync {
    fn call(
        &self,
        arguments: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<McpContent>, String>> + Send>>;
}

impl<F, Fut> ToolFn for F
where
    F: Fn(Value) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<Vec<McpContent>, String>> + Send + 'static,
{
    fn call(
        &self,
        arguments: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<McpContent>, String>> + Send>>
    {
        Box::pin(self(arguments))
    }
}

// ── Server state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct McpServerState {
    server_name: String,
    server_version: String,
    tools: Arc<HashMap<String, Arc<RegisteredTool>>>,
    resources: Arc<Vec<McpResource>>,
    sse_tx: broadcast::Sender<Value>,
}

// ── Routes ────────────────────────────────────────────────────────────────────

fn build_router(state: McpServerState) -> Router {
    Router::new()
        .route("/mcp", post(rpc_handler))
        .route("/mcp/sse", get(sse_handler))
        .with_state(state)
}

async fn rpc_handler(
    State(state): State<McpServerState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let method = body.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let rpc_id = body.get("id").cloned().unwrap_or(json!(1));
    let params = body.get("params").cloned().unwrap_or(json!({}));

    debug!(method = %method, "MCP RPC request");

    let result = match method {
        "initialize" => {
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": { "listChanged": false },
                    "resources": { "subscribe": false, "listChanged": false },
                    "prompts": { "listChanged": false }
                },
                "serverInfo": {
                    "name": state.server_name,
                    "version": state.server_version
                }
            })
        }

        "initialized" | "notifications/initialized" => json!({}),

        "tools/list" => {
            let tools: Vec<Value> = state
                .tools
                .values()
                .map(|t| {
                    json!({
                        "name": t.definition.name,
                        "description": t.definition.description,
                        "inputSchema": t.definition.input_schema,
                    })
                })
                .collect();
            json!({ "tools": tools })
        }

        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));

            match state.tools.get(name) {
                Some(tool) => match tool.handler.call(args).await {
                    Ok(content) => {
                        let resp = McpToolCallResponse {
                            content,
                            is_error: Some(false),
                        };
                        serde_json::to_value(resp).unwrap_or(json!({}))
                    }
                    Err(e) => {
                        warn!(tool = %name, error = %e, "Tool call failed");
                        let resp = McpToolCallResponse {
                            content: vec![McpContent::Text { text: e }],
                            is_error: Some(true),
                        };
                        serde_json::to_value(resp).unwrap_or(json!({}))
                    }
                },
                None => {
                    return (
                        StatusCode::OK,
                        Json(json!({
                            "jsonrpc": "2.0",
                            "id": rpc_id,
                            "error": {
                                "code": -32601,
                                "message": format!("tool not found: {name}")
                            }
                        })),
                    )
                        .into_response();
                }
            }
        }

        "resources/list" => {
            let resources: Vec<Value> = state
                .resources
                .iter()
                .map(|r| {
                    json!({
                        "uri": r.uri,
                        "name": r.name,
                        "description": r.description,
                        "mimeType": r.mime_type,
                    })
                })
                .collect();
            json!({ "resources": resources })
        }

        "prompts/list" => json!({ "prompts": [] }),

        "ping" => json!({}),

        // MCP notifications are fire-and-forget — accept silently.
        other if other.starts_with("notifications/") => json!({}),

        other => {
            warn!(method = %other, "Unknown MCP method");
            return (
                StatusCode::OK,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "error": {
                        "code": -32601,
                        "message": format!("method not found: {other}")
                    }
                })),
            )
                .into_response();
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": result,
        })),
    )
        .into_response()
}

async fn sse_handler(State(state): State<McpServerState>) -> impl IntoResponse {
    let rx = state.sse_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result: Result<Value, _>| {
        result.ok().and_then(|event| {
            serde_json::to_string(&event)
                .ok()
                .map(|data| Ok::<_, Infallible>(SseEvent::default().data(data)))
        })
    });
    Sse::new(stream)
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Configuration for the MCP server.
pub struct McpServerConfig {
    pub port: u16,
    pub server_name: String,
    pub server_version: String,
    pub exposed_tools: Vec<String>,
    pub exposed_resources: Vec<String>,
}

/// An MCP server that exposes agent capabilities to external MCP clients.
pub struct McpServer {
    port: u16,
    server_name: String,
    server_version: String,
    tools: HashMap<String, Arc<RegisteredTool>>,
    resources: Vec<McpResource>,
}

impl McpServer {
    pub fn new(name: impl Into<String>, version: impl Into<String>, port: u16) -> Self {
        Self {
            port,
            server_name: name.into(),
            server_version: version.into(),
            tools: HashMap::new(),
            resources: Vec::new(),
        }
    }

    /// Register a tool with an async handler function.
    pub fn register_tool<F, Fut>(mut self, definition: McpTool, handler: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Vec<McpContent>, String>> + Send + 'static,
    {
        let name = definition.name.clone();
        self.tools.insert(
            name,
            Arc::new(RegisteredTool {
                definition,
                handler: Box::new(handler),
            }),
        );
        self
    }

    /// Add a resource to expose.
    pub fn register_resource(mut self, resource: McpResource) -> Self {
        self.resources.push(resource);
        self
    }

    /// Convert into an Axum `Router` suitable for merging into another app.
    ///
    /// Unlike [`start`], this does not bind a TCP listener — the caller mounts
    /// the returned router into their own server.
    pub fn into_router(self) -> Router {
        let (sse_tx, _) = broadcast::channel(64);
        let state = McpServerState {
            server_name: self.server_name,
            server_version: self.server_version,
            tools: Arc::new(self.tools),
            resources: Arc::new(self.resources),
            sse_tx,
        };
        build_router(state)
    }

    /// Start the MCP server (binds on `0.0.0.0:{port}`).
    pub async fn start(self) -> Result<(), String> {
        let (sse_tx, _) = broadcast::channel(64);
        let state = McpServerState {
            server_name: self.server_name.clone(),
            server_version: self.server_version.clone(),
            tools: Arc::new(self.tools),
            resources: Arc::new(self.resources),
            sse_tx,
        };

        let router = build_router(state);
        let addr = format!("0.0.0.0:{}", self.port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("failed to bind MCP server on {addr}: {e}"))?;

        info!(
            name = %self.server_name,
            addr = %addr,
            "MCP server listening"
        );

        axum::serve(listener, router)
            .await
            .map_err(|e| format!("MCP server error: {e}"))?;

        Ok(())
    }
}
