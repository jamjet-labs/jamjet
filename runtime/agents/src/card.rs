use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The Agent Card — machine-readable capability manifest for a JamJet agent.
/// Aligned with the A2A Agent Card specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    pub id: String,
    pub uri: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub capabilities: AgentCapabilities,
    pub autonomy: AutonomyLevel,
    pub constraints: Option<AutonomyConstraints>,
    pub auth: AuthSpec,
    /// Latency classification for routing decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_class: Option<LatencyClass>,
    /// Cost tier for delegation decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_class: Option<CostClass>,
    /// Reasoning modes this agent supports (e.g. react, plan-and-execute, critic).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_modes: Vec<String>,
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// Latency classification for agent routing and SLA matching.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LatencyClass {
    /// Sub-second responses (caches, deterministic tools).
    Realtime,
    /// Seconds (fast model inference, simple tool calls).
    Fast,
    /// Tens of seconds (complex reasoning, multi-step).
    Medium,
    /// Minutes+ (research tasks, multi-agent delegation).
    Slow,
}

/// Cost tier for delegation budget planning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CostClass {
    /// Free or negligible cost (local tools, cached responses).
    Free,
    /// Low cost per invocation (small models, simple APIs).
    Low,
    /// Moderate cost (frontier models, paid APIs).
    Medium,
    /// High cost (long-running agents, expensive external services).
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub skills: Vec<Skill>,
    /// Supported protocols: "mcp_server", "mcp_client", "a2a"
    pub protocols: Vec<String>,
    /// Tools this agent exposes to others.
    pub tools_provided: Vec<String>,
    /// Tools this agent needs from others.
    pub tools_consumed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub input_schema: String,
    pub output_schema: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyLevel {
    Deterministic,
    #[default]
    Guided,
    BoundedAutonomous,
    FullyAutonomous,
}

/// Every field is optional (RFC-005), so a card may carry only the
/// constraints it sets.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AutonomyConstraints {
    pub max_iterations: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub token_budget: Option<u64>,
    pub cost_budget_usd: Option<f64>,
    /// Glob patterns for the tools this agent may use (e.g. "search_*").
    /// Enforced on every node attributed to the agent (`agent_ref` in the
    /// workflow IR) by `jamjet_worker::node_guard`; empty means no
    /// restriction. See `jamjet_policy::allowed_tools`.
    pub allowed_tools: Vec<String>,
    /// Glob patterns for blocked tools (e.g. "delete_*")
    pub blocked_tools: Vec<String>,
    /// Agent ids this agent is allowed to delegate to.
    pub allowed_delegations: Vec<String>,
    /// Operations that require explicit approval before execution.
    pub require_approval_for: Vec<String>,
    pub time_budget_secs: Option<u64>,
    /// Consecutive failed steps of this agent before its circuit breaker
    /// trips. Absent means [`DEFAULT_MAX_CONSECUTIVE_ERRORS`].
    pub max_consecutive_errors: Option<u32>,
    /// What happens when a limit is reached or the circuit breaker trips.
    /// Absent means [`OnLimitExceeded::Fail`].
    pub on_limit_exceeded: Option<OnLimitExceeded>,
}

/// The circuit breaker's threshold when a card sets none.
pub const DEFAULT_MAX_CONSECUTIVE_ERRORS: u32 = 3;

/// What the runtime does with a node that would take an agent past one of
/// its limits (or past a tripped circuit breaker).
///
/// RFC-005 also names `escalate_to_supervisor`. Nothing implements it, so it
/// is not accepted: a card that asks for it fails to register rather than
/// being silently treated as `fail`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnLimitExceeded {
    /// Refuse the node and fail it for good.
    #[default]
    Fail,
    /// Hold the node for a human approval of that one step, through the
    /// ordinary approval path (bound to the action, with the policy chain's
    /// deadline). Each approval releases one step, once: a retry, a duplicate
    /// item or the next step past the limit needs its own.
    EscalateToHuman,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthSpec {
    BearerToken { scopes: Vec<String> },
    ApiKey { header: String },
    None,
}
