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
