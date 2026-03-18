"""Graph builder API for complex workflow construction."""

from __future__ import annotations

from typing import Any

from jamjet.workflow.nodes import (
    AgentToolNode,
    ConditionNode,
    CoordinatorNode,
    EvalNode,
    HumanApprovalNode,
    ModelNode,
    ToolNode,
)

AnyNode = ModelNode | ToolNode | ConditionNode | HumanApprovalNode | EvalNode | CoordinatorNode | AgentToolNode


class WorkflowGraph:
    """
    Explicit graph builder for complex workflow orchestration.

    Usage::

        graph = WorkflowGraph("complex_pipeline")
        graph.add_node("fetch", ToolNode(tool_ref="get_data"))
        graph.add_node("analyze", ModelNode(model="default_chat"))
        graph.add_edge("fetch", "analyze")
        graph.add_edge("analyze", "end")
        ir = graph.compile()
    """

    def __init__(self, workflow_id: str, version: str = "0.1.0") -> None:
        self.workflow_id = workflow_id
        self.version = version
        self._nodes: dict[str, AnyNode] = {}
        self._edges: list[dict[str, Any]] = []
        self._start: str | None = None
        self._state_schema: str = ""
        self._already_expanded = False

    def set_state(self, schema: str) -> WorkflowGraph:
        self._state_schema = schema
        return self

    def add_node(self, node_id: str, node: AnyNode) -> WorkflowGraph:
        if not self._nodes:
            self._start = node_id
        self._nodes[node_id] = node
        self._already_expanded = False
        return self

    def add_coordinator(self, name: str, **kwargs: Any) -> WorkflowGraph:
        from .nodes import CoordinatorNode

        node = CoordinatorNode(**kwargs)
        return self.add_node(name, node)

    def add_agent_tool(self, name: str, **kwargs: Any) -> WorkflowGraph:
        from .nodes import AgentToolNode

        node = AgentToolNode(**kwargs)
        return self.add_node(name, node)

    def add_edge(self, from_id: str, to_id: str, condition: str | None = None) -> WorkflowGraph:
        self._edges.append({"from": from_id, "to": to_id, "condition": condition})
        self._already_expanded = False
        return self

    def _expand_auto_agents(self) -> None:
        if self._already_expanded:
            return

        from .nodes import AgentToolNode, CoordinatorNode

        auto_nodes = [
            (nid, node) for nid, node in self._nodes.items() if isinstance(node, AgentToolNode) and node.agent == "auto"
        ]

        for agent_node_id, agent_node in auto_nodes:
            coord_id = f"_coordinator_{agent_node_id}"

            if coord_id in self._nodes:
                raise ValueError(
                    f"Auto-expansion would overwrite existing node '{coord_id}'. "
                    "Reserve the '_coordinator_' prefix for generated nodes."
                )

            coord_node = CoordinatorNode(
                task=f"Route to agent for: {agent_node_id}",
                output_key=f"_selected_agent_{agent_node_id}",
                strategy="default",
            )
            self._nodes[coord_id] = coord_node

            agent_node.agent = f"state._selected_agent_{agent_node_id}"

            for edge in self._edges:
                if edge["to"] == agent_node_id:
                    edge["to"] = coord_id

            self._edges.append({"from": coord_id, "to": agent_node_id, "condition": None})

            if self._start == agent_node_id:
                self._start = coord_id

        self._already_expanded = True

    def compile(self) -> dict[str, Any]:
        self._expand_auto_agents()
        nodes: dict[str, Any] = {}
        for node_id, node in self._nodes.items():
            nodes[node_id] = {
                "id": node_id,
                "kind": node.to_ir_kind(),
                "retry_policy": None,
                "node_timeout_secs": None,
                "description": None,
                "labels": {},
            }

        return {
            "workflow_id": self.workflow_id,
            "version": self.version,
            "state_schema": self._state_schema,
            "start_node": self._start or "",
            "nodes": nodes,
            "edges": self._edges,
            "retry_policies": {},
            "timeouts": {},
            "models": {},
            "tools": {},
            "mcp_servers": {},
            "remote_agents": {},
            "labels": {},
        }
