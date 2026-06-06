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
    name: str
    cron_expression: str
    workflow_id: str
    workflow_version: str
    input: dict[str, Any] = field(default_factory=dict)
    enabled: bool = True


@dataclass
class CompiledBundle:
    workflows: list[dict[str, Any]] = field(default_factory=list)
    cron_jobs: list[CronSpec] = field(default_factory=list)


def is_bundle(data: dict[str, Any]) -> bool:
    """A multi-unit file has an ``agents:`` and/or ``workflows:`` (plural) map."""
    return isinstance(data, dict) and ("agents" in data or "workflows" in data)


def _validate_cron(expr: str) -> None:
    """Light client-side check; the runtime's cron_next is authoritative."""
    if not isinstance(expr, str) or len(expr.split()) != 5:
        raise ValueError(
            f"cron expression must have 5 fields "
            f"(minute hour day-of-month month day-of-week), got: {expr!r}"
        )


def _schedule_to_spec(unit_id: str, version: str, schedule: dict[str, Any]) -> CronSpec:
    cron = schedule.get("cron")
    if not cron:
        raise ValueError(f"unit '{unit_id}' has a schedule with no 'cron' field")
    _validate_cron(cron)
    tz = schedule.get("timezone", "UTC")
    if tz != "UTC":
        raise ValueError(
            f"unit '{unit_id}': only timezone 'UTC' is supported in this version "
            f"(got {tz!r})"
        )
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
    r = _ResolvedUses()

    for t in inline_tools or []:
        name = _tool_name(t)
        r.tool_names.append(name)
        if isinstance(t, dict):
            r.ir_tools[name] = {k: v for k, v in t.items() if k != "name"}

    for ref in uses or []:
        if not isinstance(ref, str) or ":" not in ref:
            raise ValueError(
                f"unit '{unit_id}': unknown ref {ref!r} "
                f"(expected tool:/mcp:/agent:/workflow: prefix)"
            )
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
            raise ValueError(
                f"unit '{unit_id}': unknown ref {ref!r} "
                f"(expected tool:/mcp:/agent:/workflow: prefix)"
            )

    return r


def compile_bundle(data: dict[str, Any]) -> CompiledBundle:
    """Compile a fleet document into a CompiledBundle."""
    bundle = CompiledBundle()
    return bundle
