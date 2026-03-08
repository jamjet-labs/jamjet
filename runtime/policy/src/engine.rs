//! Helpers for building `EvaluationContext` from runtime node data.

use crate::EvaluationContext;
use jamjet_core::node::NodeKind;

impl EvaluationContext {
    /// Derive an `EvaluationContext` from a `NodeKind`.
    ///
    /// Extracts the tool name (for tool/mcp_tool/a2a_task nodes) and model
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
        "mcp_tool" => {
            let tool = v
                .get("tool_name")
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
