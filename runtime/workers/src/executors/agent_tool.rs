//! Executor for `AgentTool` workflow nodes (Phase A — sync mode only).
//!
//! Invokes a remote agent via an A2A-style HTTP POST to `{agent_uri}/tasks/send`
//! and returns the response as the node output.

use crate::executor::{ExecutionResult, NodeExecutor};
use async_trait::async_trait;
use jamjet_state::backend::WorkItem;
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use tracing::{debug, instrument};

pub struct AgentToolExecutor;

#[async_trait]
impl NodeExecutor for AgentToolExecutor {
    #[instrument(skip(self, item), fields(node_id = %item.node_id))]
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, String> {
        let start = std::time::Instant::now();
        let p = &item.payload;

        // Extract agent target — handle both { "explicit": "uri" } and plain string
        let agent_uri = p
            .get("agent")
            .and_then(|a| a.get("explicit").and_then(|v| v.as_str()).or_else(|| a.as_str()))
            .ok_or("AgentTool: missing 'agent' URI in payload")?;

        let mode = p.get("mode").and_then(|v| v.as_str()).unwrap_or("sync");
        let output_key = p.get("output_key").and_then(|v| v.as_str()).unwrap_or("result");
        let timeout_ms = p.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(30_000);
        let input = p.get("input").cloned().unwrap_or(json!({}));

        // Only sync mode in Phase A
        if mode != "sync" {
            return Err(format!(
                "Only sync mode is supported in this release. Got '{mode}'. \
                 Streaming and conversational modes coming in Phase B."
            ));
        }

        // Check for unresolved auto target
        if p.get("agent").and_then(|a| a.get("auto")).is_some() {
            return Err(
                "AgentTool with 'auto' target was not expanded at compile time. \
                 Use the compiler to expand 'auto' into coordinator + agent_tool nodes."
                    .into(),
            );
        }

        // Resolve protocol based on URI scheme
        let protocol = if agent_uri.starts_with("https://") {
            "a2a"
        } else if agent_uri.starts_with("jamjet://") {
            "local"
        } else {
            "mcp"
        };

        // Compute input hash for tracing
        let mut hasher = DefaultHasher::new();
        input.to_string().hash(&mut hasher);
        let input_hash = format!("{:016x}", hasher.finish());

        debug!(agent_uri = %agent_uri, mode = %mode, protocol = %protocol, "AgentTool: invoking");

        // Build HTTP client with timeout
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(timeout_ms))
            .build()
            .map_err(|e| format!("HTTP client: {e}"))?;

        // Resolve endpoint URL — only https:// URIs are supported for remote invocation in Phase A
        let task_url = if agent_uri.starts_with("https://") {
            format!("{}/tasks/send", agent_uri.trim_end_matches('/'))
        } else {
            return Err(format!(
                "Cannot resolve '{}' to HTTP endpoint. \
                 Only https:// agent URIs are supported for remote invocation in Phase A.",
                agent_uri
            ));
        };

        // Invoke via A2A-style HTTP POST
        let resp = client
            .post(&task_url)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "tasks/send",
                "params": { "message": { "parts": [{ "text": input.to_string() }] } }
            }))
            .send()
            .await
            .map_err(|e| format!("AgentTool invocation failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Agent returned error {status}: {body}"));
        }

        let output: Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse agent response: {e}"))?;

        let duration_ms = start.elapsed().as_millis() as u64;

        Ok(ExecutionResult {
            output: json!({ output_key: output }),
            state_patch: json!({
                "agent_tool_events": [
                    {
                        "type": "agent_tool_invoked",
                        "node_id": &item.node_id,
                        "agent_uri": agent_uri,
                        "mode": mode,
                        "protocol": protocol,
                        "input_hash": &input_hash
                    },
                    {
                        "type": "agent_tool_completed",
                        "node_id": &item.node_id,
                        "output": &output,
                        "total_cost": 0.0,
                        "latency_ms": duration_ms
                    },
                ]
            }),
            duration_ms,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}
