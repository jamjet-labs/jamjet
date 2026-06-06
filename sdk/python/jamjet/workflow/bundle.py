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


def compile_bundle(data: dict[str, Any]) -> CompiledBundle:
    """Compile a fleet document into a CompiledBundle."""
    bundle = CompiledBundle()
    return bundle
