//! Policy evaluation for ADK agent tool-dispatch nodes.
//!
//! An ADK agent compiles a whole turn's tool calls into one `python_fn` /
//! `java_fn` node, so the tool names are absent from the static node kind and
//! the ordinary [`crate::EvaluationContext::from_node_kind`] path sees nothing
//! to police (review finding C1). The scheduler freezes the accumulated state
//! into the work-item payload before the worker runs, so the pending calls are
//! readable here — the same bytes the dispatch will consume, which is what
//! makes this free of a time-of-check-to-time-of-use window.

use serde_json::Value;

/// One tool call an agent dispatch node is about to execute.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingToolCall {
    pub name: String,
    pub arguments: Value,
}

/// Read the tool calls a marked agent-dispatch node will execute from its
/// frozen work-item input.
///
/// The key order MUST stay identical to `dispatch_tool_calls` in
/// `sdk/python/jamjet/agents/tool_runtime.py:48-50`: `tool_calls` first (unless
/// null/absent), then `last_model_tool_calls`. If the two sides ever disagree,
/// policy evaluates a different list than the one that executes.
///
/// Returns `None` when the calls are present but not well-formed. Callers MUST
/// treat `None` as a block. `Some(vec![])` means there is genuinely nothing to
/// run and is safe to allow.
pub fn pending_tool_calls(input: &Value) -> Option<Vec<PendingToolCall>> {
    let raw = match input.get("tool_calls") {
        Some(v) if !v.is_null() => v,
        _ => match input.get("last_model_tool_calls") {
            Some(v) if !v.is_null() => v,
            // Neither key present: the dispatch has nothing to execute.
            _ => return Some(Vec::new()),
        },
    };

    let arr = raw.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for call in arr {
        // A call whose name cannot be read cannot be matched against
        // blocked_tools or require_approval_for, so it must not run.
        let name = call.get("name")?.as_str()?.to_string();
        let arguments = call.get("arguments").cloned().unwrap_or(Value::Null);
        out.push(PendingToolCall { name, arguments });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str) -> serde_json::Value {
        json!({"id": "c1", "name": name, "arguments": {"x": 1}})
    }

    #[test]
    fn reads_tool_calls_key_first() {
        // Mirrors tool_runtime.py:48-50 — `tool_calls` wins when present.
        let input = json!({
            "tool_calls": [call("send_wire")],
            "last_model_tool_calls": [call("read_only")],
        });
        let calls = pending_tool_calls(&input).expect("readable");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "send_wire");
    }

    #[test]
    fn falls_back_to_last_model_tool_calls() {
        let input = json!({"last_model_tool_calls": [call("send_wire")]});
        let calls = pending_tool_calls(&input).expect("readable");
        assert_eq!(calls[0].name, "send_wire");
    }

    #[test]
    fn null_tool_calls_falls_back() {
        // Python: `input.get("tool_calls")` returning None triggers the fallback.
        let input = json!({
            "tool_calls": null,
            "last_model_tool_calls": [call("send_wire")],
        });
        let calls = pending_tool_calls(&input).expect("readable");
        assert_eq!(calls[0].name, "send_wire");
    }

    #[test]
    fn empty_tool_calls_list_is_not_a_fallback() {
        // Python uses an explicit `is None` check, so an empty list stays empty
        // rather than falling through to last_model_tool_calls.
        let input = json!({
            "tool_calls": [],
            "last_model_tool_calls": [call("send_wire")],
        });
        let calls = pending_tool_calls(&input).expect("readable");
        assert!(calls.is_empty());
    }

    #[test]
    fn both_absent_means_no_calls() {
        let calls = pending_tool_calls(&json!({})).expect("readable");
        assert!(calls.is_empty());
    }

    #[test]
    fn non_array_is_unreadable() {
        let input = json!({"tool_calls": "send_wire"});
        assert!(pending_tool_calls(&input).is_none());
    }

    #[test]
    fn call_without_a_name_is_unreadable() {
        // Fail closed: a call we cannot name is a call we cannot police.
        let input = json!({"tool_calls": [{"id": "c1", "arguments": {}}]});
        assert!(pending_tool_calls(&input).is_none());
    }

    #[test]
    fn missing_arguments_defaults_to_null() {
        let input = json!({"tool_calls": [{"id": "c1", "name": "t"}]});
        let calls = pending_tool_calls(&input).expect("readable");
        assert_eq!(calls[0].arguments, serde_json::Value::Null);
    }
}
