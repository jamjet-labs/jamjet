"""
Compile Python workflow definitions and YAML files to the canonical JamJet IR.

Both paths produce the same IR dict, which is then submitted to the runtime API.

Agent-first YAML
----------------
A top-level ``agent:`` block triggers the strategy compiler instead of the
manual ``nodes:`` + ``edges:`` path::

    agent:
      id: my-agent
      strategy: plan-and-execute
      goal: "Research the given company and write an investment memo"
      tools: [search_web, read_document]
      model: gpt-4o
      limits:
        max_iterations: 6
        max_cost_usd: 0.50
        timeout_seconds: 120
"""

from __future__ import annotations

import warnings
from typing import TYPE_CHECKING, Any

import yaml

from jamjet.workflow.types import DEFAULT_WORKFLOW_VERSION, StepDef, WorkflowDef

if TYPE_CHECKING:
    from jamjet.spec import WorkflowSpec


class WorkflowSchemaWarning(UserWarning):
    """A workflow document has a key the compiler does not read.

    The key is dropped from the compiled IR. Structural problems (no nodes,
    ``nodes:`` nested under ``workflow:`` or written as a list, an unknown node
    type) and malformed governance blocks are errors instead.
    """


# The keys the graph compiler reads, so the ones it does not can be reported
# instead of dropped without a word. Keep these in step with
# _compile_graph_yaml and _yaml_node_to_kind.
#
# Governance settings. The runtime enforces policy, cost_budget_usd,
# token_budget and continue_as_new; it reads neither data_policy nor
# on_budget_exceeded yet, so those compile with a warning. They may sit at the top level of
# the document or in the `workflow:` header, but not both.
_GOVERNANCE_KEYS = ("policy", "cost_budget_usd", "token_budget", "on_budget_exceeded", "data_policy", "continue_as_new")
_GRAPH_TOP_LEVEL_KEYS = frozenset(
    {
        "workflow",
        "nodes",
        "retry_policies",
        "timeouts",
        "models",
        "tools",
        "mcp",
        "a2a",
        "id",
        "version",
        *_GOVERNANCE_KEYS,
    }
)
_WORKFLOW_HEADER_KEYS = frozenset(
    {"id", "version", "name", "description", "state_schema", "start", "labels", "agent_ref", *_GOVERNANCE_KEYS}
)
# Every node may carry these, whatever its type.
_NODE_COMMON_KEYS = frozenset(
    {"type", "description", "labels", "policy", "data_policy", "retry_policy", "timeout", "acts_for", "next"}
)
# What each node type reads on top of the common keys.
_NODE_TYPE_KEYS: dict[str, frozenset[str]] = {
    "model": frozenset({"model", "prompt", "output_schema", "system_prompt", "temperature", "max_tokens"}),
    "tool": frozenset({"tool_ref", "input", "output_schema"}),
    "mcp_tool": frozenset({"server", "tool", "input", "output_schema"}),
    "a2a_task": frozenset({"remote_agent", "skill", "input", "output_schema", "stream", "on_input_required"}),
    "human_approval": frozenset({"fallback"}),
    "agent": frozenset({"agent_ref", "input", "output_schema"}),
    "condition": frozenset({"conditions", "default"}),
    "branch": frozenset({"conditions", "default"}),
    "eval": frozenset({"scorers", "on_fail", "max_retries", "input_expr"}),
    "limit_exceeded": frozenset(),
    "end": frozenset(),
}
_CONDITION_ENTRY_KEYS = frozenset({"if", "when", "next", "to"})
_NEXT_ENTRY_KEYS = frozenset({"to", "when"})
_AGENT_TOP_LEVEL_KEYS = frozenset({"agent", *_GOVERNANCE_KEYS})
_AGENT_KEYS = frozenset(
    {
        "id",
        "name",
        "version",
        "strategy",
        "strategy_config",
        "goal",
        "model",
        "tools",
        "limits",
        "state_schema",
        "agent_ref",
        *_GOVERNANCE_KEYS,
    }
)

# What to use instead of a node key the compiler does not read.
_IGNORED_NODE_KEY_HINTS = {
    "output_key": "a model node's output is written to state.last_model_output",
    "retry": "name a policy with retry_policy: io_default, llm_default or no_retry",
    "oauth_scopes": "OAuth scopes are not part of the workflow IR",
    "prompt_template": "use prompt",
    "kind": "use type",
    "config": "put the settings on the node itself",
}
_TOKEN_BUDGET_KEYS = frozenset({"input_tokens", "output_tokens", "total_tokens"})
_DATA_POLICY_KEYS = frozenset(
    {"pii_fields", "pii_detectors", "redaction_mode", "retain_prompts", "retain_outputs", "retention_days"}
)


def _warn(message: str) -> None:
    warnings.warn(message, WorkflowSchemaWarning, stacklevel=4)


def _warn_unknown_keys(found: Any, known: frozenset[str], *, location: str) -> None:
    unknown = sorted(str(k) for k in set(found) - known)
    if unknown:
        _warn(f"{location}: unknown key(s) {', '.join(unknown)} are ignored. Known keys: {', '.join(sorted(known))}")


def _rename(node_id: str, data: dict[str, Any], old: str, new: str) -> None:
    """Read ``old`` as ``new``; both set is ambiguous and an error."""
    if old not in data:
        return
    if new in data and data[new] != data[old]:
        raise ValueError(f"node '{node_id}': both {old} and {new} are set; keep {new}")
    data.setdefault(new, data[old])
    del data[old]


def _normalize_node(node_id: str, raw: Any, extras: dict[str, dict[str, Any]]) -> tuple[str, dict[str, Any]]:
    """Check one node and map the spellings that have a real IR target.

    Returns ``(type, node)``; ``node`` is a copy with aliases resolved and the
    keys the compiler does not read removed (each reported with a warning).
    ``extras`` collects the ``models`` and ``remote_agents`` entries a node's
    settings need. Raises ``ValueError`` for a node that is not a mapping, an
    unknown type, or `agent_ref` on a node that is not an `agent` node.
    """
    if not isinstance(raw, dict):
        raise ValueError(f"node '{node_id}' must be a mapping of settings, got {type(raw).__name__}")
    data = dict(raw)
    node_type = data.get("type", "tool")
    if not isinstance(node_type, str) or node_type not in _NODE_TYPE_KEYS:
        raise ValueError(
            f"node '{node_id}': unknown type {node_type!r}. Supported types: {', '.join(sorted(_NODE_TYPE_KEYS))}"
        )
    # `acts_for`, not `agent_ref`: on a node, `agent_ref` names the agent an
    # `agent` node calls.
    if node_type != "agent" and "agent_ref" in data:
        raise ValueError(
            f"node '{node_id}': agent_ref names the agent an `agent` node calls; "
            "to say which agent this node acts for, use acts_for"
        )

    _rename(node_id, data, "label", "description")
    if node_type == "model":
        _rename(node_id, data, "system", "system_prompt")
    if node_type == "human_approval":
        _rename(node_id, data, "message", "description")
    if node_type in ("tool", "mcp_tool"):
        _rename(node_id, data, "arguments", "input")
    if "input" in data:
        data["input"] = _input_mapping(node_id, data["input"])
    if node_type == "tool" and "server" in data:
        # An MCP call written as a tool node: the IR has a node kind for it.
        _warn(f"node '{node_id}': a tool node with server: is compiled as type: mcp_tool; write type: mcp_tool")
        node_type = "mcp_tool"
        data["type"] = "mcp_tool"
    if node_type == "a2a_task":
        _rename(node_id, data, "timeout_seconds", "timeout")
        for url_key in ("agent_url", "agent_uri"):
            if url_key in data:
                if "remote_agent" in data:
                    raise ValueError(f"node '{node_id}': both {url_key} and remote_agent are set; keep one")
                # A URL on the node becomes a remote agent of the same name.
                extras["remote_agents"][node_id] = {"url": data.pop(url_key)}
                data["remote_agent"] = node_id
    if node_type in ("condition", "branch") and "branches" in data:
        if "conditions" in data:
            raise ValueError(f"node '{node_id}': both branches and conditions are set; keep conditions")
        branches = data.pop("branches")
        if not isinstance(branches, list):
            raise ValueError(f"node '{node_id}': branches must be a list")
        aliases = {"condition": "if", "target": "next"}
        data["conditions"] = [
            {aliases.get(k, k): v for k, v in b.items()} if isinstance(b, dict) else b for b in branches
        ]

    for key in ("cost_budget_usd", "token_budget", "on_budget_exceeded"):
        if key in data:
            raise ValueError(
                f"node '{node_id}': {key} cannot be set on a node; the runtime has no per-node budget. "
                "Set it on the workflow (top level or the workflow: header)."
            )
    if "data_policy" in data:
        _warn(f"node '{node_id}': data_policy is compiled into the IR, but the runtime does not enforce it yet")
    known = _NODE_COMMON_KEYS | _NODE_TYPE_KEYS[node_type]
    for key in sorted(str(k) for k in set(data) - known):
        hint = _IGNORED_NODE_KEY_HINTS.get(key)
        _warn(
            f"node '{node_id}' ({node_type}): key '{key}' is ignored"
            + (f"; {hint}" if hint else f". Keys {node_type} reads: {', '.join(sorted(known))}")
            + ". It will be an error in a future release."
        )
        del data[key]

    conditions = data.get("conditions")
    if conditions is not None:
        if not isinstance(conditions, list):
            raise ValueError(f"node '{node_id}': conditions must be a list")
        for i, cond in enumerate(conditions):
            if not isinstance(cond, dict):
                raise ValueError(f"node '{node_id}': conditions[{i}] must be a mapping")
            if "target" in cond:
                if "next" in cond or "to" in cond:
                    raise ValueError(f"node '{node_id}': conditions[{i}] sets target and next/to; keep one")
                cond["next"] = cond.pop("target")
            _warn_unknown_keys(cond, _CONDITION_ENTRY_KEYS, location=f"node '{node_id}' conditions[{i}]")

    next_val = data.get("next")
    if isinstance(next_val, list):
        entries = []
        for i, edge in enumerate(next_val):
            if isinstance(edge, dict):
                edge = dict(edge)
                if "else" in edge:
                    # `else: X` is the unconditional edge.
                    if "to" in edge:
                        raise ValueError(f"node '{node_id}': next[{i}] sets both else and to")
                    edge["to"] = edge.pop("else")
                for old, new in (("condition", "when"), ("if", "when"), ("next", "to")):
                    if old in edge and new not in edge:
                        edge[new] = edge.pop(old)
                _warn_unknown_keys(edge, _NEXT_ENTRY_KEYS, location=f"node '{node_id}' next[{i}]")
            entries.append(edge)
        data["next"] = entries

    if node_type == "model" and ("temperature" in data or "max_tokens" in data):
        # The runtime reads sampling settings from the models entry the node's
        # model_ref names, so they go on the entry for this model, and
        # model_ref (what model_allowlist checks and audit records) stays the
        # real model.
        extras["models"][node_id] = {
            "model": data.get("model", "default_chat"),
            "temperature": data.pop("temperature", None),
            "max_tokens": data.pop("max_tokens", None),
        }
    return node_type, data


def _input_mapping(node_id: str, raw: Any) -> dict[str, str]:
    """A node's ``input`` as the runtime takes it: every value a string.

    Numbers and booleans are written as their JSON text; a nested mapping or
    list is an error, since the runtime would refuse it at registration with a
    message that names neither the node nor the key.
    """
    if not isinstance(raw, dict):
        raise ValueError(f"node '{node_id}': input must be a mapping of name to value, got {type(raw).__name__}")
    out: dict[str, str] = {}
    for key, value in raw.items():
        if isinstance(value, str):
            out[key] = value
        elif isinstance(value, bool):
            out[key] = "true" if value else "false"
        elif isinstance(value, (int, float)):
            out[key] = str(value)
        else:
            raise ValueError(
                f"node '{node_id}': input '{key}' must be a string, number or boolean, "
                f"got {type(value).__name__}; write it as a template string"
            )
    return out


def _check_every_node_is_reachable(nodes: dict[str, Any], edges: list[dict[str, Any]], start: str) -> None:
    """Raise ``ValueError`` for a node nothing leads to.

    The scheduler treats a node with no incoming edge as a root and runs it
    when the run starts, so a step meant to follow an approval would run before
    anyone decides. Edges and a human_approval's fallback count as incoming.
    """
    incoming = {edge["to"] for edge in edges}
    for node in nodes.values():
        fallback = node["kind"].get("fallback_node") if node["kind"].get("type") == "human_approval" else None
        if fallback:
            incoming.add(fallback)
    orphans = [node_id for node_id in nodes if node_id != start and node_id not in incoming]
    if orphans:
        raise ValueError(
            f"node(s) {', '.join(repr(n) for n in orphans)} have no incoming edge, so the engine would run them "
            "when the run starts. Point a next:, a condition branch or a fallback at each, or remove it."
        )


def _governance_value(sources: list[tuple[str, dict[str, Any]]], key: str) -> Any:
    """``key`` from the one place it is set, or None. Set in two places is an error."""
    found = [(where, block[key]) for where, block in sources if isinstance(block, dict) and key in block]
    if len(found) > 1:
        places = " and ".join(where for where, _ in found)
        raise ValueError(f"{key} is set in {places}; set it in one place")
    return found[0][1] if found else None


def _compile_token_budget(raw: Any) -> dict[str, int]:
    if not isinstance(raw, dict):
        raise ValueError(f"token_budget must be a mapping of {', '.join(sorted(_TOKEN_BUDGET_KEYS))}")
    unknown = sorted(set(raw) - _TOKEN_BUDGET_KEYS)
    if unknown:
        raise ValueError(
            f"Unknown key(s) in token_budget: {', '.join(unknown)}. Allowed: {', '.join(sorted(_TOKEN_BUDGET_KEYS))}"
        )
    out: dict[str, int] = {}
    for key, value in raw.items():
        if isinstance(value, bool) or not isinstance(value, int) or value < 1:
            raise ValueError(f"token_budget.{key} must be a positive whole number, got {value!r}")
        out[key] = value
    return out


def _compile_data_policy(raw: Any, *, location: str) -> dict[str, Any] | None:
    if raw is None:
        return None
    if not isinstance(raw, dict):
        raise ValueError(f"data_policy at {location} must be a mapping, got {type(raw).__name__}")
    unknown = sorted(set(raw) - _DATA_POLICY_KEYS)
    if unknown:
        raise ValueError(
            f"Unknown key(s) in data_policy at {location}: {', '.join(unknown)}. "
            f"Allowed keys: {', '.join(sorted(_DATA_POLICY_KEYS))}"
        )
    return dict(raw)


def _apply_governance(ir: dict[str, Any], sources: list[tuple[str, dict[str, Any]]]) -> None:
    """Copy the governance settings found in ``sources`` into ``ir``.

    None may be dropped without a word: a setting given in two places, or with
    a malformed value, is an error. The runtime enforces policy,
    cost_budget_usd, token_budget and continue_as_new; data_policy and
    on_budget_exceeded compile into the IR with a warning, because the runtime
    does not read them yet.
    """
    ir["policy"] = _compile_policy(_governance_value(sources, "policy"), location="workflow-level policy")
    budget = _governance_value(sources, "cost_budget_usd")
    if budget is not None:
        if isinstance(budget, bool) or not isinstance(budget, (int, float)) or budget <= 0:
            raise ValueError(f"cost_budget_usd must be a positive number, got {budget!r}")
        ir["cost_budget_usd"] = float(budget)
    tokens = _governance_value(sources, "token_budget")
    if tokens is not None:
        ir["token_budget"] = _compile_token_budget(tokens)
    target = _governance_value(sources, "on_budget_exceeded")
    if target is not None:
        if not isinstance(target, str):
            raise ValueError(f"on_budget_exceeded must name a node of this workflow, got {target!r}")
        if target in ir["nodes"]:
            ir["on_budget_exceeded"] = target
            _warn(
                "on_budget_exceeded is compiled into the IR, but the runtime does not read it yet: "
                "a run that exceeds the budget fails"
            )
        else:
            # Leaving it out is the stricter reading: the budget still holds, and
            # a run that exceeds it fails instead of going anywhere else.
            _warn(
                f"on_budget_exceeded: {target!r} does not name a node of this workflow, so it is ignored "
                "and a run that exceeds the budget fails"
            )
    data_policy = _compile_data_policy(_governance_value(sources, "data_policy"), location="workflow")
    if data_policy is not None:
        ir["data_policy"] = data_policy
        _warn("data_policy is compiled into the IR, but the runtime does not enforce it yet")
    rollover = _governance_value(sources, "continue_as_new")
    if rollover is not None:
        if not isinstance(rollover, bool):
            raise ValueError(f"continue_as_new must be true or false, got {rollover!r}")
        ir["continue_as_new"] = rollover


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


def compile_to_workflow_spec(defn: WorkflowDef) -> WorkflowSpec:
    """Compile a WorkflowDef into a Pydantic WorkflowSpec.

    Wraps the existing dict-IR producer; preserves edge/node structure.
    """
    from jamjet.spec import EdgeSpec, NodeSpec, WorkflowSpec

    nodes: list[NodeSpec] = []
    for step in defn.steps:
        handler_ref = f"{step.fn.__module__}:{step.fn.__qualname__}"
        nodes.append(NodeSpec(id=step.name, handler_ref=handler_ref))

    edges: list[EdgeSpec] = []
    steps = defn.steps
    for i, step in enumerate(steps):
        if step.next:
            for target, _ in step.next.items():
                edges.append(EdgeSpec(from_node=step.name, to_node=target))
        else:
            next_name = steps[i + 1].name if i + 1 < len(steps) else "end"
            edges.append(EdgeSpec(from_node=step.name, to_node=next_name))

    return WorkflowSpec(
        name=defn.workflow_id,
        nodes=nodes,
        edges=edges,
        entry_node=defn.start_node,
    )


def compile_yaml(yaml_content: str) -> dict[str, Any]:
    """
    Compile a workflow.yaml string to the canonical IR dict.

    Supports two top-level schemas:

    **Graph/workflow mode** (manual nodes + edges)::

        workflow:
          id: ...
        nodes:
          <node_id>:
            type: model|tool|condition|human_approval|...

    **Agent-first mode** (strategy compiler)::

        agent:
          id: my-agent
          strategy: plan-and-execute
          goal: "..."
          tools: [search_web]
          model: gpt-4o
          limits:
            max_iterations: 6
            max_cost_usd: 0.50
            timeout_seconds: 120
    """
    data = yaml.safe_load(yaml_content)
    if not isinstance(data, dict):
        raise ValueError(f"a workflow document must be a mapping, got {type(data).__name__}")

    # ── Agent-first mode ──────────────────────────────────────────────────────
    if "agent" in data:
        return _compile_agent_yaml(data)

    if "agents" in data or "workflows" in data:
        raise ValueError(
            "this is a fleet file (agents:/workflows: maps); compile it with "
            "jamjet.workflow.bundle.compile_bundle, or use `jamjet validate`/`jamjet run`"
        )

    # ── Graph/workflow mode ───────────────────────────────────────────────────
    return _compile_graph_yaml(data)


def agent_ref(value: Any, *, location: str) -> str | None:
    """An ``agent_ref`` (the ``uri`` of a registered Agent Card) as written, or
    ``None`` when absent. The engine holds a workflow or node that names one to
    that card's constraints, and refuses it if the agent is not registered."""
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{location}: agent_ref must be a non-empty string (an Agent Card uri)")
    return value


def _compile_graph_yaml(data: dict[str, Any]) -> dict[str, Any]:
    """Compile a graph-mode document (``workflow:`` header + ``nodes:``) to IR.

    Raises ``ValueError`` for a document with no nodes, a malformed ``nodes:``
    block, a node of an unknown type, or a malformed or doubly-set governance
    setting. Keys the compiler does not read are dropped with a
    ``WorkflowSchemaWarning``; spellings with a real IR target (``system``,
    ``temperature``, ``arguments``, ...) are mapped to it.
    """
    wf = data.get("workflow") or {}
    if not isinstance(wf, dict):
        raise ValueError(f"workflow: must be a mapping, got {type(wf).__name__}")
    if "nodes" in wf:
        raise ValueError(
            "nodes: belongs at the top level of the document, not under workflow:. "
            "Write `nodes:` as a mapping of node id to node definition."
        )
    _warn_unknown_keys(data, _GRAPH_TOP_LEVEL_KEYS, location="top level of the document")
    _warn_unknown_keys(wf, _WORKFLOW_HEADER_KEYS, location="workflow header")
    raw_nodes = data.get("nodes") or {}
    if not isinstance(raw_nodes, dict):
        raise ValueError(f"nodes: must be a mapping of node id to node definition, got {type(raw_nodes).__name__}")

    # `id`/`version` at the top level stand for the header's.
    header = dict(wf)
    for key in ("id", "version"):
        if key in data:
            if key in header and header[key] != data[key]:
                raise ValueError(f"{key} is set at the top level and under workflow: with different values")
            header.setdefault(key, data[key])

    extras: dict[str, dict[str, Any]] = {"models": {}, "remote_agents": {}}
    normalized = {node_id: _normalize_node(node_id, node, extras) for node_id, node in raw_nodes.items()}
    end_ids = frozenset(nid for nid, (node_type, _) in normalized.items() if node_type == "end")
    if len(end_ids) == len(normalized):
        raise ValueError(f"workflow '{header.get('id', 'unknown')}' has no nodes to run")

    models = dict(data.get("models", {}) or {})
    declared_models = set(models)
    settings_by_model: dict[str, tuple[str, dict[str, Any]]] = {}
    for node_id, overrides in extras["models"].items():
        name = overrides["model"]
        wanted = {k: overrides[k] for k in ("temperature", "max_tokens") if overrides[k] is not None}
        if name in settings_by_model and settings_by_model[name][1] != wanted:
            raise ValueError(
                f"nodes '{settings_by_model[name][0]}' and '{node_id}' set different temperature/max_tokens "
                f"for model {name!r}; declare two entries under models: and name one in each node"
            )
        settings_by_model[name] = (node_id, wanted)
    for name, (node_id, wanted) in settings_by_model.items():
        others = [
            nid
            for nid, (t, d) in normalized.items()
            if t == "model" and d.get("model", "default_chat") == name and nid not in extras["models"]
        ]
        if others:
            raise ValueError(
                f"node '{node_id}' sets temperature/max_tokens for model {name!r}, which node '{others[0]}' uses "
                "without them; the runtime reads these per model, so declare an entry under models: instead"
            )
        entry = dict(models.get(name) or {"provider": "", "model": name})
        for key, value in wanted.items():
            if name in declared_models and key in entry and entry[key] != value:
                raise ValueError(f"node '{node_id}' sets {key}={value!r}, but models.{name} has {entry[key]!r}")
            entry[key] = value
        models[name] = entry
    remote_agents = dict((data.get("a2a", {}) or {}).get("remote_agents", {}) or {})
    for name, cfg in extras["remote_agents"].items():
        if name in remote_agents:
            raise ValueError(f"node '{name}' sets agent_url, but a2a.remote_agents already has '{name}'")
        remote_agents[name] = cfg

    # The runtime evaluates edge conditions only on condition nodes, so a
    # conditional `next` list on another node gets a condition node of its own.
    for node_id in list(normalized):
        node_type, node_data = normalized[node_id]
        next_val = node_data.get("next")
        if node_type in ("condition", "branch") or not isinstance(next_val, list):
            continue
        if not any(isinstance(e, dict) and e.get("when") is not None for e in next_val):
            continue
        route_id = f"{node_id}__route"
        if route_id in normalized:
            raise ValueError(f"node '{node_id}': cannot add the routing node '{route_id}', the name is taken")
        conditions: list[dict[str, Any]] = []
        default = None
        for i, entry in enumerate(next_val):
            target = entry.get("to", "end") if isinstance(entry, dict) else entry
            when = entry.get("when") if isinstance(entry, dict) else None
            if when is None:
                if default is not None:
                    raise ValueError(f"node '{node_id}': next[{i}] is a second unconditional entry")
                default = target
            else:
                conditions.append({"if": when, "next": target})
        route: dict[str, Any] = {"type": "condition", "conditions": conditions}
        if default is not None:
            route["default"] = default
        normalized[route_id] = ("condition", route)
        node_data["next"] = route_id

    nodes: dict[str, Any] = {}
    edges: list[dict[str, Any]] = []
    for node_id, (node_type, node_data) in normalized.items():
        if node_type == "end":
            continue
        kind = _yaml_node_to_kind(node_id, node_type, node_data, end_ids)
        node_policy = _compile_policy(node_data.get("policy"), location=f"node '{node_id}'")
        nodes[node_id] = {
            "id": node_id,
            "kind": kind,
            "retry_policy": node_data.get("retry_policy"),
            "node_timeout_secs": _parse_timeout(node_data.get("timeout")),
            "description": node_data.get("description"),
            "labels": node_data.get("labels", {}),
            "policy": node_policy,
        }
        node_data_policy = _compile_data_policy(node_data.get("data_policy"), location=f"node '{node_id}'")
        if node_data_policy is not None:
            nodes[node_id]["data_policy"] = node_data_policy
        node_agent = agent_ref(node_data.get("acts_for"), location=f"node '{node_id}' acts_for")
        if node_agent is not None:
            nodes[node_id]["agent_ref"] = node_agent

        next_val = node_data.get("next")
        if node_type in ("condition", "branch"):
            # The runtime routes a condition node along the out-edge matching
            # the branch it picks, and refuses a branch with no such edge.
            targets: list[str] = []
            for branch in kind.get("branches", []):
                if branch["target"] not in targets:
                    targets.append(branch["target"])
            for target in targets:
                edges.append({"from": node_id, "to": target, "condition": None})
            if next_val is None:
                continue
        if isinstance(next_val, str):
            edges.append({"from": node_id, "to": _term(next_val, end_ids), "condition": None})
        elif isinstance(next_val, list):
            for edge in next_val:
                if isinstance(edge, dict):
                    edges.append(
                        {
                            "from": node_id,
                            "to": _term(edge.get("to", "end"), end_ids),
                            "condition": edge.get("when"),
                        }
                    )
                elif edge == "end":
                    edges.append({"from": node_id, "to": "end", "condition": None})
        elif next_val == "end" or next_val is None:
            edges.append({"from": node_id, "to": "end", "condition": None})

    start_node = header.get("start", next(iter(nodes)) if nodes else "")
    _check_every_node_is_reachable(nodes, edges, start_node)
    workflow_agent = agent_ref(header.get("agent_ref"), location="workflow agent_ref")

    ir: dict[str, Any] = {
        "workflow_id": header.get("id", "unknown"),
        "version": header.get("version", DEFAULT_WORKFLOW_VERSION),
        "name": header.get("name"),
        "description": header.get("description"),
        "state_schema": _state_schema_str(header.get("state_schema", "")),
        "start_node": start_node,
        "nodes": nodes,
        "edges": edges,
        "retry_policies": data.get("retry_policies", {}),
        "timeouts": data.get("timeouts", {}),
        "models": models,
        "tools": data.get("tools", {}),
        "mcp_servers": (data.get("mcp", {}) or {}).get("servers", {}),
        "remote_agents": remote_agents,
        "labels": header.get("labels", {}),
    }
    _apply_governance(ir, [("the top level", data), ("the workflow: header", wf)])
    if workflow_agent is not None:
        ir["agent_ref"] = workflow_agent
    return ir


def _compile_agent_yaml(data: dict[str, Any]) -> dict[str, Any]:
    """
    Compile an agent-first YAML document to IR (§14.2).

    Required fields inside the ``agent:`` block:
        id, strategy, goal, model, limits.max_iterations,
        limits.max_cost_usd, limits.timeout_seconds

    Raises ``ValueError`` with an actionable message if required fields are
    missing (§14.5 / J2.10).
    """
    from jamjet.compiler.strategies import StrategyLimits, compile_strategy

    agent = data["agent"]
    if not isinstance(agent, dict):
        raise ValueError(f"agent: must be a mapping, got {type(agent).__name__}")
    _warn_unknown_keys(data, _AGENT_TOP_LEVEL_KEYS, location="top level of the document")
    _warn_unknown_keys(agent, _AGENT_KEYS, location="agent block")

    # ── Validate required fields ──────────────────────────────────────────────
    missing = [f for f in ("id", "strategy", "goal", "model") if not agent.get(f)]
    if missing:
        raise ValueError(f"agent-first workflow is missing required fields: {', '.join(missing)}")
    tools_raw = agent.get("tools") or []
    if not isinstance(tools_raw, list) or not all(isinstance(t, str) for t in tools_raw):
        raise ValueError(
            f"agent.tools must be a list of tool names (functions registered with @tool), got {tools_raw!r:.200}"
        )

    limits_raw = agent.get("limits")
    if not limits_raw:
        raise ValueError(
            "agent-first workflows require a 'limits' block (§14.5).\n"
            "Add:\n"
            "  limits:\n"
            "    max_iterations: 10\n"
            "    max_cost_usd: 0.50\n"
            "    timeout_seconds: 120"
        )

    for lf in ("max_iterations", "max_cost_usd", "timeout_seconds"):
        if lf not in limits_raw:
            raise ValueError(
                f"agent-first workflow limits block is missing '{lf}' (§14.5).\n"
                "All three limits are required: max_iterations, max_cost_usd, timeout_seconds."
            )

    limits = StrategyLimits(
        max_iterations=int(limits_raw["max_iterations"]),
        max_cost_usd=float(limits_raw["max_cost_usd"]),
        timeout_seconds=int(limits_raw["timeout_seconds"]),
    )

    agent_id = agent["id"]
    strategy_name = agent["strategy"]
    strategy_config = agent.get("strategy_config", {})
    tools = agent.get("tools", [])
    model = agent["model"]
    goal = agent["goal"]
    version = agent.get("version", DEFAULT_WORKFLOW_VERSION)

    # ── Compile strategy → IR sub-DAG ─────────────────────────────────────────
    compiled = compile_strategy(
        strategy_name=strategy_name,
        strategy_config=strategy_config,
        tools=tools,
        model=model,
        limits=limits,
        goal=goal,
        agent_id=agent_id,
    )

    raw_nodes = compiled["nodes"]
    raw_edges = compiled["edges"]
    start_node = compiled["start_node"]
    strategy_meta = compiled["strategy_metadata"]

    # Normalise nodes into IR format
    nodes: dict[str, Any] = {}
    for node_id, node_def in raw_nodes.items():
        nodes[node_id] = {
            "id": node_id,
            "kind": node_def["kind"],
            "retry_policy": node_def.get("retry_policy"),
            "node_timeout_secs": node_def.get("node_timeout_secs"),
            "description": node_def.get("description"),
            "labels": node_def.get("labels", {}),
        }

    out: dict[str, Any] = {
        "workflow_id": agent_id,
        "version": version,
        "name": agent.get("name", agent_id),
        "description": goal,
        "state_schema": agent.get("state_schema", ""),
        "start_node": start_node,
        "nodes": nodes,
        "edges": raw_edges,
        "retry_policies": {},
        "timeouts": {
            "workflow_timeout": limits.timeout_seconds,
            "heartbeat_interval": 30,
        },
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {
            "jamjet.strategy": strategy_name,
            "jamjet.agent.id": agent_id,
        },
        "strategy_metadata": strategy_meta,
    }
    _apply_governance(out, [("the top level", data), ("the agent: block", agent)])
    acts_as = agent_ref(agent.get("agent_ref"), location=f"agent '{agent_id}' agent_ref")
    if acts_as is not None:
        out["agent_ref"] = acts_as
    return out


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


def _term(target: Any, end_ids: frozenset) -> Any:
    """Rewrite an edge/branch target that names an `end`-type node to the terminal sentinel."""
    return "end" if target in end_ids else target


def _state_schema_str(value: Any) -> str:
    """The runtime IR ``state_schema`` is an opaque String; serialize an inline map to JSON."""
    if isinstance(value, str):
        return value
    if value:
        import json as _json

        return _json.dumps(value)
    return ""


def _yaml_node_to_kind(
    node_id: str, node_type: str, data: dict[str, Any], end_ids: frozenset = frozenset()
) -> dict[str, Any]:
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
    if node_type in ("condition", "branch"):
        branches: list[dict[str, Any]] = []
        for cond in data.get("conditions", []) or []:
            target = cond.get("next") or cond.get("to") or "end"
            branches.append({"condition": cond.get("if") or cond.get("when"), "target": _term(target, end_ids)})
        default = data.get("default")
        if default is not None:
            branches.append({"condition": None, "target": _term(default, end_ids)})
        return {"type": "condition", "branches": branches}
    if node_type == "eval":
        return {
            "type": "eval",
            "scorers": _compile_yaml_scorers(data.get("scorers", [])),
            "on_fail": data.get("on_fail", "halt"),
            "max_retries": data.get("max_retries", 0),
            "input_expr": data.get("input_expr"),
        }
    # Default fallback
    return {"type": node_type}


def _compile_yaml_scorers(scorers: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Convert YAML scorer definitions to the IR format expected by the Rust executor."""
    compiled = []
    for s in scorers:
        scorer_type = s.get("type", "")
        if scorer_type == "llm_judge":
            compiled.append(
                {
                    "type": "llm_judge",
                    "model": s.get("model", "default_chat"),
                    "rubric": s.get("rubric", ""),
                    "min_score": s.get("min_score", 3),
                }
            )
        elif scorer_type == "assertion":
            compiled.append(
                {
                    "type": "assertion",
                    "checks": s.get("checks", []),
                }
            )
        elif scorer_type == "latency":
            compiled.append(
                {
                    "type": "latency",
                    "threshold_ms": s.get("threshold_ms", 5000),
                }
            )
        elif scorer_type == "cost":
            compiled.append(
                {
                    "type": "cost",
                    "threshold_usd": s.get("threshold_usd", 1.0),
                }
            )
        elif scorer_type == "custom":
            compiled.append(
                {
                    "type": "custom",
                    "module": s.get("module", ""),
                    "kwargs": s.get("kwargs", {}),
                }
            )
        else:
            compiled.append(s)
    return compiled


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


# Known keys for the PolicySetIr struct (runtime/ir/src/workflow.rs).
_POLICY_KNOWN_KEYS = frozenset(
    {
        "blocked_tools",
        "require_approval_for",
        "model_allowlist",
        "approval_timeout_secs",
        "escalate_after_secs",
    }
)

# The longest approval deadline the runtime accepts (MAX_APPROVAL_TIMEOUT_SECS).
MAX_APPROVAL_TIMEOUT_SECS = 365 * 24 * 60 * 60


def compile_approval_deadline(raw: dict[str, Any], *, location: str = "policy") -> dict[str, int]:
    """The deadline keys of a policy, checked as the runtime checks them.

    ``approval_timeout_secs`` is a whole number of seconds from 1 to one year;
    ``escalate_after_secs`` needs a timeout in the same policy and is at least 1
    and less than it. Returns only the keys that are set: the runtime binds an
    approval to the node's serialized definition, so an unset deadline must not
    add keys. Raises ``ValueError`` otherwise, so a bad deadline fails at compile
    time rather than at registration.
    """
    out: dict[str, int] = {}
    for key in ("approval_timeout_secs", "escalate_after_secs"):
        if raw.get(key) is None:
            continue
        value = raw[key]
        if isinstance(value, bool) or not isinstance(value, int):
            raise ValueError(f"{location}: {key} must be a whole number of seconds, got {value!r}")
        out[key] = value
    timeout = out.get("approval_timeout_secs")
    escalate = out.get("escalate_after_secs")
    if timeout is not None and not 1 <= timeout <= MAX_APPROVAL_TIMEOUT_SECS:
        raise ValueError(
            f"{location}: approval_timeout_secs must be between 1 and {MAX_APPROVAL_TIMEOUT_SECS}, got {timeout}"
        )
    if escalate is not None:
        if timeout is None:
            raise ValueError(f"{location}: escalate_after_secs needs approval_timeout_secs in the same policy")
        if not 1 <= escalate < timeout:
            raise ValueError(
                f"{location}: escalate_after_secs must be at least 1 and less than "
                f"approval_timeout_secs ({timeout}), got {escalate}"
            )
    return out


def _compile_policy(raw: Any, *, location: str = "policy") -> dict[str, Any] | None:
    """Compile a raw YAML policy block to a PolicySetIr-compatible dict.

    Returns None when ``raw`` is None (policy absent).
    Raises ``ValueError`` when unknown keys are present.

    All three fields default to ``[]`` so the emitted JSON matches the
    ``gated_ir()`` fixture in ``runtime/api/tests/approval_e2e.rs`` exactly.
    """
    if raw is None:
        return None
    if not isinstance(raw, dict):
        raise ValueError(f"policy block at {location} must be a mapping, got {type(raw).__name__}")

    unknown = set(raw) - _POLICY_KNOWN_KEYS
    if unknown:
        bad = ", ".join(sorted(unknown))
        raise ValueError(f"Unknown key(s) in {location}: {bad}. Allowed keys: {', '.join(sorted(_POLICY_KNOWN_KEYS))}")

    return {
        "blocked_tools": list(raw.get("blocked_tools") or []),
        "require_approval_for": list(raw.get("require_approval_for") or []),
        "model_allowlist": list(raw.get("model_allowlist") or []),
        **compile_approval_deadline(raw, location=location),
    }
