use crate::error::{IrError, IrResult};
use crate::workflow::WorkflowIr;
use std::collections::{HashSet, VecDeque};

/// Validate a WorkflowIr. Returns Ok(()) if valid, Err with the first
/// violation found otherwise.
///
/// Validation rules:
/// 1. workflow_id and version are non-empty; version is valid semver
/// 2. No duplicate node ids
/// 3. start_node exists in nodes
/// 4. All edge targets exist
/// 5. Every Condition node's branch targets exist
/// 6. All nodes are reachable from start_node
/// 7. All paths from start lead to a terminal node ("end" or a terminal kind)
/// 8. All tool_ref, model_ref, agent_ref resolve to known definitions
/// 9. All MCP servers referenced by mcp_tool nodes are configured
/// 10. All remote agents referenced by a2a_task nodes are configured
/// 11. Every ADK tool-dispatch node carries its `agent_tool_dispatch` marker
/// 12. Every node id is in the safe set (see [`validate_node_ids`])
pub fn validate_workflow(ir: &WorkflowIr) -> IrResult<()> {
    validate_metadata(ir)?;
    validate_no_duplicate_nodes(ir)?;
    validate_start_node(ir)?;
    validate_edges(ir)?;
    validate_branch_targets(ir)?;
    validate_reachability(ir)?;
    validate_refs(ir)?;
    validate_agent_tool_dispatch(ir)?;
    validate_condition_expressions(ir)?;
    validate_external_queues(ir)?;
    validate_node_ids(ir)?;
    validate_approval_deadlines(ir)?;
    Ok(())
}

/// The workflow policy and every node policy set a deadline the engine can
/// honour (see `PolicySetIr::validate_approval_deadline`).
pub fn validate_approval_deadlines(ir: &WorkflowIr) -> IrResult<()> {
    if let Some(p) = &ir.policy {
        p.validate_approval_deadline()
            .map_err(|reason| IrError::InvalidApprovalDeadline {
                scope: "workflow".to_string(),
                reason,
            })?;
    }
    let mut ids: Vec<&String> = ir.nodes.keys().collect();
    ids.sort();
    for id in ids {
        if let Some(p) = &ir.nodes[id].policy {
            p.validate_approval_deadline()
                .map_err(|reason| IrError::InvalidApprovalDeadline {
                    scope: format!("node '{id}'"),
                    reason,
                })?;
        }
    }
    Ok(())
}

/// Refuse a non-null `timeouts.approval_timeout` in a raw IR document.
///
/// The key was declared on `TimeoutConfig` and never read: a workflow that
/// set it believed its approvals expired, and they waited forever. The
/// setting is now `approval_timeout_secs` on a policy. The parsed IR no
/// longer has the field, so this checks the document as sent. A null, which
/// the SDK compilers write, is accepted.
pub fn refuse_retired_timeouts(raw: &serde_json::Value) -> IrResult<()> {
    match raw.get("timeouts").and_then(|t| t.get("approval_timeout")) {
        None | Some(serde_json::Value::Null) => Ok(()),
        Some(_) => Err(IrError::RetiredApprovalTimeout),
    }
}

/// The longest node id.
pub const MAX_NODE_ID_LEN: usize = 128;

/// Every node id is 1 to [`MAX_NODE_ID_LEN`] ASCII letters, digits, `_`, `.`
/// or `-`, as both its map key and its `id` field.
///
/// Node ids are chosen by whoever writes the IR, and they are printed into
/// commands a reviewer pastes into a shell (`jamjet approvals` prints the
/// `jamjet approve ... --node-id` line). An id like `x; curl ...|sh #` would
/// run as the reviewer, with the reviewer's token. The CLI quotes what it
/// prints as well; this keeps every id inert wherever else it is shown.
pub fn validate_node_ids(ir: &WorkflowIr) -> IrResult<()> {
    let valid = |id: &str| {
        !id.is_empty()
            && id.len() <= MAX_NODE_ID_LEN
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    };
    for (key, node) in &ir.nodes {
        for id in [key.as_str(), node.id.as_str()] {
            if !valid(id) {
                return Err(IrError::InvalidNodeId(id.to_string()));
            }
        }
        // The key routes the node; the id is what the approval basis shows.
        // `POST /workflows` does not run `validate_workflow`, so the two are
        // held equal here.
        if key != &node.id {
            return Err(IrError::NodeIdMismatch {
                key: key.clone(),
                id: node.id.clone(),
            });
        }
    }
    Ok(())
}

/// The ADK tool-dispatch coroutine. Kept in sync with `_DISPATCH_MODULE` and
/// `_DISPATCH_FUNCTION` in `sdk/python/jamjet/compiler/agent_ir.py:51-52`.
pub const ADK_DISPATCH_MODULE: &str = "jamjet.agents.tool_runtime";
pub const ADK_DISPATCH_FUNCTION: &str = "dispatch_tool_calls";

/// True when a `python_fn`'s coordinates are the ADK tool-dispatch coroutine.
///
/// The marker is the declarative signal, but it is `#[serde(default)]`, so an IR
/// stored before the marker existed reads as unmarked — and nothing re-validates
/// a workflow already in the backend. Enforcement therefore keys on these
/// coordinates too, which is why this is public rather than inlined into
/// [`validate_agent_tool_dispatch`]: the validator and the worker must agree on
/// what an ADK dispatch node IS, from one definition.
pub fn is_adk_dispatch_coordinates(module: &str, function: &str) -> bool {
    module == ADK_DISPATCH_MODULE && function == ADK_DISPATCH_FUNCTION
}

/// Reject an ADK tool-dispatch node that is missing its policy marker.
///
/// Without the marker the worker treats the node as an opaque python_fn and
/// enforces no tool policy on it. An IR compiled before the marker existed
/// deserializes to `false`, so this makes the drift loud instead of silent.
///
/// The dispatch coordinates are the only signal that survives to Rust: the
/// compiler nests its `tools` resolver map inside the *kind* dict, `PythonFn`
/// has no such field, and serde drops unknown keys.
///
/// Python coordinates only — no Java ADK compiler emits `java_fn` dispatch
/// nodes yet. Add the Java pair here when that compiler lands.
///
/// Public, and callable independently of [`validate_workflow`], because the
/// `POST /workflows` route needs exactly this rule without the ref rules it
/// deliberately skips (see `runtime/api/src/routes.rs`).
///
/// And the other direction: the flag is refused on any node that is not the
/// ADK dispatcher. A flagged node's policy used to be decided over the tool
/// calls in its input alone, so an author could put the flag on an ordinary
/// function and present no calls; the flag is author-controlled, so it is
/// accepted only where it is true. `java_fn` has no ADK dispatcher, so the flag
/// is refused on every `java_fn`.
pub fn validate_agent_tool_dispatch(ir: &WorkflowIr) -> IrResult<()> {
    use jamjet_core::node::NodeKind;

    for (id, node) in &ir.nodes {
        match &node.kind {
            NodeKind::PythonFn {
                module,
                function,
                agent_tool_dispatch,
                ..
            } => {
                let is_dispatcher = is_adk_dispatch_coordinates(module, function);
                if is_dispatcher && !agent_tool_dispatch {
                    return Err(IrError::UnmarkedAgentToolDispatch(id.clone()));
                }
                if *agent_tool_dispatch && !is_dispatcher {
                    return Err(IrError::MisplacedAgentToolDispatch(
                        id.clone(),
                        format!("python_fn {module}:{function}"),
                    ));
                }
            }
            NodeKind::JavaFn {
                class_name,
                method,
                agent_tool_dispatch: true,
                ..
            } => {
                return Err(IrError::MisplacedAgentToolDispatch(
                    id.clone(),
                    format!("java_fn {class_name}/{method}"),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Every condition branch expression must be one the runtime's evaluator can
/// satisfy: a `state.<path>` reference, optionally compared with `==` or `!=`
/// to a JSON literal. Anything else evaluates false forever at runtime, so it
/// is refused here, at the one moment the author is listening.
pub fn validate_condition_expressions(ir: &WorkflowIr) -> IrResult<()> {
    use jamjet_core::node::NodeKind;
    for (id, node) in &ir.nodes {
        if let NodeKind::Condition { branches } = &node.kind {
            for branch in branches {
                if let Some(expr) = &branch.condition {
                    if let Err(reason) = jamjet_core::validate_expression(expr) {
                        return Err(IrError::InvalidConditionExpression {
                            node: id.clone(),
                            expr: expr.clone(),
                            reason,
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// The longest queue name an `external_fn` node may use.
pub const MAX_QUEUE_NAME_LEN: usize = 64;

/// Every `external_fn` node must name a queue an external worker can own:
/// non-empty, at most [`MAX_QUEUE_NAME_LEN`] ASCII letters, digits, `-`, `_`
/// or `.`, and not one of the built-in queues. A built-in name would route the
/// item to a worker that cannot run it (a `python_tool` worker gets a payload
/// with no module or function and fails it until it is dead-lettered).
pub fn validate_external_queues(ir: &WorkflowIr) -> IrResult<()> {
    use jamjet_core::node::{NodeKind, QueueType};
    for (id, node) in &ir.nodes {
        let NodeKind::ExternalFn { queue, .. } = &node.kind else {
            continue;
        };
        let reason = if queue.is_empty() {
            Some("the queue name is empty".to_string())
        } else if queue.len() > MAX_QUEUE_NAME_LEN {
            Some(format!(
                "the queue name is longer than {MAX_QUEUE_NAME_LEN} characters"
            ))
        } else if !queue
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            Some("use only ASCII letters, digits, '-', '_' and '.'".to_string())
        } else if !matches!(QueueType::from_name(queue), QueueType::Named(_)) {
            Some("that is a built-in queue; name a queue of your own".to_string())
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(IrError::InvalidQueueName {
                node: id.clone(),
                queue: queue.clone(),
                reason,
            });
        }
    }
    Ok(())
}

fn validate_metadata(ir: &WorkflowIr) -> IrResult<()> {
    if ir.workflow_id.is_empty() {
        return Err(IrError::InvalidVersion("workflow_id is empty".into()));
    }
    // Basic semver check: must contain two dots
    let parts: Vec<&str> = ir.version.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.parse::<u32>().is_err()) {
        return Err(IrError::InvalidVersion(ir.version.clone()));
    }
    Ok(())
}

fn validate_no_duplicate_nodes(ir: &WorkflowIr) -> IrResult<()> {
    // Node ids are the HashMap keys so duplicates are structurally impossible,
    // but we check the stored ids match their keys.
    for (key, node) in &ir.nodes {
        if key != &node.id {
            return Err(IrError::DuplicateNodeId(node.id.clone()));
        }
    }
    Ok(())
}

fn validate_start_node(ir: &WorkflowIr) -> IrResult<()> {
    if ir.start_node.is_empty() {
        return Err(IrError::NoStartNode);
    }
    if !ir.nodes.contains_key(&ir.start_node) {
        return Err(IrError::UnreachableNode(ir.start_node.clone()));
    }
    Ok(())
}

fn validate_edges(ir: &WorkflowIr) -> IrResult<()> {
    for edge in &ir.edges {
        if !ir.nodes.contains_key(&edge.to) && edge.to != "end" {
            return Err(IrError::UnknownEdgeTarget {
                from: edge.from.clone(),
                to: edge.to.clone(),
            });
        }
    }
    Ok(())
}

// FDL-5: every `ConditionalBranch.target` of a Condition node must reference an
// existing node id (or the terminal sentinel `"end"`, like an edge target). A
// dangling branch target is a validation error: the scheduler would route to a
// node that never runs, silently stranding the branch. This mirrors
// `validate_edges` but checks the branch list the routing executor reads, which a
// hand-built or miscompiled IR could leave inconsistent with the edges.
//
// Additionally, each branch target must appear as a `to` in the Condition node's
// out-edges. The compiler emits branches and edges 1:1; a mismatch means the
// executor would resolve a target that the scheduler's edge graph can never
// traverse, silently dropping the branch at runtime.
fn validate_branch_targets(ir: &WorkflowIr) -> IrResult<()> {
    use jamjet_core::node::NodeKind;

    for (node_id, node) in &ir.nodes {
        if let NodeKind::Condition { branches } = &node.kind {
            let out_targets: HashSet<&str> = ir
                .edges_from(node_id)
                .into_iter()
                .map(|e| e.to.as_str())
                .collect();

            for branch in branches {
                if branch.target != "end" && !ir.nodes.contains_key(&branch.target) {
                    return Err(IrError::UnknownBranchTarget {
                        node: node_id.clone(),
                        target: branch.target.clone(),
                    });
                }
                if !out_targets.contains(branch.target.as_str()) {
                    return Err(IrError::BranchTargetMissingOutEdge {
                        node: node_id.clone(),
                        target: branch.target.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

fn validate_reachability(ir: &WorkflowIr) -> IrResult<()> {
    // BFS from start_node
    let mut visited: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    queue.push_back(&ir.start_node);
    visited.insert(&ir.start_node);

    while let Some(current) = queue.pop_front() {
        for edge in ir.edges_from(current) {
            let next = edge.to.as_str();
            if next != "end" && !visited.contains(next) {
                visited.insert(next);
                queue.push_back(next);
            }
        }
    }

    // Check all nodes are reachable
    for node_id in ir.nodes.keys() {
        if !visited.contains(node_id.as_str()) {
            return Err(IrError::UnreachableNode(node_id.clone()));
        }
    }
    Ok(())
}

// `if !map.contains_key(...)` inside each match arm is intentional here —
// reads more naturally than collapsing into match guards (which would require
// an explicit success arm per variant and duplicate the destructuring).
#[allow(clippy::collapsible_match, clippy::collapsible_if)]
fn validate_refs(ir: &WorkflowIr) -> IrResult<()> {
    use jamjet_core::node::NodeKind;

    for (node_id, node) in &ir.nodes {
        match &node.kind {
            NodeKind::Tool { tool_ref, .. } => {
                if !ir.tools.contains_key(tool_ref) {
                    return Err(IrError::UnknownToolRef(node_id.clone(), tool_ref.clone()));
                }
            }
            NodeKind::Model { model_ref, .. } => {
                if !ir.models.contains_key(model_ref) {
                    return Err(IrError::UnknownModelRef(node_id.clone(), model_ref.clone()));
                }
            }
            NodeKind::McpTool { server, .. } => {
                if !ir.mcp_servers.contains_key(server) {
                    return Err(IrError::UnknownMcpServer(node_id.clone(), server.clone()));
                }
            }
            NodeKind::A2aTask { remote_agent, .. } => {
                if !ir.remote_agents.contains_key(remote_agent) {
                    return Err(IrError::UnknownRemoteAgent(
                        node_id.clone(),
                        remote_agent.clone(),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::*;
    use jamjet_core::node::{ConditionalBranch, NodeKind};
    use jamjet_core::timeout::TimeoutConfig;
    use std::collections::HashMap;

    fn make_ir(start: &str, nodes: Vec<(&str, NodeKind)>, edges: Vec<(&str, &str)>) -> WorkflowIr {
        let nodes_map: HashMap<String, NodeDef> = nodes
            .into_iter()
            .map(|(id, kind)| {
                (
                    id.to_string(),
                    NodeDef {
                        id: id.to_string(),
                        kind,
                        retry_policy: None,
                        node_timeout_secs: None,
                        description: None,
                        labels: HashMap::new(),
                        policy: None,
                        data_policy: None,
                        agent_ref: None,
                    },
                )
            })
            .collect();

        let edges_vec: Vec<EdgeDef> = edges
            .into_iter()
            .map(|(from, to)| EdgeDef {
                from: from.to_string(),
                to: to.to_string(),
                condition: None,
            })
            .collect();

        WorkflowIr {
            workflow_id: "test".into(),
            version: "0.1.0".into(),
            name: None,
            description: None,
            state_schema: "schemas.State".into(),
            start_node: start.to_string(),
            nodes: nodes_map,
            edges: edges_vec,
            retry_policies: HashMap::new(),
            timeouts: TimeoutConfig::default(),
            models: HashMap::new(),
            tools: HashMap::new(),
            mcp_servers: HashMap::new(),
            remote_agents: HashMap::new(),
            labels: HashMap::new(),
            policy: None,
            token_budget: None,
            cost_budget_usd: None,
            on_budget_exceeded: None,
            data_policy: None,
            agent_ref: None,
            continue_as_new: false,
        }
    }

    #[test]
    fn valid_simple_workflow() {
        let cond_node = NodeKind::Condition { branches: vec![] };
        let ir = make_ir("a", vec![("a", cond_node)], vec![("a", "end")]);
        assert!(validate_workflow(&ir).is_ok());
    }

    #[test]
    fn unknown_edge_target() {
        let cond = NodeKind::Condition { branches: vec![] };
        let ir = make_ir("a", vec![("a", cond)], vec![("a", "nonexistent")]);
        let err = validate_workflow(&ir);
        assert!(matches!(err, Err(IrError::UnknownEdgeTarget { .. })));
    }

    #[test]
    fn unreachable_node() {
        let cond = NodeKind::Condition { branches: vec![] };
        let cond2 = NodeKind::Condition { branches: vec![] };
        let ir = make_ir(
            "a",
            vec![("a", cond), ("orphan", cond2)],
            vec![("a", "end")],
        );
        let err = validate_workflow(&ir);
        assert!(matches!(err, Err(IrError::UnreachableNode(_))));
    }

    #[test]
    fn unknown_branch_target() {
        // A Condition whose branch points at a node that doesn't exist. The edges
        // are all valid (validate_edges passes), so the dangling BRANCH target is
        // what gets caught — a clear, branch-specific error.
        let cond = NodeKind::Condition {
            branches: vec![
                ConditionalBranch {
                    condition: Some("state.x == \"go\"".into()),
                    target: "ghost".into(),
                },
                ConditionalBranch {
                    condition: None,
                    target: "b".into(),
                },
            ],
        };
        let b = NodeKind::Condition { branches: vec![] };
        let ir = make_ir(
            "a",
            vec![("a", cond), ("b", b)],
            vec![("a", "b"), ("b", "end")],
        );
        let err = validate_workflow(&ir);
        assert!(
            matches!(&err, Err(IrError::UnknownBranchTarget { node, target }) if node == "a" && target == "ghost"),
            "a dangling branch target must fail with UnknownBranchTarget, got {err:?}"
        );
    }

    #[test]
    fn valid_branch_targets_pass() {
        // Branches targeting a real node and the `end` sentinel both validate
        // (the agent-loop gate's `{tool_calls -> tools, else -> end}` shape).
        let cond = NodeKind::Condition {
            branches: vec![
                ConditionalBranch {
                    condition: Some("state.x == \"go\"".into()),
                    target: "b".into(),
                },
                ConditionalBranch {
                    condition: None,
                    target: "end".into(),
                },
            ],
        };
        let b = NodeKind::Condition { branches: vec![] };
        let ir = make_ir(
            "a",
            vec![("a", cond), ("b", b)],
            vec![("a", "b"), ("a", "end"), ("b", "end")],
        );
        assert!(
            validate_workflow(&ir).is_ok(),
            "branch targets that are real nodes or `end` must validate"
        );
    }

    #[test]
    fn branch_target_missing_out_edge() {
        // Branch declares target "b" (a known node), but no edge a->b exists.
        // The cross-check must catch the mismatch before reachability does.
        let cond = NodeKind::Condition {
            branches: vec![
                ConditionalBranch {
                    condition: Some("state.x == \"go\"".into()),
                    target: "b".into(),
                },
                ConditionalBranch {
                    condition: None,
                    target: "end".into(),
                },
            ],
        };
        let b = NodeKind::Condition { branches: vec![] };
        // Edge a->b intentionally absent; only a->end and b->end present.
        let ir = make_ir(
            "a",
            vec![("a", cond), ("b", b)],
            vec![("a", "end"), ("b", "end")],
        );
        let err = validate_workflow(&ir);
        assert!(
            matches!(&err, Err(IrError::BranchTargetMissingOutEdge { node, target }) if node == "a" && target == "b"),
            "a branch target with no matching out-edge must fail with BranchTargetMissingOutEdge, got {err:?}"
        );
    }

    /// A one-node IR built from raw JSON, so a node kind can omit
    /// `agent_tool_dispatch` entirely — exactly how a pre-marker IR
    /// deserializes. The struct fixture above cannot express that: the field is
    /// not optional in Rust, so it can only ever be written as `false`.
    ///
    /// The maps are spelled out because `WorkflowIr` has no serde defaults for
    /// them; this mirrors the dispatch fixtures in `runtime/workers` and
    /// `runtime/api`.
    fn ir_with_kind(kind: serde_json::Value) -> WorkflowIr {
        serde_json::from_value(serde_json::json!({
            "workflow_id": "wf",
            "version": "1.0.0",
            "state_schema": "{}",
            "start_node": "n1",
            "nodes": { "n1": { "id": "n1", "kind": kind } },
            "edges": [],
            "retry_policies": {},
            "models": {},
            "tools": {},
            "mcp_servers": {},
            "remote_agents": {}
        }))
        .expect("fixture IR must parse")
    }

    #[test]
    fn unmarked_adk_dispatch_node_is_rejected() {
        // An ADK IR compiled before the flag existed would otherwise run with
        // no tool policy at all (C1 regression guard).
        let ir = ir_with_kind(serde_json::json!({
            "type": "python_fn",
            "module": "jamjet.agents.tool_runtime",
            "function": "dispatch_tool_calls",
            "output_schema": ""
        }));
        let err = validate_workflow(&ir).expect_err("must reject an unmarked dispatch node");
        assert!(format!("{err}").contains("agent_tool_dispatch"));
    }

    #[test]
    fn marked_adk_dispatch_node_validates() {
        let ir = ir_with_kind(serde_json::json!({
            "type": "python_fn",
            "module": "jamjet.agents.tool_runtime",
            "function": "dispatch_tool_calls",
            "output_schema": "",
            "agent_tool_dispatch": true
        }));
        assert!(validate_workflow(&ir).is_ok());
    }

    /// The flag is author-controlled, so it is accepted only on the ADK
    /// dispatcher: on any other function it would route the node's policy to
    /// the per-call guard (round-3 review, C1).
    #[test]
    fn the_dispatch_flag_on_another_python_fn_is_rejected() {
        let ir = ir_with_kind(serde_json::json!({
            "type": "python_fn",
            "module": "payments.transfer",
            "function": "send",
            "output_schema": "",
            "agent_tool_dispatch": true
        }));
        let err = validate_agent_tool_dispatch(&ir).expect_err("must reject the misplaced flag");
        assert!(
            matches!(err, IrError::MisplacedAgentToolDispatch(..)),
            "{err}"
        );
        assert!(format!("{err}").contains("agent_tool_dispatch"));
    }

    #[test]
    fn the_dispatch_flag_on_a_java_fn_is_rejected() {
        let ir = ir_with_kind(serde_json::json!({
            "type": "java_fn",
            "class_name": "com.acme.Payments",
            "method": "send",
            "output_schema": "",
            "agent_tool_dispatch": true
        }));
        assert!(matches!(
            validate_agent_tool_dispatch(&ir),
            Err(IrError::MisplacedAgentToolDispatch(..))
        ));
    }

    #[test]
    fn ordinary_python_fn_validates() {
        let ir = ir_with_kind(serde_json::json!({
            "type": "python_fn",
            "module": "my.module",
            "function": "my_fn",
            "output_schema": ""
        }));
        assert!(validate_workflow(&ir).is_ok());
    }

    #[test]
    fn invalid_semver() {
        let mut ir = make_ir("a", vec![], vec![]);
        ir.nodes.insert(
            "a".into(),
            NodeDef {
                id: "a".into(),
                kind: NodeKind::Condition { branches: vec![] },
                retry_policy: None,
                node_timeout_secs: None,
                description: None,
                labels: HashMap::new(),
                policy: None,
                data_policy: None,
                agent_ref: None,
            },
        );
        ir.edges.push(EdgeDef {
            from: "a".into(),
            to: "end".into(),
            condition: None,
        });
        ir.version = "not-semver".into();
        let err = validate_workflow(&ir);
        assert!(matches!(err, Err(IrError::InvalidVersion(_))));
    }
}
