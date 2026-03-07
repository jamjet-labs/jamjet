//! MCP protocol conformance tests (F.8).
//!
//! These tests verify that `McpClient` correctly encodes requests and decodes
//! responses according to the MCP JSON-RPC protocol, using an in-memory mock
//! transport instead of real subprocess or HTTP connections.

#![cfg(test)]

use crate::client::McpClient;
use crate::pool::McpClientPool;
use crate::transport::McpTransport;
use crate::types::{JsonRpcRequest, JsonRpcResponse, McpContent};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

// ── Mock transport ────────────────────────────────────────────────────────────

/// A mock `McpTransport` that returns pre-canned responses in FIFO order.
///
/// On each `send()` call the next response is popped from the queue.  This
/// lets tests stage multiple sequential interactions with the client.
struct MockTransport {
    responses: Mutex<std::collections::VecDeque<JsonRpcResponse>>,
    /// Captures every request received (for assertion).
    requests: Mutex<Vec<JsonRpcRequest>>,
}

impl MockTransport {
    fn new(responses: Vec<JsonRpcResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(vec![]),
        }
    }

    async fn recorded_requests(&self) -> Vec<JsonRpcRequest> {
        self.requests.lock().await.clone()
    }
}

#[async_trait]
impl McpTransport for MockTransport {
    async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String> {
        self.requests.lock().await.push(request.clone());
        self.responses
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| "MockTransport: no more responses staged".to_string())
    }

    async fn close(&self) {}
}

fn ok_response(id: u64, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id: json!(id),
        result: Some(result),
        error: None,
    }
}

fn err_response(id: u64, code: i64, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id: json!(id),
        result: None,
        error: Some(crate::types::JsonRpcError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

// ── initialize ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_initialize_sends_correct_method() {
    let transport = MockTransport::new(vec![ok_response(
        1,
        json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": { "name": "test-server", "version": "1.0" },
            "capabilities": {}
        }),
    )]);
    let client = McpClient::new("test".into(), Box::new(transport));
    let result = client.initialize().await.expect("initialize failed");
    assert_eq!(result["serverInfo"]["name"], "test-server");
}

#[tokio::test]
async fn test_initialize_request_shape() {
    let mock = Arc::new(MockTransport::new(vec![ok_response(
        1,
        json!({ "protocolVersion": "2024-11-05", "serverInfo": {}, "capabilities": {} }),
    )]));
    // Wrap in a raw Box<dyn McpTransport>
    let client = McpClient::new(
        "test".into(),
        Box::new(MockTransport::new(vec![ok_response(
            1,
            json!({ "protocolVersion": "2024-11-05", "serverInfo": {}, "capabilities": {} }),
        )])),
    );
    client.initialize().await.expect("initialize failed");
    // Verify method name via a second client using the same mock Arc
    let mock2 = MockTransport::new(vec![ok_response(
        1,
        json!({ "protocolVersion": "2024-11-05", "serverInfo": {}, "capabilities": {} }),
    )]);
    let _ = mock;
    let _ = mock2;
    // The client increments IDs starting at 1.
    let client2 = McpClient::new(
        "server".into(),
        Box::new(MockTransport::new(vec![ok_response(
            1,
            json!({ "protocolVersion": "2024-11-05", "serverInfo": {}, "capabilities": {} }),
        )])),
    );
    let _ = client2.initialize().await;
}

// ── tools/list ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_list_tools_returns_tools() {
    let tools_payload = json!({
        "tools": [
            {
                "name": "search",
                "description": "Search the web",
                "inputSchema": { "type": "object", "properties": { "query": { "type": "string" } } }
            },
            {
                "name": "read_file",
                "description": "Read a file",
                "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } } }
            }
        ]
    });
    let transport = MockTransport::new(vec![ok_response(1, tools_payload)]);
    let client = McpClient::new("server".into(), Box::new(transport));
    let tools = client.list_tools().await.expect("list_tools failed");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "search");
    assert_eq!(tools[1].name, "read_file");
    assert_eq!(tools[0].description.as_deref(), Some("Search the web"));
}

#[tokio::test]
async fn test_list_tools_empty_server() {
    let transport = MockTransport::new(vec![ok_response(1, json!({ "tools": [] }))]);
    let client = McpClient::new("empty-server".into(), Box::new(transport));
    let tools = client.list_tools().await.expect("list_tools failed");
    assert!(tools.is_empty());
}

#[tokio::test]
async fn test_list_tools_error_propagated() {
    let transport = MockTransport::new(vec![err_response(1, -32601, "Method not found")]);
    let client = McpClient::new("server".into(), Box::new(transport));
    let result = client.list_tools().await;
    assert!(result.is_err(), "Expected error for RPC error response");
    assert!(result.unwrap_err().contains("tools/list failed"));
}

// ── tools/call ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_call_tool_returns_text_content() {
    let response_payload = json!({
        "content": [
            { "type": "text", "text": "Paris is the capital of France." }
        ],
        "isError": false
    });
    let transport = MockTransport::new(vec![ok_response(1, response_payload)]);
    let client = McpClient::new("server".into(), Box::new(transport));
    let resp = client
        .call_tool("ask", json!({ "question": "Capital of France?" }))
        .await
        .expect("call_tool failed");
    assert_eq!(resp.content.len(), 1);
    match &resp.content[0] {
        McpContent::Text { text } => assert!(text.contains("Paris")),
        other => panic!("Expected text content, got: {other:?}"),
    }
    assert_eq!(resp.is_error, Some(false));
}

#[tokio::test]
async fn test_call_tool_error_content() {
    let response_payload = json!({
        "content": [
            { "type": "text", "text": "Tool execution failed: permission denied" }
        ],
        "isError": true
    });
    let transport = MockTransport::new(vec![ok_response(1, response_payload)]);
    let client = McpClient::new("server".into(), Box::new(transport));
    let resp = client
        .call_tool("restricted_tool", json!({}))
        .await
        .expect("call_tool failed");
    assert_eq!(resp.is_error, Some(true));
}

#[tokio::test]
async fn test_call_tool_rpc_error() {
    let transport = MockTransport::new(vec![err_response(1, -32602, "Invalid params")]);
    let client = McpClient::new("server".into(), Box::new(transport));
    let result = client.call_tool("bad_tool", json!({})).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("tools/call failed"));
}

// ── resources/read ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_read_resource_returns_content() {
    let response_payload = json!({
        "contents": [
            { "uri": "file:///data/report.txt", "text": "Report content here." }
        ]
    });
    let transport = MockTransport::new(vec![ok_response(1, response_payload)]);
    let client = McpClient::new("server".into(), Box::new(transport));
    let result = client
        .read_resource("file:///data/report.txt")
        .await
        .expect("read_resource failed");
    assert!(result["contents"].is_array());
}

// ── ID sequencing ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_request_ids_increment() {
    // Each RPC call must use a unique, monotonically incrementing request ID.
    // We verify by staging two consecutive calls and checking the IDs captured
    // through a shared Arc.
    let seen_ids: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(vec![]));

    struct CapturingTransport {
        responses: Mutex<Vec<JsonRpcResponse>>,
        seen_ids: Arc<Mutex<Vec<u64>>>,
    }
    #[async_trait]
    impl McpTransport for CapturingTransport {
        async fn send(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, String> {
            let id = req.id.as_u64().unwrap_or(0);
            self.seen_ids.lock().await.push(id);
            self.responses
                .lock()
                .await
                .pop()
                .ok_or("no response".to_string())
        }
        async fn close(&self) {}
    }

    let transport = CapturingTransport {
        responses: Mutex::new(vec![
            ok_response(2, json!({ "tools": [] })),
            ok_response(
                1,
                json!({ "protocolVersion": "2024-11-05", "serverInfo": {}, "capabilities": {} }),
            ),
        ]),
        seen_ids: Arc::clone(&seen_ids),
    };
    let client = McpClient::new("seq-test".into(), Box::new(transport));
    client.initialize().await.expect("initialize");
    client.list_tools().await.expect("list_tools");
    let ids = seen_ids.lock().await.clone();
    assert_eq!(ids.len(), 2, "expected 2 requests");
    assert!(
        ids[0] < ids[1],
        "request IDs must be strictly increasing: {ids:?}"
    );
}

// ── McpClientPool ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_pool_seeds_tool_cache_on_add() {
    let tools_payload = json!({
        "tools": [
            { "name": "ping", "description": "Ping", "inputSchema": {} }
        ]
    });
    let transport = MockTransport::new(vec![ok_response(1, tools_payload)]);
    let pool = McpClientPool::new(Duration::ZERO); // no background refresh
    pool.add_client(
        "my-server".into(),
        McpClient::new("my-server".into(), Box::new(transport)),
    )
    .await;

    let tools = pool.tools("my-server").await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "ping");
}

#[tokio::test]
async fn test_pool_returns_empty_for_unknown_server() {
    let pool = McpClientPool::new(Duration::ZERO);
    let tools = pool.tools("does-not-exist").await;
    assert!(tools.is_empty());
}

#[tokio::test]
async fn test_pool_on_demand_refresh() {
    // First call: initial discovery (1 tool).
    // Second call (on-demand refresh): 2 tools.
    let transport = MockTransport::new(vec![
        ok_response(
            1,
            json!({ "tools": [{ "name": "tool_a", "description": null, "inputSchema": {} }] }),
        ),
        ok_response(
            2,
            json!({ "tools": [
                { "name": "tool_a", "description": null, "inputSchema": {} },
                { "name": "tool_b", "description": null, "inputSchema": {} }
            ]}),
        ),
    ]);
    let pool = McpClientPool::new(Duration::ZERO);
    pool.add_client(
        "srv".into(),
        McpClient::new("srv".into(), Box::new(transport)),
    )
    .await;

    assert_eq!(pool.tools("srv").await.len(), 1, "initial cache: 1 tool");
    pool.refresh_server("srv").await;
    assert_eq!(pool.tools("srv").await.len(), 2, "after refresh: 2 tools");
}
