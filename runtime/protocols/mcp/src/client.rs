//! MCP client — connects to external MCP servers, discovers and invokes tools.

use crate::transport::McpTransport;
use crate::types::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, info};

pub struct McpClient {
    transport: Box<dyn McpTransport>,
    request_counter: AtomicU64,
    server_name: String,
}

impl McpClient {
    pub fn new(server_name: String, transport: Box<dyn McpTransport>) -> Self {
        Self {
            transport,
            request_counter: AtomicU64::new(1),
            server_name,
        }
    }

    fn next_id(&self) -> u64 {
        self.request_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Initialize the MCP connection (sends the initialize handshake).
    pub async fn initialize(&self) -> Result<Value, String> {
        let request = JsonRpcRequest::new(
            self.next_id(),
            "initialize",
            Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "roots": { "listChanged": false }
                },
                "clientInfo": {
                    "name": "jamjet",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
        );
        let response = self.transport.send(request).await?;
        response
            .result
            .ok_or_else(|| format!("MCP initialize failed: {:?}", response.error))
    }

    /// Discover all tools available on this MCP server.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>, String> {
        debug!(server = %self.server_name, "Listing MCP tools");
        let request = JsonRpcRequest::new(self.next_id(), "tools/list", None);
        let response = self.transport.send(request).await?;
        let result = response
            .result
            .ok_or_else(|| format!("tools/list failed: {:?}", response.error))?;

        let tools: Vec<McpTool> =
            serde_json::from_value(result["tools"].clone()).map_err(|e| e.to_string())?;

        info!(
            server = %self.server_name,
            count = tools.len(),
            "Discovered MCP tools"
        );
        Ok(tools)
    }

    /// Invoke a tool on this MCP server.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<McpToolCallResponse, String> {
        debug!(server = %self.server_name, tool = name, "Calling MCP tool");
        let request = JsonRpcRequest::new(
            self.next_id(),
            "tools/call",
            Some(json!({ "name": name, "arguments": arguments })),
        );
        let response = self.transport.send(request).await?;
        let result = response
            .result
            .ok_or_else(|| format!("tools/call failed: {:?}", response.error))?;

        serde_json::from_value(result).map_err(|e| e.to_string())
    }

    /// List resources available on this MCP server.
    pub async fn list_resources(&self) -> Result<Vec<McpResource>, String> {
        let request = JsonRpcRequest::new(self.next_id(), "resources/list", None);
        let response = self.transport.send(request).await?;
        let result = response
            .result
            .ok_or_else(|| format!("resources/list failed: {:?}", response.error))?;

        serde_json::from_value(result["resources"].clone()).map_err(|e| e.to_string())
    }

    /// Read a resource from this MCP server.
    pub async fn read_resource(&self, uri: &str) -> Result<Value, String> {
        let request = JsonRpcRequest::new(
            self.next_id(),
            "resources/read",
            Some(json!({ "uri": uri })),
        );
        let response = self.transport.send(request).await?;
        response
            .result
            .ok_or_else(|| format!("resources/read failed: {:?}", response.error))
    }

    pub async fn close(&self) {
        self.transport.close().await;
    }
}
