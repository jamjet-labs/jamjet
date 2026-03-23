//! Executor for `AgentTool` workflow nodes (Phase B — sync, streaming, and conversational modes).
//!
//! Invokes a remote agent via A2A-style HTTP POST and returns the response as the node output.
//! - Sync:           POST `{agent_uri}/tasks/send`           — single request/response
//! - Streaming:      POST `{agent_uri}/tasks/sendSubscribe`  — NDJSON stream with budget guard
//! - Conversational: POST `{agent_uri}/tasks/send` in a loop — multi-turn exchange

#![allow(clippy::too_many_arguments)]

use crate::executor::{ExecutionResult, NodeExecutor, StreamEventSender};
use async_trait::async_trait;
use bytes::BytesMut;
use futures::StreamExt;
use jamjet_state::backend::WorkItem;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;
use tracing::{debug, instrument, warn};

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
    /// In test builds, `http://` is also accepted (for wiremock).
    fn resolve_url(agent_uri: &str, endpoint: &str) -> Result<String, String> {
        let is_http =
            agent_uri.starts_with("https://") || (cfg!(test) && agent_uri.starts_with("http://"));
        if is_http {
            Ok(format!("{}/{}", agent_uri.trim_end_matches('/'), endpoint))
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

    /// Legacy streaming mode: POST to `/tasks/sendSubscribe`, buffer full body, parse NDJSON.
    ///
    /// Emits `agent_tool_progress` events per chunk, with optional early termination
    /// when `max_cost_usd` budget is exceeded. Final event is `agent_tool_completed`
    /// or `agent_tool_terminated` on budget breach.
    ///
    /// Kept as fallback when invoked through `execute()` (no channel available).
    /// The incremental path is `execute_streaming` on the trait.
    async fn stream_ndjson(
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

            let chunk: Value =
                serde_json::from_str(trimmed).unwrap_or_else(|_| json!({ "raw": trimmed }));

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

    /// Best-effort A2A cancel. Fire-and-forget with 5s timeout.
    async fn send_a2a_cancel(client: &Client, agent_uri: &str, task_id: &Option<String>) {
        if let Some(ref id) = task_id {
            if let Ok(cancel_url) = Self::resolve_url(agent_uri, "tasks/cancel") {
                let _ = client
                    .post(&cancel_url)
                    .json(&serde_json::json!({ "id": id }))
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await;
            }
        }
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
                return Err(format!(
                    "Agent returned error {status} on turn {turn}: {body}"
                ));
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
            let status = response
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("");
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
            .and_then(|a| {
                a.get("explicit")
                    .and_then(|v| v.as_str())
                    .or_else(|| a.as_str())
            })
            .ok_or("AgentTool: missing 'agent' URI in payload")?;

        // Extract mode — handle both string and object forms
        // e.g. "sync" or {"conversational": {"max_turns": 5}}
        let mode = if let Some(mode_val) = p.get("mode") {
            if let Some(s) = mode_val.as_str() {
                s.to_string()
            } else if mode_val.get("conversational").is_some() {
                "conversational".to_string()
            } else if mode_val.get("streaming").is_some() {
                "streaming".to_string()
            } else {
                "sync".to_string()
            }
        } else {
            "sync".to_string()
        };
        let output_key = p
            .get("output_key")
            .and_then(|v| v.as_str())
            .unwrap_or("result");
        let timeout_ms = p
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);
        let input = p.get("input").cloned().unwrap_or(json!({}));
        // Budget lookup: check nested {"budget": {"max_cost_usd": …}} first, then flat "max_cost_usd"
        let max_cost_usd = p
            .get("budget")
            .and_then(|b| b.get("max_cost_usd"))
            .and_then(|v| v.as_f64())
            .or_else(|| p.get("max_cost_usd").and_then(|v| v.as_f64()));

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

        match mode.as_str() {
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
                self.stream_ndjson(
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

    /// Incremental NDJSON streaming with per-chunk idle timeout, budget guard,
    /// and A2A cancel on early termination. Events are sent via `tx` in real time.
    #[instrument(skip(self, item, tx), fields(node_id = %item.node_id))]
    async fn execute_streaming(
        &self,
        item: &WorkItem,
        tx: StreamEventSender,
    ) -> Result<ExecutionResult, String> {
        let start = std::time::Instant::now();
        let p = &item.payload;

        // ── Extract params ──────────────────────────────────────────────
        let agent_uri = p
            .get("agent")
            .and_then(|a| {
                a.get("explicit")
                    .and_then(|v| v.as_str())
                    .or_else(|| a.as_str())
            })
            .ok_or("AgentTool: missing 'agent' URI in payload")?;

        let mode = if let Some(mode_val) = p.get("mode") {
            if let Some(s) = mode_val.as_str() {
                s.to_string()
            } else if mode_val.get("streaming").is_some() {
                "streaming".to_string()
            } else {
                "sync".to_string()
            }
        } else {
            "sync".to_string()
        };

        // Short-circuit non-streaming modes back to execute()
        if mode != "streaming" {
            return self.execute(item).await;
        }

        let input = p.get("input").cloned().unwrap_or(json!({}));

        let max_cost_usd = p
            .get("budget")
            .and_then(|b| b.get("max_cost_usd"))
            .and_then(|v| v.as_f64())
            .or_else(|| p.get("max_cost_usd").and_then(|v| v.as_f64()));

        let idle_timeout_secs = p
            .get("idle_timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(30);

        // Resolve protocol
        let protocol = if agent_uri.starts_with("https://")
            || (cfg!(test) && agent_uri.starts_with("http://"))
        {
            "a2a"
        } else if agent_uri.starts_with("jamjet://") {
            "local"
        } else {
            "mcp"
        };

        // Compute input hash
        let mut hasher = DefaultHasher::new();
        input.to_string().hash(&mut hasher);
        let input_hash = format!("{:016x}", hasher.finish());

        // ── Build client WITHOUT overall timeout (streaming uses per-chunk idle) ──
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("HTTP client: {e}"))?;

        // ── Emit invoked event ──────────────────────────────────────────
        let now_ms = || -> u64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        };

        let invoked_event = json!({
            "type": "agent_tool_invoked",
            "node_id": &item.node_id,
            "agent_uri": agent_uri,
            "mode": &mode,
            "protocol": protocol,
            "input_hash": &input_hash,
            "timestamp_ms": now_ms()
        });
        if tx.send(invoked_event).await.is_err() {
            return Err("Streaming receiver dropped before invocation event".into());
        }

        // ── POST to /tasks/sendSubscribe ────────────────────────────────
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

        // ── Incremental NDJSON read loop ────────────────────────────────
        let mut stream = resp.bytes_stream();
        let mut line_buf = BytesMut::new();
        let mut chunk_index: u64 = 0;
        let mut accumulated_cost: f64 = 0.0;
        let mut task_id: Option<String> = None;
        let mut last_chunk: Value = json!(null);
        let output_key = p
            .get("output_key")
            .and_then(|v| v.as_str())
            .unwrap_or("result");
        let mut terminated_early = false;
        let mut terminal_error: Option<String> = None;
        let idle_dur = Duration::from_secs(idle_timeout_secs);

        loop {
            match tokio::time::timeout(idle_dur, stream.next()).await {
                // Timeout — no data within idle window
                Err(_elapsed) => {
                    warn!(
                        node_id = %item.node_id,
                        idle_timeout_secs,
                        "AgentTool streaming: idle timeout, terminating"
                    );
                    let _ = tx
                        .send(json!({
                            "type": "agent_tool_terminated",
                            "node_id": &item.node_id,
                            "reason": "idle_timeout",
                            "accumulated_cost_usd": accumulated_cost,
                            "latency_ms": start.elapsed().as_millis() as u64,
                            "timestamp_ms": now_ms()
                        }))
                        .await;
                    Self::send_a2a_cancel(&client, agent_uri, &task_id).await;
                    terminated_early = true;
                    terminal_error =
                        Some(format!("AgentTool idle timeout after {idle_timeout_secs}s"));
                    break;
                }
                // Stream ended normally
                Ok(None) => {
                    break;
                }
                // Network error
                Ok(Some(Err(e))) => {
                    warn!(
                        node_id = %item.node_id,
                        error = %e,
                        "AgentTool streaming: network error"
                    );
                    let _ = tx
                        .send(json!({
                            "type": "agent_tool_error",
                            "node_id": &item.node_id,
                            "error": e.to_string(),
                            "timestamp_ms": now_ms()
                        }))
                        .await;
                    terminated_early = true;
                    terminal_error = Some(format!("AgentTool stream error: {e}"));
                    break;
                }
                // Got a chunk of bytes
                Ok(Some(Ok(bytes))) => {
                    line_buf.extend_from_slice(&bytes);

                    // Process all complete lines in the buffer
                    while let Some(newline_pos) = line_buf.iter().position(|&b| b == b'\n') {
                        let line_bytes = line_buf.split_to(newline_pos + 1);
                        let line_str = match std::str::from_utf8(&line_bytes) {
                            Ok(s) => s.trim().to_string(),
                            Err(e) => {
                                warn!(
                                    node_id = %item.node_id,
                                    error = %e,
                                    "AgentTool streaming: non-UTF8 chunk, skipping"
                                );
                                continue;
                            }
                        };
                        if line_str.is_empty() {
                            continue;
                        }

                        let chunk: Value = serde_json::from_str(&line_str)
                            .unwrap_or_else(|_| json!({ "raw": &line_str }));
                        last_chunk = chunk.clone();

                        // Extract task_id from first chunk
                        if task_id.is_none() {
                            if let Some(id) = chunk.get("id").and_then(|v| v.as_str()) {
                                task_id = Some(id.to_string());
                            }
                        }

                        // Accumulate cost
                        if let Some(cost) = chunk.get("cost_usd").and_then(|v| v.as_f64()) {
                            accumulated_cost += cost;
                        }

                        // Emit progress event
                        let progress = json!({
                            "type": "agent_tool_progress",
                            "node_id": &item.node_id,
                            "chunk_index": chunk_index,
                            "chunk": &chunk,
                            "accumulated_cost_usd": accumulated_cost,
                            "timestamp_ms": now_ms()
                        });
                        chunk_index += 1;

                        if tx.send(progress).await.is_err() {
                            // Receiver dropped — treat as cancellation
                            debug!(
                                node_id = %item.node_id,
                                "AgentTool streaming: receiver dropped, cancelling"
                            );
                            Self::send_a2a_cancel(&client, agent_uri, &task_id).await;
                            terminated_early = true;
                            terminal_error = Some("AgentTool stream receiver dropped".into());
                            break;
                        }

                        // Budget guard
                        if let Some(budget) = max_cost_usd {
                            if accumulated_cost > budget {
                                debug!(
                                    node_id = %item.node_id,
                                    accumulated_cost,
                                    budget,
                                    "AgentTool streaming: budget exceeded, terminating"
                                );
                                let _ = tx
                                    .send(json!({
                                        "type": "agent_tool_terminated",
                                        "node_id": &item.node_id,
                                        "reason": "budget_exceeded",
                                        "accumulated_cost_usd": accumulated_cost,
                                        "latency_ms": start.elapsed().as_millis() as u64,
                                        "timestamp_ms": now_ms()
                                    }))
                                    .await;
                                Self::send_a2a_cancel(&client, agent_uri, &task_id).await;
                                terminated_early = true;
                                break;
                            }
                        }
                    }

                    // If inner loop broke due to termination, break outer loop too
                    if terminated_early {
                        break;
                    }
                }
            }
        }

        // ── Drain remaining bytes in line_buf ───────────────────────────
        if !terminated_early && !line_buf.is_empty() {
            if let Ok(remaining) = std::str::from_utf8(&line_buf) {
                let trimmed = remaining.trim();
                if !trimmed.is_empty() {
                    let chunk: Value =
                        serde_json::from_str(trimmed).unwrap_or_else(|_| json!({ "raw": trimmed }));
                    last_chunk = chunk.clone();

                    if let Some(cost) = chunk.get("cost_usd").and_then(|v| v.as_f64()) {
                        accumulated_cost += cost;
                    }

                    let _ = tx
                        .send(json!({
                            "type": "agent_tool_progress",
                            "node_id": &item.node_id,
                            "chunk_index": chunk_index,
                            "chunk": &chunk,
                            "accumulated_cost_usd": accumulated_cost,
                            "timestamp_ms": now_ms()
                        }))
                        .await;
                }
            }
        }

        // ── Return error for hard failures ───────────────────────────────
        if let Some(error) = terminal_error {
            return Err(error);
        }

        // ── Emit completed (if not terminated early) ────────────────────
        let duration_ms = start.elapsed().as_millis() as u64;
        if !terminated_early {
            let _ = tx
                .send(json!({
                    "type": "agent_tool_completed",
                    "node_id": &item.node_id,
                    "output": &last_chunk,
                    "total_cost": accumulated_cost,
                    "latency_ms": duration_ms,
                    "timestamp_ms": now_ms()
                }))
                .await;
        }

        Ok(ExecutionResult {
            output: json!({ output_key: last_chunk }),
            state_patch: json!({}),
            duration_ms,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::NodeExecutor;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a WorkItem that targets the given agent URI with streaming mode.
    fn make_test_work_item(
        agent_uri: &str,
        idle_timeout: Option<u64>,
        max_cost: Option<f64>,
    ) -> WorkItem {
        let mut payload = serde_json::json!({
            "agent": agent_uri,
            "mode": "streaming",
            "input": {"query": "test"},
            "workflow_id": "wf1",
            "workflow_version": "1.0.0",
        });
        if let Some(t) = idle_timeout {
            payload["idle_timeout_secs"] = serde_json::json!(t);
        }
        if let Some(c) = max_cost {
            payload["budget"] = serde_json::json!({"max_cost_usd": c});
        }
        WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: jamjet_core::workflow::ExecutionId::new(),
            node_id: "n1".into(),
            queue_type: "agent_tool".into(),
            payload,
            attempt: 1,
            max_attempts: 3,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            tenant_id: "default".into(),
        }
    }

    /// Join NDJSON lines into a single body terminated by a newline.
    fn ndjson_body(lines: &[&str]) -> String {
        lines.join("\n") + "\n"
    }

    /// Drain all events from the receiver (non-blocking).
    fn collect_events(
        rx: &mut tokio::sync::mpsc::Receiver<serde_json::Value>,
    ) -> Vec<serde_json::Value> {
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    }

    // ── Test 1: streams NDJSON chunks in order ──────────────────────────

    #[tokio::test]
    async fn streams_ndjson_chunks_in_order() {
        let server = MockServer::start().await;

        let body = ndjson_body(&[r#"{"text":"hello"}"#, r#"{"text":"world"}"#]);

        Mock::given(method("POST"))
            .and(path("/tasks/sendSubscribe"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let item = make_test_work_item(&server.uri(), Some(5), None);
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);

        let executor = AgentToolExecutor;
        let result = executor.execute_streaming(&item, tx).await;
        assert!(
            result.is_ok(),
            "execute_streaming failed: {:?}",
            result.err()
        );

        let events = collect_events(&mut rx);

        // Expect: invoked, progress(0), progress(1), completed
        assert!(
            events.len() >= 4,
            "Expected at least 4 events, got {}: {:#?}",
            events.len(),
            events
        );

        assert_eq!(events[0]["type"], "agent_tool_invoked");
        assert_eq!(events[0]["mode"], "streaming");

        assert_eq!(events[1]["type"], "agent_tool_progress");
        assert_eq!(events[1]["chunk_index"], 0);
        assert_eq!(events[1]["chunk"]["text"], "hello");

        assert_eq!(events[2]["type"], "agent_tool_progress");
        assert_eq!(events[2]["chunk_index"], 1);
        assert_eq!(events[2]["chunk"]["text"], "world");

        assert_eq!(events[3]["type"], "agent_tool_completed");
    }

    // ── Test 2: budget exceeded terminates stream ───────────────────────

    #[tokio::test]
    async fn budget_exceeded_terminates_stream() {
        let server = MockServer::start().await;

        // 3 chunks each costing 0.3; budget is 0.5 → should terminate after chunk 1
        let body = ndjson_body(&[
            r#"{"text":"a","cost_usd":0.3}"#,
            r#"{"text":"b","cost_usd":0.3}"#,
            r#"{"text":"c","cost_usd":0.3}"#,
        ]);

        Mock::given(method("POST"))
            .and(path("/tasks/sendSubscribe"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        // Also mock /tasks/cancel so the A2A cancel doesn't fail
        Mock::given(method("POST"))
            .and(path("/tasks/cancel"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let item = make_test_work_item(&server.uri(), Some(5), Some(0.5));
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);

        let executor = AgentToolExecutor;
        let result = executor.execute_streaming(&item, tx).await;
        assert!(result.is_ok());

        let events = collect_events(&mut rx);

        // Find a terminated event with reason "budget_exceeded"
        let terminated = events.iter().find(|e| e["type"] == "agent_tool_terminated");
        assert!(
            terminated.is_some(),
            "Expected an agent_tool_terminated event, got: {:#?}",
            events
        );
        assert_eq!(terminated.unwrap()["reason"], "budget_exceeded");

        // Should NOT have a completed event
        let completed = events.iter().any(|e| e["type"] == "agent_tool_completed");
        assert!(
            !completed,
            "Should not have agent_tool_completed when budget exceeded"
        );
    }

    // ── Test 3: malformed JSON becomes raw ──────────────────────────────

    #[tokio::test]
    async fn malformed_json_becomes_raw() {
        let server = MockServer::start().await;

        let body = ndjson_body(&[
            r#"{"text":"first"}"#,
            "not json at all",
            r#"{"text":"third"}"#,
        ]);

        Mock::given(method("POST"))
            .and(path("/tasks/sendSubscribe"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let item = make_test_work_item(&server.uri(), Some(5), None);
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);

        let executor = AgentToolExecutor;
        let result = executor.execute_streaming(&item, tx).await;
        assert!(result.is_ok());

        let events = collect_events(&mut rx);

        // events[0] = invoked, events[1] = progress(0), events[2] = progress(1), events[3] = progress(2), events[4] = completed
        let progress_events: Vec<&serde_json::Value> = events
            .iter()
            .filter(|e| e["type"] == "agent_tool_progress")
            .collect();

        assert_eq!(
            progress_events.len(),
            3,
            "Expected 3 progress events, got {}: {:#?}",
            progress_events.len(),
            progress_events
        );

        // First chunk: valid JSON
        assert_eq!(progress_events[0]["chunk"]["text"], "first");

        // Second chunk: malformed → wrapped in {"raw": "not json at all"}
        assert_eq!(progress_events[1]["chunk"]["raw"], "not json at all");

        // Third chunk: valid JSON
        assert_eq!(progress_events[2]["chunk"]["text"], "third");
    }
}
