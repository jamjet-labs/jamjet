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
    #[serde(default)]
    pub labels: HashMap<String, String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyConstraints {
    pub max_iterations: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub token_budget: Option<u64>,
    pub cost_budget_usd: Option<f64>,
    /// Glob patterns for allowed tools (e.g. "search_*")
    pub allowed_tools: Vec<String>,
    /// Glob patterns for blocked tools (e.g. "delete_*")
    pub blocked_tools: Vec<String>,
    /// Agent ids this agent is allowed to delegate to.
    pub allowed_delegations: Vec<String>,
    /// Operations that require explicit approval before execution.
    pub require_approval_for: Vec<String>,
    pub time_budget_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthSpec {
    BearerToken { scopes: Vec<String> },
    ApiKey { header: String },
    None,
}
