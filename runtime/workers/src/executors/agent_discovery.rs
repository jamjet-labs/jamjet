//! Executor for `AgentDiscovery` workflow nodes (F2.4).
//!
//! Fetches a remote Agent Card from `/.well-known/agent.json` and returns
//! the discovered capabilities as the node output so the workflow can select
//! an agent and delegate to it dynamically.

use crate::executor::{ExecutionResult, NodeExecutor};
use async_trait::async_trait;
use jamjet_state::backend::WorkItem;
use serde_json::{json, Value};
use tracing::{debug, instrument};

pub struct AgentDiscoveryExecutor;

#[async_trait]
impl NodeExecutor for AgentDiscoveryExecutor {
    #[instrument(skip(self, item), fields(node_id = %item.node_id))]
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, String> {
        let start = std::time::Instant::now();

        let agent_url = item
            .payload
            .get("agent_url")
            .and_then(|v| v.as_str())
            .ok_or("AgentDiscovery: missing 'agent_url' in payload")?;

        debug!(agent_url = %agent_url, "Discovering agent");

        let card_url = format!("{}/.well-known/agent.json", agent_url.trim_end_matches('/'));
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("HTTP client: {e}"))?;

        let card: Value = client
            .get(&card_url)
            .send()
            .await
            .map_err(|e| format!("fetch Agent Card from {card_url}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("parse Agent Card: {e}"))?;

        let duration_ms = start.elapsed().as_millis() as u64;

        Ok(ExecutionResult {
            output: card.clone(),
            state_patch: json!({ "discovered_agent": card }),
            duration_ms,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}
