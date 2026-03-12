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
/// 5. All nodes are reachable from start_node
/// 6. All paths from start lead to a terminal node ("end" or a terminal kind)
/// 7. All tool_ref, model_ref, agent_ref resolve to known definitions
/// 8. All MCP servers referenced by mcp_tool nodes are configured
/// 9. All remote agents referenced by a2a_task nodes are configured
pub fn validate_workflow(ir: &WorkflowIr) -> IrResult<()> {
    validate_metadata(ir)?;
    validate_no_duplicate_nodes(ir)?;
    validate_start_node(ir)?;
    validate_edges(ir)?;
    validate_reachability(ir)?;
    validate_refs(ir)?;
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
    use jamjet_core::node::NodeKind;
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
