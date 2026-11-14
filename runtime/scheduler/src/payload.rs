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
        NodeKind::ExternalFn { handler, claim, .. } => {
            obj.insert("handler".into(), Value::String(handler.clone()));
            obj.insert("input".into(), Value::Object(state.clone()));
            // The claim queries read this to keep the item away from the
            // oldest-first claim. Only written for the opt-in, so other
            // payloads (and the digests that bind them) are unchanged.
            if *claim == jamjet_core::node::ClaimMode::ByExecution {
                obj.insert(
                    jamjet_core::node::CLAIM_PAYLOAD_KEY.into(),
                    Value::String(jamjet_core::node::CLAIM_BY_EXECUTION.into()),
                );
            }
        }
        // The model worker needs the resolved model id, optional config
        // overrides, the node's prompt rendered against the run's state, its
        // system prompt, the tool schemas the model may call, and the running
        // conversation so each turn sees the history.
        NodeKind::Model {
            model_ref,
            prompt_ref,
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
            // A node with a prompt sends the prompt alone, so the payload,
            // which is what an approval binds and a reviewer is shown, does
            // not carry a conversation the model would not see.
            match model_prompt(prompt_ref, state) {
                ModelPrompt::Text(text) => {
                    obj.insert("prompt".into(), Value::String(text));
                }
                ModelPrompt::Unavailable(reason) => {
                    obj.insert("prompt_error".into(), Value::String(reason));
                }
                ModelPrompt::None => {
                    if let Some(messages) = state.get("messages") {
                        obj.insert("messages".into(), messages.clone());
                    }
                }
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

/// What a model node's `prompt_ref` puts on its work item.
enum ModelPrompt {
    /// The prompt, rendered against the run's state: sent as `prompt`.
    Text(String),
    /// The node has a prompt the runtime cannot produce: sent as
    /// `prompt_error`, and the model executor fails the node with it rather
    /// than calling the model without the prompt the author wrote.
    Unavailable(String),
    /// No prompt: the model reads the conversation in `messages` (the agent
    /// loop compiles its model nodes with an empty `prompt_ref`).
    None,
}

/// Resolve a model node's `prompt_ref` against the run's `state`.
///
/// Until 2026-11 nothing read `prompt_ref`: the payload carried only
/// `messages`, so a YAML model node with a `prompt:` failed with "no prompt or
/// messages provided", or, with `messages` in state, ran without its prompt.
fn model_prompt(prompt_ref: &str, state: &Map<String, Value>) -> ModelPrompt {
    if prompt_ref.trim().is_empty() {
        return ModelPrompt::None;
    }
    if jamjet_core::template::is_prompt_file_ref(prompt_ref) {
        // The compilers write `prompts/<node>.md` when the author gave no
        // inline prompt. A node that reads the conversation keeps doing so;
        // one with neither fails with the reason instead of the bare
        // "no prompt or messages".
        if state.contains_key("messages") {
            return ModelPrompt::None;
        }
        return ModelPrompt::Unavailable(format!(
            "prompt_ref `{}` names a prompt file, which the runtime cannot read (it stays on \
             the machine that compiled the workflow); write the prompt text inline under \
             `prompt:`",
            prompt_ref.trim()
        ));
    }
    match jamjet_core::template::render_template(prompt_ref, state) {
        // An empty prompt is not "no prompt": falling back to the
        // conversation would send the model something the author did not ask.
        Ok(text) if text.trim().is_empty() => ModelPrompt::Unavailable(
            "prompt template: the prompt rendered empty (every value it reads is empty)"
                .to_string(),
        ),
        Ok(text) => ModelPrompt::Text(text),
        Err(reason) => ModelPrompt::Unavailable(format!("prompt template: {reason}")),
    }
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
            json!({ "type": "model", "model_ref": "m", "prompt_ref": "", "output_schema": "", "system_prompt": "be brief", "tools": [ { "name": "t" } ] }),
        ));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state(), "wf", "0.1.0");
        assert_eq!(p["model"], "anthropic/claude-sonnet-4-6");
        assert_eq!(p["temperature"], 0.5);
        assert_eq!(p["max_tokens"], 100);
        assert_eq!(p["system_prompt"], "be brief");
        assert_eq!(p["tools"][0]["name"], "t");
        assert_eq!(p["messages"][0]["content"], "hi");
    }

    fn model_node(prompt_ref: &str) -> Value {
        node(
            json!({ "type": "model", "model_ref": "m", "prompt_ref": prompt_ref, "output_schema": "", "system_prompt": null }),
        )
    }

    /// The YAML hello world: a model node whose `prompt:` reads the run's
    /// input. The prompt reaches the payload with the state filled in.
    #[test]
    fn model_carries_its_prompt_rendered_against_state() {
        let ir = ir_with(model_node(
            "Answer this question clearly and concisely:\n\n{{ state.query }}\n",
        ));
        let state = json!({ "query": "What is JamJet?" })
            .as_object()
            .cloned()
            .unwrap();
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state, "wf", "0.1.0");
        assert_eq!(
            p["prompt"],
            "Answer this question clearly and concisely:\n\nWhat is JamJet?\n"
        );
        assert!(p.get("prompt_error").is_none());
        assert!(p.get("messages").is_none(), "no conversation in state");
    }

    /// A node with a prompt sends the prompt alone: the payload (what an
    /// approval binds) carries no conversation the model would not see.
    #[test]
    fn a_prompted_model_does_not_carry_the_conversation() {
        let ir = ir_with(model_node("Summarize: {{ state.key }}"));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state(), "wf", "0.1.0");
        assert_eq!(p["prompt"], "Summarize: value");
        assert!(p.get("messages").is_none(), "{p}");
    }

    /// A prompt that renders to nothing is an error, not "no prompt": the
    /// conversation is not sent in its place.
    #[test]
    fn a_prompt_that_renders_empty_is_an_error() {
        let ir = ir_with(model_node("{{ state.query }}"));
        let mut s = state();
        s.insert("query".into(), json!("  "));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &s, "wf", "0.1.0");
        assert!(p.get("prompt").is_none());
        assert!(p.get("messages").is_none());
        assert!(p["prompt_error"]
            .as_str()
            .unwrap()
            .contains("rendered empty"));
    }

    /// A prompt that cannot be rendered is carried as `prompt_error`, never
    /// as a prompt with a hole in it.
    #[test]
    fn model_prompt_naming_a_missing_key_carries_the_error_not_a_prompt() {
        let ir = ir_with(model_node("Q: {{ state.query }}"));
        let state = json!({ "question": "x" }).as_object().cloned().unwrap();
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state, "wf", "0.1.0");
        assert!(p.get("prompt").is_none());
        let err = p["prompt_error"].as_str().unwrap();
        assert!(err.contains("no `query`"), "{err}");
    }

    /// An empty `prompt_ref` (the agent loop) reads the conversation only.
    #[test]
    fn model_with_an_empty_prompt_ref_carries_only_the_conversation() {
        let ir = ir_with(model_node(""));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state(), "wf", "0.1.0");
        assert!(p.get("prompt").is_none());
        assert!(p.get("prompt_error").is_none());
        assert_eq!(p["messages"][0]["content"], "hi");
    }

    /// A prompt FILE reference cannot be read by the runtime. With a
    /// conversation in state the node reads that, as before; without one it
    /// carries the reason.
    #[test]
    fn model_prompt_file_reference_is_never_sent_as_the_prompt() {
        let ir = ir_with(model_node("prompts/summarize.md"));
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &state(), "wf", "0.1.0");
        assert!(p.get("prompt").is_none());
        assert!(p.get("prompt_error").is_none());
        assert_eq!(p["messages"][0]["content"], "hi");

        let empty = Map::new();
        let p = build_work_item_payload(&ir, "n", &ir.nodes["n"], &empty, "wf", "0.1.0");
        assert!(p.get("prompt").is_none());
        let err = p["prompt_error"].as_str().unwrap();
        assert!(err.contains("prompts/summarize.md"), "{err}");
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
