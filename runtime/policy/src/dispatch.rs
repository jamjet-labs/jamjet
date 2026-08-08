//! Policy evaluation for ADK agent tool-dispatch nodes.
//!
//! An ADK agent compiles a whole turn's tool calls into one `python_fn` /
//! `java_fn` node, so the tool names are absent from the static node kind and
//! the ordinary [`crate::EvaluationContext::from_node_kind`] path sees nothing
//! to police (review finding C1). The scheduler freezes the accumulated state
//! into the work-item payload before the worker runs, so the pending calls are
//! readable here — the same bytes the dispatch will consume, which is what
//! makes this free of a time-of-check-to-time-of-use window.

use crate::{EvaluationContext, PolicyDecision, PolicyEvaluator};
use jamjet_ir::workflow::PolicySetIr;
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
/// Returns `None` when the input is not an object, or when the calls are present
/// but not well-formed. Callers MUST treat `None` as a block. `Some(vec![])`
/// means there is genuinely nothing to run and is safe to allow.
pub fn pending_tool_calls(input: &Value) -> Option<Vec<PendingToolCall>> {
    // A non-object input is unreadable, not empty. `Value::get` returns `None`
    // for every non-object variant rather than only for a missing key, so
    // without this check a non-object would fall through both arms below and
    // read as "nothing to run" — allowing the whole dispatch unpoliced. The
    // live case: `payload["input"]` yields `Value::Null` when the key is absent
    // or misspelled, because `Index for Value` returns null instead of panicking.
    let obj = input.as_object()?;

    let raw = match obj.get("tool_calls") {
        Some(v) if !v.is_null() => v,
        _ => match obj.get("last_model_tool_calls") {
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

/// The aggregate policy outcome for one dispatch batch.
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchDecision {
    Allow,
    Block {
        tool_name: String,
        reason: String,
    },
    RequireApproval {
        approver: String,
        gated: Vec<String>,
    },
}

/// Evaluate every pending call and collapse the results to one decision.
///
/// Precedence is fail-closed: any Block blocks the batch, otherwise any
/// RequireApproval holds the batch, otherwise Allow. The first block wins and
/// short-circuits, since the outcome for the batch cannot get any stricter.
///
/// Approval is per-batch rather than per-call because `hold_for_approval` keys
/// approval state on `node_id` (`runtime/workers/src/worker.rs:667`). `gated`
/// carries every call that asked for approval so the approver sees all of them.
pub fn evaluate_dispatch(
    node_id: &str,
    calls: &[PendingToolCall],
    policy_sets: &[&PolicySetIr],
) -> DispatchDecision {
    let ev = PolicyEvaluator;
    let mut gated: Vec<String> = Vec::new();
    let mut approver: Option<String> = None;

    for call in calls {
        let ctx = EvaluationContext {
            node_id: node_id.to_string(),
            // "tool", not "python_fn": only the model-allowlist branch keys on
            // the tag (runtime/policy/src/lib.rs:62), and "tool" is what makes
            // blocked_tools / require_approval_for read naturally here.
            node_kind_tag: "tool".to_string(),
            tool_name: Some(call.name.clone()),
            model_ref: None,
        };
        match ev.evaluate(&ctx, policy_sets) {
            PolicyDecision::Block { reason } => {
                return DispatchDecision::Block {
                    tool_name: call.name.clone(),
                    reason,
                }
            }
            PolicyDecision::RequireApproval { approver: a } => {
                gated.push(call.name.clone());
                approver.get_or_insert(a);
            }
            PolicyDecision::Allow => {}
        }
    }

    match approver {
        Some(a) => DispatchDecision::RequireApproval { approver: a, gated },
        None => DispatchDecision::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamjet_ir::workflow::PolicySetIr;
    use serde_json::json;

    fn call(name: &str) -> serde_json::Value {
        json!({"id": "c1", "name": name, "arguments": {"x": 1}})
    }

    fn policy(blocked: &[&str], approval: &[&str]) -> PolicySetIr {
        PolicySetIr {
            blocked_tools: blocked.iter().map(|s| s.to_string()).collect(),
            require_approval_for: approval.iter().map(|s| s.to_string()).collect(),
            model_allowlist: vec![],
        }
    }

    fn pending(names: &[&str]) -> Vec<PendingToolCall> {
        names
            .iter()
            .map(|n| PendingToolCall {
                name: n.to_string(),
                arguments: json!({}),
            })
            .collect()
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
    fn non_object_input_is_unreadable() {
        // `Value::get` returns None for EVERY non-object variant, not just for a
        // missing key, so without an explicit object check these would read as
        // "nothing to run" and allow the dispatch through unpoliced. The null
        // case is the live one: `payload["input"]` yields Value::Null when the
        // key is absent or misspelled.
        assert!(pending_tool_calls(&serde_json::Value::Null).is_none());
        assert!(pending_tool_calls(&json!("send_wire")).is_none());
        assert!(pending_tool_calls(&json!([call("send_wire")])).is_none());
    }

    #[test]
    fn call_with_a_non_string_name_is_unreadable() {
        // Stringifying instead of rejecting would yield the tool name "123",
        // which quietly fails to match blocked_tools — a fail-open.
        assert!(pending_tool_calls(&json!({"tool_calls": [{"name": 123}]})).is_none());
    }

    #[test]
    fn null_call_element_is_unreadable() {
        assert!(pending_tool_calls(&json!({"tool_calls": [null]})).is_none());
    }

    #[test]
    fn missing_arguments_defaults_to_null() {
        let input = json!({"tool_calls": [{"id": "c1", "name": "t"}]});
        let calls = pending_tool_calls(&input).expect("readable");
        assert_eq!(calls[0].arguments, serde_json::Value::Null);
    }

    #[test]
    fn ungated_batch_is_allowed() {
        let p = policy(&[], &[]);
        let d = evaluate_dispatch("__tools_0__", &pending(&["read_file"]), &[&p]);
        assert_eq!(d, DispatchDecision::Allow);
    }

    #[test]
    fn blocked_tool_blocks_the_batch() {
        let p = policy(&["send_wire"], &[]);
        let d = evaluate_dispatch("__tools_0__", &pending(&["read_file", "send_wire"]), &[&p]);
        match d {
            DispatchDecision::Block { tool_name, .. } => assert_eq!(tool_name, "send_wire"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn approval_tool_holds_the_batch() {
        let p = policy(&[], &["send_wire"]);
        let d = evaluate_dispatch("__tools_0__", &pending(&["send_wire"]), &[&p]);
        match d {
            DispatchDecision::RequireApproval { gated, .. } => assert_eq!(gated, vec!["send_wire"]),
            other => panic!("expected RequireApproval, got {other:?}"),
        }
    }

    #[test]
    fn one_gated_call_holds_the_whole_batch() {
        // Documented limitation: approval is per-batch, because hold_for_approval
        // keys approval state on node_id.
        let p = policy(&[], &["send_wire"]);
        let d = evaluate_dispatch(
            "__tools_0__",
            &pending(&["read_file", "send_wire", "log"]),
            &[&p],
        );
        match d {
            DispatchDecision::RequireApproval { gated, .. } => assert_eq!(gated, vec!["send_wire"]),
            other => panic!("expected RequireApproval, got {other:?}"),
        }
    }

    #[test]
    fn every_gated_call_is_reported_to_the_approver() {
        // `gated` is what a human approver is shown as the tools they are
        // authorising. A truncated list means approving `send_wire` silently
        // releases `wire_batch` too, so membership AND order are load-bearing.
        // The ungated call in the middle also re-proves the filter.
        let p = policy(&[], &["send_wire", "wire_batch"]);
        let d = evaluate_dispatch(
            "__tools_0__",
            &pending(&["send_wire", "read_file", "wire_batch"]),
            &[&p],
        );
        match d {
            DispatchDecision::RequireApproval { gated, .. } => {
                assert_eq!(gated, vec!["send_wire", "wire_batch"])
            }
            other => panic!("expected RequireApproval, got {other:?}"),
        }
    }

    #[test]
    fn block_wins_over_approval() {
        // Fail closed: a batch containing both must never merely hold.
        let p = policy(&["drop_table"], &["send_wire"]);
        let d = evaluate_dispatch("__tools_0__", &pending(&["send_wire", "drop_table"]), &[&p]);
        assert!(matches!(d, DispatchDecision::Block { .. }));
    }

    #[test]
    fn glob_patterns_match_per_call() {
        let p = policy(&["admin_*"], &[]);
        let d = evaluate_dispatch("__tools_0__", &pending(&["admin_delete"]), &[&p]);
        assert!(matches!(d, DispatchDecision::Block { .. }));
    }

    #[test]
    fn empty_batch_is_allowed() {
        let p = policy(&["send_wire"], &[]);
        assert_eq!(
            evaluate_dispatch("__tools_0__", &[], &[&p]),
            DispatchDecision::Allow
        );
    }

    #[test]
    fn node_policy_overrides_tenant_policy() {
        // Sets are ordered least-specific to most-specific; the evaluator
        // iterates in reverse, so the node layer wins.
        let tenant = policy(&["send_wire"], &[]);
        let node = policy(&[], &["send_wire"]);
        let d = evaluate_dispatch("__tools_0__", &pending(&["send_wire"]), &[&tenant, &node]);
        assert!(matches!(d, DispatchDecision::RequireApproval { .. }));
    }
}
