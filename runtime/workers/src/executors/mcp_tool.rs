//! Executor for `McpTool` workflow nodes.
//!
//! When a workflow node has kind `McpTool`, this executor:
//! 1. Resolves the MCP server configuration from the workflow IR.
//! 2. Connects to the MCP server (stdio or HTTP).
//! 3. Sends the initialize handshake.
//! 4. Invokes the specified tool with mapped inputs.
//! 5. Returns the tool's output as the node result.
//!
//! Each invocation opens a fresh connection. For Phase 2, connection pooling
//! and persistent stdio processes can be added.

use crate::executor::{ExecutionResult, NodeExecutor};
use async_trait::async_trait;
use jamjet_ir::workflow::{McpServerConfig, McpTransport as IrMcpTransport};
use jamjet_mcp::{HttpSseTransport, McpClient, StdioTransport};
use jamjet_state::backend::WorkItem;
use serde_json::{json, Value};
use tracing::{debug, instrument};

/// Executor for `mcp_tool` nodes.
pub struct McpToolExecutor {
    /// Resolved MCP server configs from the workflow IR, keyed by server alias.
    servers: std::collections::HashMap<String, McpServerConfig>,
}

impl McpToolExecutor {
    pub fn new(servers: std::collections::HashMap<String, McpServerConfig>) -> Self {
        Self { servers }
    }
}

#[async_trait]
impl NodeExecutor for McpToolExecutor {
    #[instrument(skip(self, item), fields(node_id = %item.node_id))]
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, String> {
        let start = std::time::Instant::now();

        // Extract server alias and tool name from the payload.
        let server_alias = item
            .payload
            .get("server")
            .and_then(|v| v.as_str())
            .ok_or("McpTool: missing 'server' in payload")?;
        let tool_name = item
            .payload
            .get("tool")
            .and_then(|v| v.as_str())
            .ok_or("McpTool: missing 'tool' in payload")?;
        let arguments = item.payload.get("arguments").cloned().unwrap_or(json!({}));

        let server_config = self
            .servers
            .get(server_alias)
            .ok_or_else(|| format!("McpTool: no server config for alias '{server_alias}'"))?;

        debug!(server = %server_alias, tool = %tool_name, "Invoking MCP tool");

        // Open a protocol-level span for MCP call latency tracking (H2.4).
        let mcp_span = tracing::info_span!(
            "jamjet.mcp_call",
            "jamjet.tool.protocol" = "mcp",
            "jamjet.mcp.server" = %server_alias,
            "jamjet.tool.name" = %tool_name,
        );
        let _mcp_guard = mcp_span.enter();

        // Connect to the MCP server based on transport type.
        let client: McpClient = match &server_config.transport {
            IrMcpTransport::Stdio => {
                let command = server_config
                    .command
                    .as_deref()
                    .ok_or("McpTool: stdio transport requires 'command'")?;
                let arg_strs: Vec<&str> = server_config.args.iter().map(|s| s.as_str()).collect();
                let transport = StdioTransport::spawn(command, &arg_strs).await?;
                McpClient::new(server_alias.to_string(), Box::new(transport))
            }
            IrMcpTransport::HttpSse | IrMcpTransport::WebSocket => {
                let url = server_config
                    .url
                    .as_deref()
                    .ok_or("McpTool: HTTP transport requires 'url'")?;
                let transport = HttpSseTransport::new(url.to_string());
                McpClient::new(server_alias.to_string(), Box::new(transport))
            }
        };

        // Initialize the MCP connection.
        client
            .initialize()
            .await
            .map_err(|e| format!("MCP initialize failed: {e}"))?;

        // Invoke the tool.
        let call_result = client
            .call_tool(tool_name, arguments)
            .await
            .map_err(|e| format!("MCP tool call failed: {e}"))?;

        client.close().await;

        // Convert MCP content to a JSON value.
        let output_value = content_to_json(&call_result.content);

        Ok(ExecutionResult {
            output: output_value.clone(),
            state_patch: json!({}), // caller can configure mapping
            duration_ms: start.elapsed().as_millis() as u64,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}

/// Convert a list of `McpContent` items to a single JSON value.
fn content_to_json(content: &[jamjet_mcp::types::McpContent]) -> Value {
    use jamjet_mcp::types::McpContent;
    match content {
        [] => json!(null),
        [single] => match single {
            McpContent::Text { text } => json!(text),
            McpContent::Image { data, mime_type } => {
                json!({ "type": "image", "data": data, "mime_type": mime_type })
            }
            McpContent::Resource {
                uri,
                text,
                mime_type,
            } => json!({ "type": "resource", "uri": uri, "text": text, "mime_type": mime_type }),
        },
        many => {
            let items: Vec<Value> = many.iter().map(|c| match c {
                McpContent::Text { text } => json!(text),
                McpContent::Image { data, mime_type } => json!({ "type": "image", "data": data, "mime_type": mime_type }),
                McpContent::Resource { uri, text, mime_type } => json!({ "type": "resource", "uri": uri, "text": text, "mime_type": mime_type }),
            }).collect();
            json!(items)
        }
    }
}
