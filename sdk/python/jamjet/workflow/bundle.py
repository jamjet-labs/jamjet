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


def compile_bundle(data: dict[str, Any]) -> CompiledBundle:
    """Compile a fleet document into a CompiledBundle."""
    bundle = CompiledBundle()
    return bundle
