//! The one place a work item's payload is built.
//!
//! Every node the engine hands to a worker carries the same base fields
//! (`workflow_id`, `workflow_version`, `node_id`) plus kind-specific enrichment:
//! a `python_fn` node needs `module`, `function` and the accumulated state as
//! `input`; a `java_fn` node the same with `class` and `method`; a `model` node
//! its resolved model id, config, tool schemas and message history; a
//! `condition` node its branches and the state they are evaluated against.
//!
//! Until 2026-09-06 this lived inline in the scheduler's dispatch loop, and
//! `POST /executions` built the START node's item by hand with only the two
//! workflow fields. A workflow whose first step was a Python function therefore
//! failed three times with "payload missing module or function" and died.
//! Both paths now call [`build_work_item_payload`], so the start node cannot
//! drift from every later node again.

use jamjet_core::node::NodeKind;
use jamjet_ir::workflow::{NodeDef, WorkflowIr};
use serde_json::{json, Map, Value};

/// Build the payload for dispatching `node` of `ir` with the accumulated
/// workflow `state` (the initial input for the start node, every completed
/// node's `state_patch` merged in for later ones).
pub fn build_work_item_payload(
    ir: &WorkflowIr,
    node_id: &str,
    node: &NodeDef,
    state: &Map<String, Value>,
    workflow_id: &str,
    workflow_version: &str,
) -> Value {
    let mut payload = json!({
        "workflow_id": workflow_id,
        "workflow_version": workflow_version,
        "node_id": node_id,
    });
    let obj = payload
        .as_object_mut()
        .expect("payload is always a JSON object");

    match &node.kind {
        // The external Python worker is self-contained: the module, the
        // function to invoke, and the current state as the call input.
        NodeKind::PythonFn {
            module, function, ..
        } => {
            obj.insert("module".into(), Value::String(module.clone()));
            obj.insert("function".into(), Value::String(function.clone()));
            obj.insert("input".into(), Value::Object(state.clone()));
        }
        // The external Java tool-worker mirrors the Python arm with its own
        // dispatch coordinates, over the same claim/complete API.
        NodeKind::JavaFn {
            class_name, method, ..
        } => {
            obj.insert("class".into(), Value::String(class_name.clone()));
            obj.insert("method".into(), Value::String(method.clone()));
            obj.insert("input".into(), Value::Object(state.clone()));
        }
        // Any external worker: the handler name it resolves itself, and the
        // state as input. The queue is routing, so it stays off the payload.
        NodeKind::ExternalFn { handler, .. } => {
            obj.insert("handler".into(), Value::String(handler.clone()));
            obj.insert("input".into(), Value::Object(state.clone()));
        }
        // The model worker needs the resolved model id, optional config
        // overrides, the node's system prompt, the tool schemas the model may
        // call, and the running conversation so each turn sees the history.
        NodeKind::Model {
            model_ref,
            system_prompt,
            tools,
            ..
        } => {
            let model_id = ir
                .models
                .get(model_ref.as_str())
                .map(|cfg| {
                    if cfg.provider.is_empty() {
                        cfg.model.clone()
                    } else {
                        format!("{}/{}", cfg.provider, cfg.model)
                    }
                })
                .unwrap_or_else(|| model_ref.clone());
            obj.insert("model".into(), Value::String(model_id));
            if let Some(cfg) = ir.models.get(model_ref.as_str()) {
                if let Some(temp) = cfg.temperature {
                    obj.insert("temperature".into(), json!(temp));
                }
                if let Some(mt) = cfg.max_tokens {
                    obj.insert("max_tokens".into(), json!(mt));
                }
            }
            if let Some(sp) = system_prompt {
                obj.insert("system_prompt".into(), Value::String(sp.clone()));
            }
            if !tools.is_empty() {
                obj.insert("tools".into(), Value::Array(tools.clone()));
            }
            if let Some(messages) = state.get("messages") {
                obj.insert("messages".into(), messages.clone());
            }
        }
        // The condition executor evaluates each branch against `state` and
        // records the routing decision on NodeCompleted.
        NodeKind::Condition { branches } if !branches.is_empty() => {
            obj.insert(
                "branches".into(),
                serde_json::to_value(branches).unwrap_or(json!([])),
            );
            obj.insert("state".into(), Value::Object(state.clone()));
        }
        // A `human_approval` node carries the run's state as `input`, so the
        // approval request's action digest, and the payload a reviewer is
        // shown, cover the data the run holds when the decision is asked for.
        // If the state moves before the approved node is dispatched again
        // (a parallel branch writes), the digest no longer matches and the
        // guard asks again. Without it the request bound no data at all, and
        // the guarded steps ran on whatever the state had become.
        NodeKind::HumanApproval { .. } => {
            obj.insert("input".into(), Value::Object(state.clone()));
        }
        _ => {}
    }

    payload
}

/// How many attempts a node's work item gets, from its named retry policy.
///
/// One declaration, shared by the scheduler and `POST /executions` (which
/// enqueues the start node itself), so the start node cannot get a different
/// budget from every node after it.
pub fn max_attempts_for(retry_policy: Option<&str>) -> u32 {
    match retry_policy {
        Some("no_retry") => 1,
        Some("io_default") => 5,
        Some("llm_default") => 3,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn named_retry_policies_map_to_their_attempt_budgets() {
        use super::max_attempts_for;
        assert_eq!(max_attempts_for(Some("no_retry")), 1);
        assert_eq!(max_attempts_for(Some("io_default")), 5);
        assert_eq!(max_attempts_for(Some("llm_default")), 3);
        assert_eq!(max_attempts_for(Some("unknown")), 3);
        assert_eq!(max_attempts_for(None), 3);
    }

    use super::*;

    fn ir_with(node_json: Value) -> WorkflowIr {
        serde_json::from_value(json!({
            "workflow_id": "wf", "version": "0.1.0", "name": null, "description": null,
            "state_schema": "", "start_node": "n",
            "nodes": { "n": node_json },
            "edges": [], "retry_policies": {}, "timeouts": {},
            "models": { "m": { "provider": "anthropic", "model": "claude-sonnet-4-6", "temperature": 0.5, "max_tokens": 100 } },
            "tools": {}, "mcp_servers": {}, "remote_agents": {}, "labels": {}
        }))
        .unwrap()
    }

    fn node(kind: Value) -> Value {
        json!({ "id": "n", "kind": kind, "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {} })
    }

    fn state() -> Map<String, Value> {
        json!({ "key": "value", "messages": [ { "role": "user", "content": "hi" } ] })
            .as_object()
            .cloned()
            .unwrap()
    }

    #[test]
    fn python_fn_carries_module_function_and_input() {
        let ir = ir_with(node(
            json!({ "type": "python_fn", "module": "myapp.tools", "function": "run", "output_schema": "" }),
        ));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state(), "wf", "0.1.0");
        assert_eq!(p["node_id"], "n");
        assert_eq!(p["module"], "myapp.tools");
        assert_eq!(p["function"], "run");
        assert_eq!(p["input"]["key"], "value");
    }

    #[test]
    fn java_fn_carries_class_method_and_input() {
        let ir = ir_with(node(
            json!({ "type": "java_fn", "class_name": "com.acme.Check", "method": "run", "output_schema": "" }),
        ));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state(), "wf", "0.1.0");
        assert_eq!(p["class"], "com.acme.Check");
        assert_eq!(p["method"], "run");
        assert_eq!(p["input"]["key"], "value");
    }

    #[test]
    fn model_resolves_id_config_and_threads_messages() {
        let ir = ir_with(node(
            json!({ "type": "model", "model_ref": "m", "prompt_ref": "p", "output_schema": "", "system_prompt": "be brief", "tools": [ { "name": "t" } ] }),
        ));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state(), "wf", "0.1.0");
        assert_eq!(p["model"], "anthropic/claude-sonnet-4-6");
        assert_eq!(p["temperature"], 0.5);
        assert_eq!(p["max_tokens"], 100);
        assert_eq!(p["system_prompt"], "be brief");
        assert_eq!(p["tools"][0]["name"], "t");
        assert_eq!(p["messages"][0]["content"], "hi");
    }

    #[test]
    fn condition_carries_branches_and_state() {
        let ir = ir_with(node(
            json!({ "type": "condition", "branches": [ { "condition": "state.key == \"value\"", "target": "x" } ] }),
        ));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state(), "wf", "0.1.0");
        assert_eq!(p["branches"][0]["target"], "x");
        assert_eq!(p["state"]["key"], "value");
    }

    #[test]
    fn external_fn_carries_handler_and_input() {
        let ir = ir_with(node(
            json!({ "type": "external_fn", "queue": "pas-control", "handler": "authorize", "output_schema": "" }),
        ));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state(), "wf", "0.1.0");
        assert_eq!(p["handler"], "authorize");
        assert_eq!(p["input"]["key"], "value");
        assert!(
            p.get("queue").is_none(),
            "the queue is routing, not payload"
        );
    }

    #[test]
    fn other_kinds_carry_only_the_base_fields() {
        let ir = ir_with(node(json!({ "type": "condition", "branches": [] })));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state(), "wf", "0.1.0");
        assert_eq!(p.as_object().unwrap().len(), 3);
    }

    /// The approval of a `human_approval` node binds the run's state: it is
    /// the node's `input`, which the action digest covers.
    #[test]
    fn human_approval_carries_the_state_its_approval_binds() {
        let ir = ir_with(node(
            json!({ "type": "human_approval", "description": "Pay?", "timeout_secs": null, "fallback_node": null }),
        ));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state(), "wf", "0.1.0");
        assert_eq!(p["input"]["key"], "value");
    }
}
