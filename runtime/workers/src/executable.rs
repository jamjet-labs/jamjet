//! Which node kinds this runtime can execute, and how.
//!
//! One table, read by registration (`POST /workflows` refuses a workflow with
//! a node no one can run) and by [`crate::pool::server_pool`], which registers
//! an executor for exactly the in-process kinds listed here (a test holds the
//! two to each other).
//!
//! Until 2026-11 a worker that claimed a node with no executor completed it
//! with an empty output ("No executor; using stub"). The server registered
//! three executors, so a `human_approval` node passed without anyone
//! approving it and a `tool` node "succeeded" without running. A worker now
//! fails such a node (`Worker::execute_item`), and registration refuses it.

use jamjet_ir::WorkflowIr;
use jamjet_policy::engine::node_kind_tag;

/// Node kinds the server's embedded worker pool runs in process.
pub const IN_PROCESS_KINDS: &[&str] = &[
    "model",
    "eval",
    "condition",
    "human_approval",
    "limit_exceeded",
];

/// Node kinds an external worker claims over `POST /work-items/claim`: the
/// Python `jamjet worker` (`python_tool`), the Java tool worker (`java_tool`)
/// and any worker draining the queue an `external_fn` node names.
pub const EXTERNAL_WORKER_KINDS: &[&str] = &["python_fn", "java_fn", "external_fn"];

/// The queues the engine's own kinds are dispatched to (`NodeKind::queue_type`
/// of every kind in [`IN_PROCESS_KINDS`]). `POST /work-items/claim` does not
/// serve them: an external worker that took an item from one could report any
/// result for a node the engine runs, such as an approved `human_approval`
/// node with a state patch of its choosing, or a `condition` node with the
/// route it wants. No external kind is dispatched to them (an `external_fn`
/// queue cannot take a built-in name).
pub const ENGINE_QUEUES: &[&str] = &["model", "general"];

/// How this runtime executes a node kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// An executor in the server's embedded worker pool.
    InProcess,
    /// An external worker, over the HTTP claim protocol.
    ExternalWorker,
    /// Nothing in this runtime runs it.
    None,
}

/// How this runtime executes the node kind tagged `kind` (`node_kind_tag`).
pub fn runner_for(kind: &str) -> Runner {
    if IN_PROCESS_KINDS.contains(&kind) {
        Runner::InProcess
    } else if EXTERNAL_WORKER_KINDS.contains(&kind) {
        Runner::ExternalWorker
    } else {
        Runner::None
    }
}

/// How this runtime executes `kind`.
pub fn runner_for_kind(kind: &jamjet_core::node::NodeKind) -> Runner {
    runner_for(&node_kind_tag(kind))
}

/// The nodes of `ir` nothing in this runtime can execute, as `(node id,
/// kind)`, sorted by node id. Empty when every node can run.
pub fn unexecutable_nodes(ir: &WorkflowIr) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = ir
        .nodes
        .iter()
        .map(|(id, node)| (id.clone(), node_kind_tag(&node.kind)))
        .filter(|(_, kind)| runner_for(kind) == Runner::None)
        .collect();
    out.sort();
    out
}

/// The registration refusal for `unexecutable` (from [`unexecutable_nodes`]).
pub fn refusal_message(unexecutable: &[(String, String)]) -> String {
    let nodes = unexecutable
        .iter()
        .map(|(id, kind)| format!("'{id}' ({kind})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "this runtime cannot execute node(s) {nodes}: it has no executor for that kind, so \
         the node would never run. Kinds it runs in process: {}. Kinds external workers run: {}",
        IN_PROCESS_KINDS.join(", "),
        EXTERNAL_WORKER_KINDS.join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every in-process kind is dispatched to an engine queue and no external
    /// kind is, so withholding the engine queues from external claims
    /// withholds exactly the engine's work.
    #[test]
    fn engine_queues_are_exactly_the_in_process_kinds_queues() {
        use jamjet_core::node::NodeKind;
        let kind = |tag: &str| -> NodeKind {
            let v = match tag {
                "model" => {
                    serde_json::json!({ "type": "model", "model_ref": "m", "prompt_ref": "", "output_schema": "" })
                }
                "eval" => {
                    serde_json::json!({ "type": "eval", "scorers": [], "on_fail": "halt", "input_expr": null })
                }
                "condition" => serde_json::json!({ "type": "condition", "branches": [] }),
                "human_approval" => {
                    serde_json::json!({ "type": "human_approval", "description": "", "timeout_secs": null, "fallback_node": null })
                }
                "limit_exceeded" => serde_json::json!({ "type": "limit_exceeded" }),
                "python_fn" => {
                    serde_json::json!({ "type": "python_fn", "module": "m", "function": "f", "output_schema": "" })
                }
                "java_fn" => {
                    serde_json::json!({ "type": "java_fn", "class_name": "C", "method": "m", "output_schema": "" })
                }
                "external_fn" => {
                    serde_json::json!({ "type": "external_fn", "queue": "q", "handler": "h", "output_schema": "" })
                }
                other => panic!("no sample for {other}"),
            };
            serde_json::from_value(v).unwrap()
        };
        for tag in IN_PROCESS_KINDS {
            let q = kind(tag).queue_type();
            assert!(
                ENGINE_QUEUES.contains(&q.as_str()),
                "{tag} goes to {}",
                q.as_str()
            );
        }
        for tag in EXTERNAL_WORKER_KINDS {
            let q = kind(tag).queue_type();
            assert!(
                !ENGINE_QUEUES.contains(&q.as_str()),
                "{tag} goes to {}",
                q.as_str()
            );
        }
    }

    #[test]
    fn every_kind_has_exactly_one_runner() {
        for kind in IN_PROCESS_KINDS {
            assert!(!EXTERNAL_WORKER_KINDS.contains(kind), "{kind} listed twice");
        }
        assert_eq!(runner_for("model"), Runner::InProcess);
        assert_eq!(runner_for("human_approval"), Runner::InProcess);
        assert_eq!(runner_for("python_fn"), Runner::ExternalWorker);
        for kind in [
            "tool",
            "mcp_tool",
            "a2a_task",
            "agent",
            "agent_tool",
            "agent_discovery",
            "coordinator",
            "subgraph",
            "parallel",
            "join",
            "wait",
            "memory_retrieval",
            "policy",
            "finalizer",
        ] {
            assert_eq!(runner_for(kind), Runner::None, "{kind}");
        }
    }
}
