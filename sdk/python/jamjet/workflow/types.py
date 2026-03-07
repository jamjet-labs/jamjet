"""Internal types for workflow definition."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any


@dataclass
class StepDef:
    """Internal representation of a @workflow.step definition."""

    name: str
    fn: Callable[..., Any]
    next: dict[str, Callable[..., bool]] = field(default_factory=dict)
    human_approval: bool = False
    timeout: str | None = None
    retry_policy: str | None = None
    model: str | None = None


@dataclass
class WorkflowDef:
    """Internal representation of a full workflow before IR compilation."""

    workflow_id: str
    version: str
    state_schema: str
    start_node: str
    steps: list[StepDef]
