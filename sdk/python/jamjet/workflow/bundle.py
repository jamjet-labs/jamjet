"""
Compile a multi-unit ("fleet") YAML document into N workflow IRs plus cron specs.

A fleet file has an ``agents:`` map (strategy-based units) and/or a
``workflows:`` map (explicit node graphs). Both kinds compile to the same
``WorkflowIr`` dict and share a top-level ``tools:``/``mcp:`` catalog. Any unit
may carry a ``schedule:`` (5-field cron) that becomes a cron job.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class CronSpec:
    """Cron registration payload for a single scheduled workflow unit."""

    name: str
    cron_expression: str
    workflow_id: str
    workflow_version: str
    input: dict[str, Any] = field(default_factory=dict)
    enabled: bool = True


@dataclass
class CompiledBundle:
    """Output of compile_bundle: compiled workflow IRs and their associated cron jobs."""

    workflows: list[dict[str, Any]] = field(default_factory=list)
    cron_jobs: list[CronSpec] = field(default_factory=list)


def is_bundle(data: dict[str, Any]) -> bool:
    """A multi-unit file has an ``agents:`` and/or ``workflows:`` (plural) map."""
    return isinstance(data, dict) and ("agents" in data or "workflows" in data)


def _validate_cron(expr: str) -> None:
    """Light client-side check; the runtime's cron_next is authoritative."""
    if not isinstance(expr, str) or len(expr.split()) != 5:
        raise ValueError(
            f"cron expression must have 5 fields (minute hour day-of-month month day-of-week), got: {expr!r}"
        )


def _schedule_to_spec(unit_id: str, version: str, schedule: dict[str, Any]) -> CronSpec:
    """Convert a unit's ``schedule:`` block to a CronSpec, validating the expression and timezone."""
    cron = schedule.get("cron")
    if not cron:
        raise ValueError(f"unit '{unit_id}' has a schedule with no 'cron' field")
    _validate_cron(cron)
    tz = schedule.get("timezone", "UTC")
    if tz != "UTC":
        raise ValueError(f"unit '{unit_id}': only timezone 'UTC' is supported in this version (got {tz!r})")
    return CronSpec(
        name=unit_id,
        cron_expression=cron,
        workflow_id=unit_id,
        workflow_version=version,
        input=schedule.get("input", {}) or {},
        enabled=bool(schedule.get("enabled", True)),
    )


@dataclass
class _ResolvedUses:
    """Resolved tool and MCP references for a single agent unit."""

    tool_names: list[str] = field(default_factory=list)
    ir_tools: dict[str, Any] = field(default_factory=dict)
    mcp_servers: dict[str, Any] = field(default_factory=dict)
    sibling_refs: list[str] = field(default_factory=list)


def _tool_name(t: Any) -> str:
    """Inline tools may be a {name,...} dict or a bare string name."""
    if isinstance(t, dict):
        name = t.get("name")
        if not name:
            raise ValueError(f"inline tool is missing a 'name': {t!r}")
        return str(name)
    return str(t)


def _resolve_uses(
    unit_id: str,
    uses: list[str],
    inline_tools: list[Any],
    tool_catalog: dict[str, Any],
    mcp_catalog: dict[str, Any],
    unit_ids: set[str],
) -> _ResolvedUses:
    """Resolve a unit's ``uses:`` refs and inline ``tools:`` against the top-level catalogs."""
    r = _ResolvedUses()

    for t in inline_tools or []:
        name = _tool_name(t)
        r.tool_names.append(name)
        if isinstance(t, dict):
            r.ir_tools[name] = {k: v for k, v in t.items() if k != "name"}

    for ref in uses or []:
        if not isinstance(ref, str) or ":" not in ref:
            raise ValueError(f"unit '{unit_id}': unknown ref {ref!r} (expected tool:/mcp:/agent:/workflow: prefix)")
        kind, _, name = ref.partition(":")
        if kind == "tool":
            if name not in tool_catalog:
                raise ValueError(f"unit '{unit_id}': unknown tool '{name}' (not in top-level tools:)")
            r.tool_names.append(name)
            r.ir_tools[name] = tool_catalog[name]
        elif kind == "mcp":
            if name not in mcp_catalog:
                raise ValueError(f"unit '{unit_id}': unknown mcp server '{name}' (not in top-level mcp.servers:)")
            r.mcp_servers[name] = mcp_catalog[name]
        elif kind in ("agent", "workflow"):
            if name not in unit_ids:
                raise ValueError(f"unit '{unit_id}': unknown unit '{name}' referenced via {ref!r}")
            r.tool_names.append(name)
            r.sibling_refs.append(name)
        else:
            raise ValueError(f"unit '{unit_id}': unknown ref {ref!r} (expected tool:/mcp:/agent:/workflow: prefix)")

    return r


def _compile_agent_unit(
    unit_id: str,
    agent: dict[str, Any],
    defaults: dict[str, Any],
    tool_catalog: dict[str, Any],
    mcp_catalog: dict[str, Any],
    unit_ids: set[str],
) -> dict[str, Any]:
    """Compile a single strategy-based agent unit into a workflow IR dict."""
    from jamjet.compiler.strategies import StrategyLimits, compile_strategy

    strategy_name = agent.get("strategy")
    goal = agent.get("goal")
    model = agent.get("model") or defaults.get("model")
    missing = [n for n, v in (("strategy", strategy_name), ("goal", goal), ("model", model)) if not v]
    if missing:
        raise ValueError(f"agent unit '{unit_id}' is missing required fields: {', '.join(missing)}")

    limits_raw = {**defaults.get("limits", {}), **agent.get("limits", {})}
    for lf in ("max_iterations", "max_cost_usd", "timeout_seconds"):
        if lf not in limits_raw:
            raise ValueError(f"agent unit '{unit_id}' limits is missing '{lf}'")
    limits = StrategyLimits(
        max_iterations=int(limits_raw["max_iterations"]),
        max_cost_usd=float(limits_raw["max_cost_usd"]),
        timeout_seconds=int(limits_raw["timeout_seconds"]),
    )

    resolved = _resolve_uses(
        unit_id,
        agent.get("uses", []),
        agent.get("tools", []),
        tool_catalog,
        mcp_catalog,
        unit_ids,
    )

    compiled = compile_strategy(
        strategy_name=strategy_name,
        strategy_config=agent.get("strategy_config", {}),
        tools=resolved.tool_names,
        model=model,
        limits=limits,
        goal=goal,
        agent_id=unit_id,
    )

    nodes: dict[str, Any] = {}
    for node_id, node_def in compiled["nodes"].items():
        nodes[node_id] = {
            "id": node_id,
            "kind": node_def["kind"],
            "retry_policy": node_def.get("retry_policy"),
            "node_timeout_secs": node_def.get("node_timeout_secs"),
            "description": node_def.get("description"),
            "labels": node_def.get("labels", {}),
        }

    version = str(agent.get("version") or defaults.get("version") or "0.1.0")
    return {
        "workflow_id": unit_id,
        "version": version,
        "name": agent.get("name", unit_id),
        "description": goal,
        "state_schema": agent.get("state_schema", ""),
        "start_node": compiled["start_node"],
        "nodes": nodes,
        "edges": compiled["edges"],
        "retry_policies": {},
        "timeouts": {"workflow_timeout": limits.timeout_seconds, "heartbeat_interval": 30},
        "models": {},
        # The runtime IR `tools`/`mcp_servers` maps expect typed configs
        # (ToolConfig with `kind`/`reference`, McpServerConfig with a typed
        # `transport` enum) that differ from the catalog's human-authored
        # shapes. Strategy agents convey tool names via the compiled prompt
        # (`resolved.tool_names` → `compile_strategy`); deep tool/MCP IR
        # wiring is deferred. Emit empty maps so POST /workflows succeeds.
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {
            "jamjet.strategy": strategy_name,
            "jamjet.agent.id": unit_id,
        },
        "strategy_metadata": compiled["strategy_metadata"],
    }


def _detect_cycle(graph: dict[str, list[str]]) -> list[str] | None:
    """Return the first cycle found in the sibling-reference graph, or None if acyclic."""
    white, grey, black = 0, 1, 2
    color = {n: white for n in graph}
    stack: list[str] = []

    def visit(n: str) -> list[str] | None:
        """DFS visitor; returns the cycle path if one is found starting from node n."""
        color[n] = grey
        stack.append(n)
        for m in graph.get(n, []):
            if color.get(m, black) == grey:
                return stack[stack.index(m) :] + [m]
            if color.get(m, black) == white:
                found = visit(m)
                if found:
                    return found
        stack.pop()
        color[n] = black
        return None

    for n in graph:
        if color[n] == white:
            found = visit(n)
            if found:
                return found
    return None


def compile_bundle(data: dict[str, Any]) -> CompiledBundle:
    """Compile a fleet document into a CompiledBundle."""
    defaults = data.get("defaults", {})
    tool_catalog = data.get("tools", {})
    mcp_catalog = data.get("mcp", {}).get("servers", {})
    agents = data.get("agents", {}) or {}
    workflows = data.get("workflows", {}) or {}

    # Unique ids across both maps.
    unit_ids: set[str] = set()
    for uid in list(agents) + list(workflows):
        if uid in unit_ids:
            raise ValueError(f"duplicate unit id '{uid}' (ids must be unique across agents: and workflows:)")
        unit_ids.add(uid)

    bundle = CompiledBundle()
    sibling_graph: dict[str, list[str]] = {uid: [] for uid in unit_ids}

    # Agent units.
    for uid, agent in agents.items():
        resolved = _resolve_uses(
            uid,
            agent.get("uses", []),
            agent.get("tools", []),
            tool_catalog,
            mcp_catalog,
            unit_ids,
        )
        sibling_graph[uid] = resolved.sibling_refs
        ir = _compile_agent_unit(uid, agent, defaults, tool_catalog, mcp_catalog, unit_ids)
        bundle.workflows.append(ir)
        if "schedule" in agent:
            bundle.cron_jobs.append(_schedule_to_spec(uid, ir["version"], agent["schedule"]))

    # Workflow units (explicit graphs) reuse the graph compiler with catalog access.
    for uid, wf in workflows.items():
        header = {"id": uid, "version": wf.get("version", defaults.get("version", "0.1.0"))}
        for key in ("name", "description", "start", "state_schema", "labels"):
            if key in wf:
                header[key] = wf[key]
        doc = {
            "workflow": header,
            "nodes": wf.get("nodes", {}),
            "retry_policies": wf.get("retry_policies", {}),
            "timeouts": wf.get("timeouts", {}),
            "models": wf.get("models", {}),
            "tools": tool_catalog,
            "mcp": {"servers": mcp_catalog},
            "a2a": {"remote_agents": wf.get("remote_agents", {})},
        }
        from jamjet.workflow.ir_compiler import _compile_graph_yaml

        ir = _compile_graph_yaml(doc)
        ir["workflow_id"] = uid
        ir["version"] = str(ir["version"])
        bundle.workflows.append(ir)
        if "schedule" in wf:
            bundle.cron_jobs.append(_schedule_to_spec(uid, ir["version"], wf["schedule"]))

    cycle = _detect_cycle(sibling_graph)
    if cycle:
        raise ValueError(f"cycle in agent references: {' -> '.join(cycle)}")

    return bundle
