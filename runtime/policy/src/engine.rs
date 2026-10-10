//! Helpers for building `EvaluationContext` from runtime node data.

use crate::EvaluationContext;
use jamjet_core::node::NodeKind;

impl EvaluationContext {
    /// Derive an `EvaluationContext` from a `NodeKind`.
    ///
    /// Extracts the tool name (for tool/mcp_tool/a2a_task nodes, and the
    /// handler reference for python_fn `module:function`, java_fn
    /// `class/method` and external_fn `queue/handler` nodes) and the model
    /// reference (for model nodes) so the policy engine can apply rules.
    pub fn from_node_kind(node_id: &str, kind: &NodeKind) -> Self {
        let kind_tag = node_kind_tag(kind);
        let (tool_name, model_ref) = extract_names(kind);
        Self {
            node_id: node_id.to_string(),
            node_kind_tag: kind_tag,
            tool_name,
            model_ref,
        }
    }
}

/// Map a `NodeKind` to its serde tag string.
pub fn node_kind_tag(kind: &NodeKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Extract (tool_name, model_ref) from a `NodeKind`.
fn extract_names(kind: &NodeKind) -> (Option<String>, Option<String>) {
    let v = match serde_json::to_value(kind) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let kind_tag = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind_tag {
        "tool" => {
            let tool = v
                .get("tool_ref")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());
            (tool, None)
        }
        // The kind's field is `tool`. This read `tool_name`, which the kind
        // does not have, so an mcp_tool node had no name and no pattern
        // could gate or block it.
        "mcp_tool" => {
            let tool = v
                .get("tool")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());
            (tool, None)
        }
        "a2a_task" => {
            let tool = v
                .get("skill")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());
            (tool, None)
        }
        "model" => {
            let model = v
                .get("model_ref")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());
            (None, model)
        }
        // Code-backed kinds have no tool name of their own. Their handler
        // reference stands in for one, so `require_approval_for` and
        // `blocked_tools` can name the code a node runs. The node id is a
        // second name for these kinds (see `EvaluationContext::policy_names`).
        "python_fn" => {
            let module = v.get("module").and_then(|t| t.as_str()).unwrap_or("");
            let function = v.get("function").and_then(|t| t.as_str()).unwrap_or("");
            (Some(format!("{module}:{function}")), None)
        }
        "java_fn" => {
            let class = v.get("class_name").and_then(|t| t.as_str()).unwrap_or("");
            let method = v.get("method").and_then(|t| t.as_str()).unwrap_or("");
            (Some(format!("{class}/{method}")), None)
        }
        "external_fn" => {
            let queue = v.get("queue").and_then(|t| t.as_str()).unwrap_or("");
            let handler = v.get("handler").and_then(|t| t.as_str()).unwrap_or("");
            (Some(format!("{queue}/{handler}")), None)
        }
        "agent" => {
            // agent nodes reference a registered agent — the agent ref acts as the "tool" name
            let agent = v
                .get("agent_ref")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());
            (agent, None)
        }
        _ => (None, None),
    }
}
