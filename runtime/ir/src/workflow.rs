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
    /// Workflow-level policy set (overrides global; node-level overrides this).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicySetIr>,
    /// Per-execution token budget enforcement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<TokenBudgetIr>,
    /// Per-execution cost cap in USD. Execution fails / branches when exceeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_budget_usd: Option<f64>,
    /// Node to route to when any budget is exceeded (optional).
    /// If absent, the execution fails with `WorkflowFailed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_budget_exceeded: Option<String>,
    /// If true, when a budget ceiling is hit the execution rolls over to a new
    /// segment (continue-as-new) instead of failing. Default: false.
    #[serde(default)]
    pub continue_as_new: bool,
    /// Data handling policy (PII redaction + retention controls).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_policy: Option<DataPolicyIr>,
    /// The registered agent (its Agent Card `uri`) this workflow acts for.
    /// Every node is then held to that card's `constraints.allowed_tools`, in
    /// addition to the policy chain. Unregistered or unreadable: the node is
    /// refused (see `jamjet_worker::node_guard`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_ref: Option<String>,
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
    /// Node-level policy override (most specific — overrides workflow + global).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicySetIr>,
    /// Node-level data policy override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_policy: Option<DataPolicyIr>,
    /// The registered agent (its Agent Card `uri`) this node acts for, in
    /// addition to the workflow's `agent_ref`: the node must be allowed by
    /// the `allowed_tools` of every agent it is attributed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_ref: Option<String>,
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

/// Policy rules embedded in the workflow IR.
///
/// This is the serializable form of `PolicySet` — it lives in the IR so that
/// workflow YAML/JSON can declare policy inline. The policy engine converts
/// `PolicySetIr` → internal `PolicySet` at evaluation time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicySetIr {
    /// Exact tool names or glob patterns to block (e.g. `"payments.*"`).
    #[serde(default)]
    pub blocked_tools: Vec<String>,
    /// Tool names/patterns that require human approval before execution.
    #[serde(default)]
    pub require_approval_for: Vec<String>,
    /// If non-empty, only models in this list are allowed for model nodes.
    #[serde(default)]
    pub model_allowlist: Vec<String>,
    /// How long an approval request this policy raises stays open, in
    /// seconds. When it passes, the request expires: the engine records
    /// `ApprovalExpired` and treats it as a rejection by `system:deadline`.
    /// Never an approval. `None`: no deadline at this level. Every level is a
    /// ceiling: the shortest timeout any of tenant, workflow and node sets
    /// applies, so a workflow or node can only shorten the tenant's. See
    /// [`ApprovalDeadline::resolve`].
    ///
    /// Skipped when unset, so a policy that does not use it serializes as it
    /// did before: a node's definition hash, which an approval binds, covers
    /// its policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_timeout_secs: Option<u64>,
    /// When to escalate a still-pending request, in seconds after it was
    /// raised: the engine records `ApprovalEscalated` once, for notifiers.
    /// Requires `approval_timeout_secs` at the same level, and must be shorter.
    /// The shortest escalation any level sets applies, and is dropped if it
    /// would not come before the effective timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate_after_secs: Option<u64>,
}

/// The longest approval deadline a policy may set: one year. Bounded so that
/// `now + timeout` cannot overflow and a typo cannot park a request forever.
pub const MAX_APPROVAL_TIMEOUT_SECS: u64 = 365 * 24 * 60 * 60;

/// The deadline an approval request is raised with, resolved from a policy
/// chain by [`ApprovalDeadline::resolve`]. `escalate_after_secs`, when set, is
/// always less than `timeout_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalDeadline {
    pub timeout_secs: u64,
    pub escalate_after_secs: Option<u64>,
}

impl PolicySetIr {
    /// Check this level's deadline settings: `approval_timeout_secs` is 1 to
    /// [`MAX_APPROVAL_TIMEOUT_SECS`]; `escalate_after_secs` needs a timeout at
    /// the same level and is at least 1 and shorter than it. A pair is
    /// checked together because the chain resolves it together.
    pub fn validate_approval_deadline(&self) -> Result<(), String> {
        match (self.approval_timeout_secs, self.escalate_after_secs) {
            (None, None) => Ok(()),
            (None, Some(_)) => Err(
                "escalate_after_secs needs approval_timeout_secs in the same policy".to_string(),
            ),
            (Some(t), escalate) => {
                if !(1..=MAX_APPROVAL_TIMEOUT_SECS).contains(&t) {
                    return Err(format!(
                        "approval_timeout_secs must be between 1 and {MAX_APPROVAL_TIMEOUT_SECS}, got {t}"
                    ));
                }
                match escalate {
                    Some(e) if !(1..t).contains(&e) => Err(format!(
                        "escalate_after_secs must be at least 1 and less than \
                         approval_timeout_secs ({t}), got {e}"
                    )),
                    _ => Ok(()),
                }
            }
        }
    }
}

/// What [`ApprovalDeadline::resolve`] found for a policy chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineResolution {
    /// The deadline a request raised under the chain carries, if any level
    /// sets one.
    pub deadline: Option<ApprovalDeadline>,
    /// An escalation some level asked for that would not come before the
    /// effective timeout, so it was dropped. The caller logs it.
    pub dropped_escalation_secs: Option<u64>,
}

impl ApprovalDeadline {
    /// The deadline for a request raised under `sets` (tenant, workflow,
    /// node, in any order). Every level is a CEILING: the effective timeout
    /// is the shortest any level sets, and the effective escalation the
    /// shortest any level sets. A workflow or node policy can therefore only
    /// shorten what the tenant set, never lengthen or drop it. If the
    /// escalation would not come before the timeout, it is dropped (reported
    /// in `dropped_escalation_secs`); the timeout is never extended to fit it.
    ///
    /// A level whose settings fail [`PolicySetIr::validate_approval_deadline`]
    /// is an error, never skipped: skipping it could lengthen the deadline.
    pub fn resolve(sets: &[&PolicySetIr]) -> Result<DeadlineResolution, String> {
        for set in sets {
            set.validate_approval_deadline()?;
        }
        let Some(timeout_secs) = sets.iter().filter_map(|s| s.approval_timeout_secs).min() else {
            return Ok(DeadlineResolution {
                deadline: None,
                dropped_escalation_secs: None,
            });
        };
        let escalation = sets.iter().filter_map(|s| s.escalate_after_secs).min();
        let (escalate_after_secs, dropped_escalation_secs) = match escalation {
            Some(e) if e < timeout_secs => (Some(e), None),
            Some(e) => (None, Some(e)),
            None => (None, None),
        };
        Ok(DeadlineResolution {
            deadline: Some(ApprovalDeadline {
                timeout_secs,
                escalate_after_secs,
            }),
            dropped_escalation_secs,
        })
    }
}

/// Per-execution token budget configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudgetIr {
    /// Maximum input tokens allowed for the full execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    /// Maximum output tokens allowed for the full execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
    /// Maximum combined input + output tokens for the full execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
}

/// Data handling policy — controls PII redaction and prompt/output retention.
///
/// Applied at workflow level and overridable per-node (same layering as `PolicySetIr`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataPolicyIr {
    /// JSON path patterns that contain PII and must be redacted.
    /// Examples: `"$.patient.ssn"`, `"$.user.email"`
    #[serde(default)]
    pub pii_fields: Vec<String>,

    /// Built-in PII pattern detectors to enable.
    /// Values: `"email"`, `"ssn"`, `"credit_card"`, `"phone"`, `"ip_address"`
    #[serde(default)]
    pub pii_detectors: Vec<String>,

    /// How to redact PII values. Default: `"mask"`.
    /// Options: `"mask"` (`[REDACTED]`), `"hash"` (SHA-256), `"remove"` (delete key).
    #[serde(default = "default_redaction_mode")]
    pub redaction_mode: String,

    /// Whether to store prompts in the event/audit log. Default: false.
    #[serde(default)]
    pub retain_prompts: bool,

    /// Whether to store model outputs/completions. Default: true.
    #[serde(default = "default_true")]
    pub retain_outputs: bool,

    /// Retention duration for audit entries in days. None = indefinite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

fn default_redaction_mode() -> String {
    "mask".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for DataPolicyIr {
    fn default() -> Self {
        Self {
            pii_fields: Vec::new(),
            pii_detectors: Vec::new(),
            redaction_mode: default_redaction_mode(),
            retain_prompts: false,
            retain_outputs: true,
            retention_days: None,
        }
    }
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
            policy: None,
            token_budget: None,
            cost_budget_usd: None,
            on_budget_exceeded: None,
            continue_as_new: false,
            data_policy: None,
            agent_ref: None,
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

    /// T3-5: verify that a WorkflowIr JSON carrying all governance fields emitted
    /// by compile_agent_to_ir (Python) deserializes correctly into the Rust struct.
    /// Field names, types, and nesting must match exactly — a mismatch here means
    /// the Python emitter and the Rust deserializer are out of sync.
    #[test]
    fn governance_fields_deserialize_from_python_shape() {
        // This JSON mirrors the exact output of _compile_governance_ir() in
        // sdk/python/jamjet/compiler/agent_ir.py (T3-5).
        let json = r#"{
            "workflow_id": "governed_agent",
            "version": "0.1.0+abc123456789",
            "name": "governed_agent",
            "description": "Durable agent: governed_agent",
            "state_schema": "",
            "start_node": "start",
            "nodes": {},
            "edges": [],
            "retry_policies": {},
            "timeouts": {},
            "models": {},
            "tools": {},
            "mcp_servers": {},
            "remote_agents": {},
            "labels": {},
            "policy": {
                "blocked_tools": [],
                "require_approval_for": ["delete_*"],
                "model_allowlist": []
            },
            "token_budget": {
                "total_tokens": 1000
            },
            "cost_budget_usd": 0.5,
            "data_policy": {
                "pii_fields": [],
                "pii_detectors": ["email", "ssn", "credit_card", "phone", "ip_address"],
                "redaction_mode": "mask",
                "retain_prompts": false,
                "retain_outputs": true
            }
        }"#;

        let ir = WorkflowIr::from_json(json)
            .expect("governance fields emitted by Python must deserialize into WorkflowIr");

        // Policy / approval
        let policy = ir.policy.expect("policy must be Some");
        assert_eq!(policy.require_approval_for, vec!["delete_*"]);
        assert!(policy.blocked_tools.is_empty());
        assert!(policy.model_allowlist.is_empty());

        // Token budget
        let token_budget = ir.token_budget.expect("token_budget must be Some");
        assert_eq!(token_budget.total_tokens, Some(1000));
        assert!(token_budget.input_tokens.is_none());
        assert!(token_budget.output_tokens.is_none());

        // Cost budget
        let cost = ir.cost_budget_usd.expect("cost_budget_usd must be Some");
        assert!((cost - 0.5_f64).abs() < f64::EPSILON);

        // Data policy / PII
        let dp = ir.data_policy.expect("data_policy must be Some");
        assert!(dp.pii_detectors.contains(&"email".to_string()));
        assert!(dp.pii_detectors.contains(&"ssn".to_string()));
        assert!(dp.pii_detectors.contains(&"credit_card".to_string()));
        assert!(dp.pii_detectors.contains(&"phone".to_string()));
        assert!(dp.pii_detectors.contains(&"ip_address".to_string()));
        assert_eq!(dp.redaction_mode, "mask");
        assert!(!dp.retain_prompts);
        assert!(dp.retain_outputs);
    }

    /// approval_required=True (all tools) compiles to require_approval_for=["*"].
    /// This shape must deserialize and the wildcard must be preserved.
    #[test]
    fn approval_wildcard_deserializes() {
        let json = r#"{
            "workflow_id": "w", "version": "0.1.0", "state_schema": "",
            "start_node": "s", "nodes": {}, "edges": [], "retry_policies": {},
            "timeouts": {}, "models": {}, "tools": {}, "mcp_servers": {},
            "remote_agents": {}, "labels": {},
            "policy": {
                "blocked_tools": [],
                "require_approval_for": ["*"],
                "model_allowlist": []
            }
        }"#;
        let ir = WorkflowIr::from_json(json).expect("must deserialize");
        let policy = ir.policy.unwrap();
        assert_eq!(policy.require_approval_for, vec!["*"]);
    }

    fn deadline(t: Option<u64>, e: Option<u64>) -> PolicySetIr {
        PolicySetIr {
            approval_timeout_secs: t,
            escalate_after_secs: e,
            ..Default::default()
        }
    }

    #[test]
    fn a_deadline_is_validated_as_a_pair() {
        for ok in [
            deadline(None, None),
            deadline(Some(1), None),
            deadline(Some(2), Some(1)),
            deadline(
                Some(MAX_APPROVAL_TIMEOUT_SECS),
                Some(MAX_APPROVAL_TIMEOUT_SECS - 1),
            ),
        ] {
            assert!(ok.validate_approval_deadline().is_ok(), "{ok:?}");
        }
        for bad in [
            deadline(Some(0), None),
            deadline(Some(MAX_APPROVAL_TIMEOUT_SECS + 1), None),
            deadline(None, Some(5)),
            deadline(Some(10), Some(10)),
            deadline(Some(10), Some(11)),
            deadline(Some(10), Some(0)),
        ] {
            assert!(bad.validate_approval_deadline().is_err(), "{bad:?}");
        }
    }

    fn resolved(sets: &[&PolicySetIr]) -> Option<ApprovalDeadline> {
        ApprovalDeadline::resolve(sets).unwrap().deadline
    }

    /// Every level is a ceiling: the shortest timeout and the shortest
    /// escalation, whichever levels set them.
    #[test]
    fn the_shortest_timeout_and_escalation_apply() {
        let tenant = deadline(Some(100), Some(10));
        let workflow = deadline(Some(200), None);
        let node = deadline(Some(MAX_APPROVAL_TIMEOUT_SECS), None);
        assert_eq!(
            resolved(&[&tenant, &workflow, &node]),
            Some(ApprovalDeadline {
                timeout_secs: 100,
                escalate_after_secs: Some(10)
            }),
            "a longer lower level neither lengthens nor drops the tenant's"
        );
        let node = deadline(Some(50), Some(5));
        assert_eq!(
            resolved(&[&tenant, &workflow, &node]),
            Some(ApprovalDeadline {
                timeout_secs: 50,
                escalate_after_secs: Some(5)
            })
        );
        assert_eq!(resolved(&[&deadline(None, None)]), None);
        assert_eq!(resolved(&[]), None);
    }

    /// An escalation that would not come before the effective timeout is
    /// dropped and reported; the timeout is never extended to fit it.
    #[test]
    fn an_escalation_past_the_effective_timeout_is_dropped() {
        let tenant = deadline(Some(100), Some(90));
        let node = deadline(Some(30), None);
        let r = ApprovalDeadline::resolve(&[&tenant, &node]).unwrap();
        assert_eq!(
            r.deadline,
            Some(ApprovalDeadline {
                timeout_secs: 30,
                escalate_after_secs: None
            })
        );
        assert_eq!(r.dropped_escalation_secs, Some(90));
        // Exactly at the timeout is not before it.
        let r =
            ApprovalDeadline::resolve(&[&deadline(Some(100), Some(30)), &deadline(Some(30), None)])
                .unwrap();
        assert_eq!(r.dropped_escalation_secs, Some(30));
    }

    /// An invalid level is an error even when a more specific level would
    /// win: never skipped, never a fallback.
    #[test]
    fn an_invalid_level_fails_resolution() {
        let bad = deadline(Some(0), None);
        let node = deadline(Some(300), None);
        assert!(ApprovalDeadline::resolve(&[&bad, &node]).is_err());
        assert!(ApprovalDeadline::resolve(&[&node, &deadline(Some(5), Some(9))]).is_err());
    }

    /// An unset deadline is not serialized, so a node definition (and the
    /// definition hash an approval binds) is unchanged by the new fields.
    #[test]
    fn an_unset_deadline_does_not_change_a_serialized_policy() {
        let p = PolicySetIr {
            require_approval_for: vec!["*".into()],
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            serde_json::json!({
                "blocked_tools": [], "require_approval_for": ["*"], "model_allowlist": []
            })
        );
    }
}
