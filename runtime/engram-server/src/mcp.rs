//! MCP stdio server — JSON-RPC 2.0 over stdin/stdout.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// ---------------------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

// ---------------------------------------------------------------------------
// MCP types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

// ---------------------------------------------------------------------------
// Tool handler type
// ---------------------------------------------------------------------------

pub type ToolHandler = Box<
    dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> + Send + Sync,
>;

// ---------------------------------------------------------------------------
// McpServer
// ---------------------------------------------------------------------------

pub struct McpServer {
    tools: Vec<McpToolDef>,
    handlers: HashMap<String, Arc<ToolHandler>>,
}

impl McpServer {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            handlers: HashMap::new(),
        }
    }

    /// Register a tool with its definition and async handler.
    pub fn tool<F, Fut>(mut self, def: McpToolDef, handler: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, String>> + Send + 'static,
    {
        let name = def.name.clone();
        self.tools.push(def);
        self.handlers.insert(
            name,
            Arc::new(Box::new(move |args| Box::pin(handler(args)))),
        );
        self
    }

    /// Run the stdio JSON-RPC loop.
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break; // EOF
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(e) => {
                    let err_resp = JsonRpcResponse {
                        jsonrpc: "2.0".into(),
                        id: None,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32700,
                            message: format!("parse error: {e}"),
                        }),
                    };
                    let mut out = serde_json::to_string(&err_resp)?;
                    out.push('\n');
                    stdout.write_all(out.as_bytes()).await?;
                    stdout.flush().await?;
                    continue;
                }
            };

            let response = self.handle_request(&request).await;
            let mut out = serde_json::to_string(&response)?;
            out.push('\n');
            stdout.write_all(out.as_bytes()).await?;
            stdout.flush().await?;
        }

        Ok(())
    }

    async fn handle_request(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        match req.method.as_str() {
            "initialize" => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id.clone(),
                result: Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "engram",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                })),
                error: None,
            },
            "notifications/initialized" => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id.clone(),
                result: Some(Value::Null),
                error: None,
            },
            "tools/list" => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id.clone(),
                result: Some(serde_json::json!({ "tools": self.tools })),
                error: None,
            },
            "tools/call" => {
                let params = req.params.as_ref().cloned().unwrap_or(Value::Null);
                let name = params["name"].as_str().unwrap_or("");
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));

                match self.handlers.get(name) {
                    Some(handler) => {
                        let result = handler(arguments).await;
                        match result {
                            Ok(text) => JsonRpcResponse {
                                jsonrpc: "2.0".into(),
                                id: req.id.clone(),
                                result: Some(serde_json::json!({
                                    "content": [{ "type": "text", "text": text }],
                                    "isError": false
                                })),
                                error: None,
                            },
                            Err(e) => JsonRpcResponse {
                                jsonrpc: "2.0".into(),
                                id: req.id.clone(),
                                result: Some(serde_json::json!({
                                    "content": [{ "type": "text", "text": e }],
                                    "isError": true
                                })),
                                error: None,
                            },
                        }
                    }
                    None => JsonRpcResponse {
                        jsonrpc: "2.0".into(),
                        id: req.id.clone(),
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32601,
                            message: format!("unknown tool: {name}"),
                        }),
                    },
                }
            }
            _ => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id.clone(),
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("method not found: {}", req.method),
                }),
            },
        }
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}
