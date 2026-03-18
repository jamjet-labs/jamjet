//! Executor for `AgentTool` workflow nodes (Phase B — sync, streaming, and conversational modes).
//!
//! Invokes a remote agent via A2A-style HTTP POST and returns the response as the node output.
//! - Sync:           POST `{agent_uri}/tasks/send`           — single request/response
//! - Streaming:      POST `{agent_uri}/tasks/sendSubscribe`  — NDJSON stream with budget guard
//! - Conversational: POST `{agent_uri}/tasks/send` in a loop — multi-turn exchange

use crate::executor::{ExecutionResult, NodeExecutor};
use async_trait::async_trait;
use jamjet_state::backend::WorkItem;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use tracing::{debug, instrument};

pub struct AgentToolExecutor;

impl AgentToolExecutor {
    /// Build an HTTP client with the given timeout.
    fn build_client(timeout_ms: u64) -> Result<Client, String> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(timeout_ms))
            .build()
            .map_err(|e| format!("HTTP client: {e}"))
    }

    /// Resolve the base agent URI to an `https://` URL, returning an error for unsupported schemes.
    fn resolve_url(agent_uri: &str, endpoint: &str) -> Result<String, String> {
        if agent_uri.starts_with("https://") {
            Ok(format!(
                "{}/{}",
                agent_uri.trim_end_matches('/'),
                endpoint
            ))
        } else {
            Err(format!(
                "Cannot resolve '{}' to HTTP endpoint. \
                 Only https:// agent URIs are supported for remote invocation.",
                agent_uri
            ))
        }
    }

    /// Execute in sync mode: single POST to `/tasks/send`, returns one response.
    async fn execute_sync(
        &self,
        item: &WorkItem,
        agent_uri: &str,
        protocol: &str,
        output_key: &str,
        timeout_ms: u64,
        input: &Value,
        input_hash: &str,
        start: std::time::Instant,
    ) -> Result<ExecutionResult, String> {
        let client = Self::build_client(timeout_ms)?;
        let task_url = Self::resolve_url(agent_uri, "tasks/send")?;

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
                        "mode": "sync",
                        "protocol": protocol,
                        "input_hash": input_hash
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

    /// Execute in streaming mode: POST to `/tasks/sendSubscribe`, read NDJSON stream.
    ///
    /// Emits `agent_tool_progress` events per chunk, with optional early termination
    /// when `max_cost_usd` budget is exceeded. Final event is `agent_tool_completed`
    /// or `agent_tool_terminated` on budget breach.
    async fn execute_streaming(
        &self,
        item: &WorkItem,
        agent_uri: &str,
        protocol: &str,
        output_key: &str,
        timeout_ms: u64,
        input: &Value,
        input_hash: &str,
        max_cost_usd: Option<f64>,
        start: std::time::Instant,
    ) -> Result<ExecutionResult, String> {
        let client = Self::build_client(timeout_ms)?;
        let task_url = Self::resolve_url(agent_uri, "tasks/sendSubscribe")?;

        let resp = client
            .post(&task_url)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "tasks/sendSubscribe",
                "params": { "message": { "parts": [{ "text": input.to_string() }] } }
            }))
            .send()
            .await
            .map_err(|e| format!("AgentTool streaming invocation failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Agent returned error {status}: {body}"));
        }

        // Read full body, then parse NDJSON line by line
        let body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read streaming response body: {e}"))?;

        let mut events: Vec<Value> = vec![json!({
            "type": "agent_tool_invoked",
            "node_id": &item.node_id,
            "agent_uri": agent_uri,
            "mode": "streaming",
            "protocol": protocol,
            "input_hash": input_hash
        })];

        let mut accumulated_cost: f64 = 0.0;
        let mut terminated_early = false;
        let mut last_chunk: Value = json!(null);
        let mut chunk_index: u64 = 0;

        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let chunk: Value = serde_json::from_str(trimmed)
                .unwrap_or_else(|_| json!({ "raw": trimmed }));

            // Extract cost from chunk if present
            if let Some(cost) = chunk.get("cost_usd").and_then(|v| v.as_f64()) {
                accumulated_cost += cost;
            }

            events.push(json!({
                "type": "agent_tool_progress",
                "node_id": &item.node_id,
                "chunk_index": chunk_index,
                "chunk": &chunk,
                "accumulated_cost_usd": accumulated_cost
            }));

            last_chunk = chunk;
            chunk_index += 1;

            // Budget guard — terminate early if cost threshold exceeded
            if let Some(budget) = max_cost_usd {
                if accumulated_cost > budget {
                    terminated_early = true;
                    debug!(
                        node_id = %item.node_id,
                        accumulated_cost,
                        budget,
                        "AgentTool streaming: budget exceeded, terminating early"
                    );
                    break;
                }
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        if terminated_early {
            events.push(json!({
                "type": "agent_tool_terminated",
                "node_id": &item.node_id,
                "reason": "budget_exceeded",
                "accumulated_cost_usd": accumulated_cost,
                "latency_ms": duration_ms
            }));
        } else {
            events.push(json!({
                "type": "agent_tool_completed",
                "node_id": &item.node_id,
                "output": &last_chunk,
                "total_cost": accumulated_cost,
                "latency_ms": duration_ms
            }));
        }

        Ok(ExecutionResult {
            output: json!({ output_key: &last_chunk }),
            state_patch: json!({ "agent_tool_events": events }),
            duration_ms,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }

    /// Execute in conversational mode: multi-turn loop sending to `/tasks/send`.
    ///
    /// Reads `max_turns` from `payload.mode.conversational.max_turns` (default 5).
    /// Each turn records outbound and inbound `agent_tool_turn` events. Stops early
    /// when the agent response carries `status: "completed"`.
    async fn execute_conversational(
        &self,
        item: &WorkItem,
        agent_uri: &str,
        protocol: &str,
        output_key: &str,
        timeout_ms: u64,
        input: &Value,
        input_hash: &str,
        start: std::time::Instant,
    ) -> Result<ExecutionResult, String> {
        let p = &item.payload;

        let max_turns = p
            .get("mode")
            .and_then(|m| m.get("conversational"))
            .and_then(|c| c.get("max_turns"))
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;

        let client = Self::build_client(timeout_ms)?;
        let task_url = Self::resolve_url(agent_uri, "tasks/send")?;

        let mut events: Vec<Value> = vec![json!({
            "type": "agent_tool_invoked",
            "node_id": &item.node_id,
            "agent_uri": agent_uri,
            "mode": "conversational",
            "protocol": protocol,
            "input_hash": input_hash
        })];

        let mut current_input = input.clone();
        let mut final_output: Value = json!(null);

        for turn in 0..max_turns {
            // Record outbound turn event
            events.push(json!({
                "type": "agent_tool_turn",
                "node_id": &item.node_id,
                "turn": turn,
                "direction": "outbound",
                "input": &current_input
            }));

            debug!(
                node_id = %item.node_id,
                turn,
                "AgentTool conversational: sending turn"
            );

            let resp = client
                .post(&task_url)
                .json(&json!({
                    "jsonrpc": "2.0",
                    "method": "tasks/send",
                    "params": { "message": { "parts": [{ "text": current_input.to_string() }] } }
                }))
                .send()
                .await
                .map_err(|e| format!("AgentTool turn {turn} failed: {e}"))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("Agent returned error {status} on turn {turn}: {body}"));
            }

            let response: Value = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse agent response on turn {turn}: {e}"))?;

            // Record inbound turn event
            events.push(json!({
                "type": "agent_tool_turn",
                "node_id": &item.node_id,
                "turn": turn,
                "direction": "inbound",
                "output": &response
            }));

            final_output = response.clone();

            // Check if the agent signals completion
            let status = response.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status == "completed" {
                debug!(
                    node_id = %item.node_id,
                    turn,
                    "AgentTool conversational: agent signalled completion"
                );
                break;
            }

            // Use agent output as next input for the following turn
            current_input = response
                .get("output")
                .cloned()
                .unwrap_or_else(|| response.clone());
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        events.push(json!({
            "type": "agent_tool_completed",
            "node_id": &item.node_id,
            "output": &final_output,
            "total_cost": 0.0,
            "latency_ms": duration_ms
        }));

        Ok(ExecutionResult {
            output: json!({ output_key: &final_output }),
            state_patch: json!({ "agent_tool_events": events }),
            duration_ms,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}

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
        let max_cost_usd = p.get("max_cost_usd").and_then(|v| v.as_f64());

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

        match mode {
            "sync" => {
                self.execute_sync(
                    item,
                    agent_uri,
                    protocol,
                    output_key,
                    timeout_ms,
                    &input,
                    &input_hash,
                    start,
                )
                .await
            }
            "streaming" => {
                self.execute_streaming(
                    item,
                    agent_uri,
                    protocol,
                    output_key,
                    timeout_ms,
                    &input,
                    &input_hash,
                    max_cost_usd,
                    start,
                )
                .await
            }
            "conversational" => {
                self.execute_conversational(
                    item,
                    agent_uri,
                    protocol,
                    output_key,
                    timeout_ms,
                    &input,
                    &input_hash,
                    start,
                )
                .await
            }
            other => Err(format!("Unknown agent_tool mode: '{other}'")),
        }
    }
}
