//! Node executors — one per node kind.
//! Each executor takes a WorkItem, runs the node logic, and returns a result.

use jamjet_state::backend::WorkItem;
use serde_json::Value;

pub struct ExecutionResult {
    pub output: Value,
    pub state_patch: Value,
    pub duration_ms: u64,
    /// Telemetry: GenAI provider system (e.g. "anthropic", "openai"). None for non-model nodes.
    pub gen_ai_system: Option<String>,
    /// Telemetry: model name used.
    pub gen_ai_model: Option<String>,
    /// Telemetry: input tokens consumed.
    pub input_tokens: Option<u64>,
    /// Telemetry: output tokens generated.
    pub output_tokens: Option<u64>,
    /// Telemetry: finish reason (e.g. "stop", "length", "tool_calls").
    pub finish_reason: Option<String>,
}

/// Trait implemented by each node kind executor.
#[async_trait::async_trait]
pub trait NodeExecutor: Send + Sync {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, String>;
}
