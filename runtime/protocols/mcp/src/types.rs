//! MCP JSON-RPC message types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An MCP tool definition returned by tools/list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// Request to invoke a tool (tools/call).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallRequest {
    pub name: String,
    pub arguments: Value,
}

/// Response from a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallResponse {
    pub content: Vec<McpContent>,
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        mime_type: String,
    },
    Resource {
        uri: String,
        mime_type: Option<String>,
        text: Option<String>,
    },
}

/// An MCP resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

/// JSON-RPC request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Value::Number(id.into()),
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}
