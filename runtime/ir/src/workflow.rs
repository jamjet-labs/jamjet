use jamjet_core::node::NodeKind;
use jamjet_core::retry::RetryPolicy;
use jamjet_core::timeout::TimeoutConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The canonical Intermediate Representation for a JamJet workflow.
///
/// Both YAML and Python workflow definitions compile to this struct before
/// being submitted to the runtime. The IR is serializable to JSON and YAML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowIr {
    /// Unique workflow definition identifier.
    pub workflow_id: String,
    /// Semantic version string (e.g., "1.0.0").
    pub version: String,
    /// Human-readable name.
    pub name: Option<String>,
    /// Optional description.
    pub description: Option<String>,
    /// Reference to the Pydantic model or JSON Schema for workflow state.
    pub state_schema: String,
    /// The first node to execute.
    pub start_node: String,
    /// All nodes in the workflow graph, keyed by node id.
    pub nodes: HashMap<String, NodeDef>,
    /// All edges (transitions) between nodes.
    pub edges: Vec<EdgeDef>,
    /// Named retry policies referenced by nodes.
    pub retry_policies: HashMap<String, RetryPolicy>,
    /// Timeout configuration for this workflow.
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    /// Named model configurations referenced by model nodes.
    pub models: HashMap<String, ModelConfig>,
    /// Named tool configurations referenced by tool nodes.
    pub tools: HashMap<String, ToolConfig>,
    /// Named MCP server configurations.
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Named remote A2A agents.
    pub remote_agents: HashMap<String, RemoteAgentConfig>,
    /// Observability labels attached to all spans from this workflow.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

impl WorkflowIr {
    /// Parse a WorkflowIr from a JSON string.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Parse a WorkflowIr from a YAML string.
    pub fn from_yaml(s: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(s)
    }

    /// Serialize to JSON (pretty-printed).
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Look up a node by id.
    pub fn node(&self, id: &str) -> Option<&NodeDef> {
        self.nodes.get(id)
    }

    /// Return all edges originating from a given node.
    pub fn edges_from(&self, node_id: &str) -> Vec<&EdgeDef> {
        self.edges.iter().filter(|e| e.from == node_id).collect()
    }

    /// Return the successors of a node (all nodes it can transition to).
    pub fn successors(&self, node_id: &str) -> Vec<&str> {
        self.edges_from(node_id)
            .into_iter()
            .map(|e| e.to.as_str())
            .collect()
    }
}

/// A single node definition in the workflow IR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeDef {
    pub id: String,
    pub kind: NodeKind,
    /// Reference to a named retry policy in `WorkflowIr::retry_policies`.
    pub retry_policy: Option<String>,
    /// Node-level timeout override (overrides workflow-level timeout).
    pub node_timeout_secs: Option<u64>,
    /// Human-readable description for display in traces and UI.
    pub description: Option<String>,
    /// Extra observability labels for this node's spans.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// A directed edge between two nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeDef {
    pub from: String,
    pub to: String,
    /// Optional condition expression. If None, this is an unconditional edge.
    /// Expressions are evaluated against the current workflow state +
    /// the last node's output.
    pub condition: Option<String>,
}

/// Configuration for a model provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    pub timeout_secs: Option<u64>,
    pub retry_policy: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// Configuration for a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolConfig {
    pub kind: ToolKind,
    /// Python: "module.submodule:function_name"
    pub reference: String,
    pub input_schema: Option<String>,
    pub output_schema: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Python,
    Http,
    Grpc,
    Mcp,
}

/// Configuration for an MCP server connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub transport: McpTransport,
    /// For stdio transport.
    pub command: Option<String>,
    pub args: Vec<String>,
    /// For http_sse transport.
    pub url: Option<String>,
    pub auth: Option<AuthConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    HttpSse,
    WebSocket,
}

/// Configuration for a remote A2A agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAgentConfig {
    pub url: String,
    pub agent_card_path: Option<String>,
    pub auth: Option<AuthConfig>,
}

/// Authentication configuration for external connections.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    Bearer {
        token_env: String,
    },
    ApiKey {
        header: String,
        key_env: String,
    },
    Oauth2 {
        client_id_env: String,
        client_secret_env: String,
        token_url: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_ir() -> WorkflowIr {
        WorkflowIr {
            workflow_id: "test_workflow".into(),
            version: "0.1.0".into(),
            name: None,
            description: None,
            state_schema: "schemas.TestState".into(),
            start_node: "start".into(),
            nodes: HashMap::new(),
            edges: vec![],
            retry_policies: HashMap::new(),
            timeouts: TimeoutConfig::default(),
            models: HashMap::new(),
            tools: HashMap::new(),
            mcp_servers: HashMap::new(),
            remote_agents: HashMap::new(),
            labels: HashMap::new(),
        }
    }

    #[test]
    fn ir_roundtrip_json() {
        let ir = minimal_ir();
        let json = ir.to_json_pretty().unwrap();
        let parsed = WorkflowIr::from_json(&json).unwrap();
        assert_eq!(parsed.workflow_id, ir.workflow_id);
    }
}
