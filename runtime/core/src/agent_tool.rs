use serde::{Deserialize, Serialize};

/// Specifies which agent to invoke as a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTarget {
    /// Explicit agent URI, e.g. `"jamjet://org/classifier-agent"`.
    Explicit(String),
    /// Auto-resolve via the coordinator at compile time.
    Auto,
}

/// Invocation mode for an agent tool call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentToolMode {
    /// Single request/response (default).
    #[default]
    Sync,
    /// Streamed output with support for early termination.
    Streaming,
    /// Multi-turn conversational exchange.
    Conversational {
        max_turns: u32,
        max_tokens_per_turn: Option<u32>,
    },
}

/// Spending / token budget constraints for an agent tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolBudget {
    pub max_cost_usd: Option<f64>,
    pub max_tokens: Option<u32>,
}

/// Wire protocol used to call the target agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationProtocol {
    LocalGrpc,
    A2a,
    Mcp,
}
