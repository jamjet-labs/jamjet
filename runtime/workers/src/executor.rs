//! Node executors — one per node kind.
//! Each executor takes a WorkItem, runs the node logic, and returns a result.

use jamjet_state::backend::WorkItem;
use serde_json::Value;

/// Channel sender for real-time streaming events from executors.
pub type StreamEventSender = tokio::sync::mpsc::Sender<Value>;

#[derive(Debug)]
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

/// Structured error returned by node executors.
///
/// The variant tells the worker whether to park the work item for a later retry
/// (rate-limit) or to terminate it immediately (fatal). ONLY `ModelError::RateLimited`
/// maps to `RateLimited`; every other error from every executor maps to `Fatal`.
#[derive(Debug)]
pub enum ExecutorError {
    /// Provider rate limit. The worker should park the work item and retry after
    /// `retry_after_secs` seconds. Only `ModelError::RateLimited` produces this
    /// variant; all other errors produce `Fatal`.
    RateLimited { retry_after_secs: u64 },
    /// Non-retryable failure. Equivalent to the previous `String` error type.
    Fatal(String),
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutorError::RateLimited { retry_after_secs } => {
                write!(f, "rate limited, retry after {retry_after_secs}s")
            }
            ExecutorError::Fatal(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for ExecutorError {}

/// `Err("message".to_string())` and `Err(format!(...))` callers compile unchanged.
impl From<String> for ExecutorError {
    fn from(s: String) -> Self {
        ExecutorError::Fatal(s)
    }
}

/// `Err("literal".into())` and `ok_or("literal")?` callers compile unchanged.
impl From<&str> for ExecutorError {
    fn from(s: &str) -> Self {
        ExecutorError::Fatal(s.to_string())
    }
}

/// Trait implemented by each node kind executor.
#[async_trait::async_trait]
pub trait NodeExecutor: Send + Sync {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError>;

    /// Execute with streaming event emission. Default delegates to `execute()`.
    async fn execute_streaming(
        &self,
        item: &WorkItem,
        _tx: StreamEventSender,
    ) -> Result<ExecutionResult, ExecutorError> {
        self.execute(item).await
    }
}
