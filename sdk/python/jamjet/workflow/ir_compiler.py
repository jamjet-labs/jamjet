"""
Compile Python workflow definitions and YAML files to the canonical JamJet IR.

Both paths produce the same IR dict, which is then submitted to the runtime API.
"""

from __future__ import annotations

from typing import Any

import yaml

from jamjet.workflow.types import StepDef, WorkflowDef


def compile_to_ir(defn: WorkflowDef) -> dict[str, Any]:
    """
    Compile a WorkflowDef (from Python decorators) to the canonical IR dict.
    """
    nodes: dict[str, Any] = {}
    edges: list[dict[str, Any]] = []
    steps = defn.steps

    for i, step in enumerate(steps):
        node_kind = _step_to_node_kind(step)
        nodes[step.name] = {
            "id": step.name,
            "kind": node_kind,
            "retry_policy": step.retry_policy,
            "node_timeout_secs": _parse_timeout(step.timeout),
            "description": None,
            "labels": {},
        }

        # Build edges
        if step.next:
            # Explicit routing
            for target, _ in step.next.items():
                edges.append({"from": step.name, "to": target, "condition": None})
        else:
            # Default: go to the next step in declaration order, or "end"
            next_name = steps[i + 1].name if i + 1 < len(steps) else "end"
            edges.append({"from": step.name, "to": next_name, "condition": None})

    return {
        "workflow_id": defn.workflow_id,
        "version": defn.version,
        "name": None,
        "description": None,
        "state_schema": defn.state_schema,
        "start_node": defn.start_node,
        "nodes": nodes,
        "edges": edges,
        "retry_policies": {},
        "timeouts": {
            "node_timeout": None,
            "workflow_timeout": None,
            "heartbeat_interval": 30,
            "approval_timeout": None,
        },
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {},
    }


def compile_yaml(yaml_content: str) -> dict[str, Any]:
    """
    Compile a workflow.yaml string to the canonical IR dict.

    The YAML schema is:
        workflow:
          id: ...
          version: ...
          state_schema: ...
          start: ...
        nodes:
          <node_id>:
            type: model|tool|condition|human_approval|...
            ...
        edges (or inline next: in each node)
    """
    data = yaml.safe_load(yaml_content)
    wf = data.get("workflow", {})
    raw_nodes = data.get("nodes", {})

    nodes: dict[str, Any] = {}
    edges: list[dict[str, Any]] = []

    for node_id, node_data in raw_nodes.items():
        node_type = node_data.get("type", "tool")
        kind = _yaml_node_to_kind(node_id, node_type, node_data)
        nodes[node_id] = {
            "id": node_id,
            "kind": kind,
            "retry_policy": node_data.get("retry_policy"),
            "node_timeout_secs": _parse_timeout(node_data.get("timeout")),
            "description": node_data.get("description"),
            "labels": node_data.get("labels", {}),
        }

        # Extract edges from "next" field
        next_val = node_data.get("next")
        if isinstance(next_val, str):
            edges.append({"from": node_id, "to": next_val, "condition": None})
        elif isinstance(next_val, list):
            for edge in next_val:
                if isinstance(edge, dict):
                    edges.append(
                        {
                            "from": node_id,
                            "to": edge.get("to", "end"),
                            "condition": edge.get("when"),
                        }
                    )
                elif edge == "end":
                    edges.append({"from": node_id, "to": "end", "condition": None})
        elif next_val == "end" or next_val is None:
            edges.append({"from": node_id, "to": "end", "condition": None})

    return {
        "workflow_id": wf.get("id", "unknown"),
        "version": wf.get("version", "0.1.0"),
        "name": wf.get("name"),
        "description": wf.get("description"),
        "state_schema": wf.get("state_schema", ""),
        "start_node": wf.get("start", next(iter(raw_nodes)) if raw_nodes else ""),
        "nodes": nodes,
        "edges": edges,
        "retry_policies": data.get("retry_policies", {}),
        "timeouts": data.get("timeouts", {}),
        "models": data.get("models", {}),
        "tools": data.get("tools", {}),
        "mcp_servers": data.get("mcp", {}).get("servers", {}),
        "remote_agents": data.get("a2a", {}).get("remote_agents", {}),
        "labels": wf.get("labels", {}),
    }


# ── Helpers ──────────────────────────────────────────────────────────────────


def _step_to_node_kind(step: StepDef) -> dict[str, Any]:
    """Convert a StepDef to a NodeKind dict."""
    if step.human_approval:
        return {
            "type": "human_approval",
            "description": f"Approval required for {step.name}",
            "timeout_secs": _parse_timeout(step.timeout),
            "fallback_node": None,
        }
    if step.model:
        return {
            "type": "model",
            "model_ref": step.model,
            "prompt_ref": f"prompts/{step.name}.md",
            "output_schema": "",
            "system_prompt": None,
        }
    return {
        "type": "python_fn",
        "module": step.fn.__module__,
        "function": step.fn.__qualname__,
        "output_schema": "",
    }


def _yaml_node_to_kind(node_id: str, node_type: str, data: dict[str, Any]) -> dict[str, Any]:
    """Convert a YAML node definition to a NodeKind dict."""
    if node_type == "model":
        return {
            "type": "model",
            "model_ref": data.get("model", "default_chat"),
            "prompt_ref": data.get("prompt", f"prompts/{node_id}.md"),
            "output_schema": data.get("output_schema", ""),
            "system_prompt": data.get("system_prompt"),
        }
    if node_type == "tool":
        return {
            "type": "tool",
            "tool_ref": data.get("tool_ref", node_id),
            "input_mapping": data.get("input", {}),
            "output_schema": data.get("output_schema", ""),
        }
    if node_type == "mcp_tool":
        return {
            "type": "mcp_tool",
            "server": data.get("server", ""),
            "tool": data.get("tool", ""),
            "input_mapping": data.get("input", {}),
            "output_schema": data.get("output_schema", ""),
        }
    if node_type == "a2a_task":
        return {
            "type": "a2a_task",
            "remote_agent": data.get("remote_agent", ""),
            "skill": data.get("skill", ""),
            "input_mapping": data.get("input", {}),
            "output_schema": data.get("output_schema", ""),
            "stream": data.get("stream", False),
            "on_input_required": data.get("on_input_required"),
            "timeout_secs": _parse_timeout(data.get("timeout")),
        }
    if node_type == "human_approval":
        return {
            "type": "human_approval",
            "description": data.get("description", f"Approval required for {node_id}"),
            "timeout_secs": _parse_timeout(data.get("timeout")),
            "fallback_node": data.get("fallback"),
        }
    if node_type == "agent":
        return {
            "type": "agent",
            "agent_ref": data.get("agent_ref", node_id),
            "input_mapping": data.get("input", {}),
            "output_schema": data.get("output_schema", ""),
        }
    if node_type == "condition":
        return {"type": "condition", "branches": []}
    # Default fallback
    return {"type": node_type}


def _parse_timeout(timeout: str | int | None) -> int | None:
    """Parse a timeout like '30s', '5m', '2h' to seconds."""
    if timeout is None:
        return None
    if isinstance(timeout, int):
        return timeout
    s = str(timeout).strip()
    if s.endswith("s"):
        return int(s[:-1])
    if s.endswith("m"):
        return int(s[:-1]) * 60
    if s.endswith("h"):
        return int(s[:-1]) * 3600
    return int(s)
