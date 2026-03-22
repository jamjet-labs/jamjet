"""
Local in-process workflow executor.

Runs a compiled workflow graph without needing the Rust runtime or ``jamjet dev``.
This is the in-process execution path that makes JamJet work like any other
Python library — ``workflow.run(state)`` just works.

For production deployments, submit the compiled IR to the Rust runtime
for durable, crash-safe execution.  The local executor is designed for:

- Development and testing
- Benchmarking
- Simple single-process agent deployments
- CI/CD pipelines
"""

from __future__ import annotations

import asyncio
import inspect
import time
from dataclasses import dataclass, field
from typing import Any

from pydantic import BaseModel


@dataclass
class ExecutionEvent:
    """A single event from the execution timeline."""

    step: str
    timestamp_ns: int
    duration_us: float
    status: str  # "completed" | "skipped" | "error"
    error: str | None = None

    def to_dict(self, *, include_timing: bool = False) -> dict[str, Any]:
        """Serialize for snapshot comparison. Excludes timing by default for determinism."""
        d: dict[str, Any] = {"step": self.step, "status": self.status}
        if self.error:
            d["error"] = self.error
        if include_timing:
            d["timestamp_ns"] = self.timestamp_ns
            d["duration_us"] = self.duration_us
        return d


@dataclass
class ExecutionResult:
    """Result of a local workflow execution."""

    state: BaseModel
    events: list[ExecutionEvent] = field(default_factory=list)
    steps_executed: int = 0
    total_duration_us: float = 0.0

    def to_snapshot(self, *, include_timing: bool = False) -> dict[str, Any]:
        """Serialize execution trace for snapshot comparison."""
        return {
            "state": self.state.model_dump() if hasattr(self.state, "model_dump") else str(self.state),
            "steps_executed": self.steps_executed,
            "events": [e.to_dict(include_timing=include_timing) for e in self.events],
        }

    def __str__(self) -> str:
        return str(self.state)

    def __repr__(self) -> str:
        return f"ExecutionResult(steps={self.steps_executed}, duration={self.total_duration_us:.1f}µs)"


async def execute_workflow(
    steps: list[Any],  # list[StepDef]
    initial_state: BaseModel,
    max_steps: int = 100,
) -> ExecutionResult:
    """
    Execute a workflow's step functions in-process.

    Walks the step chain: for each step, calls the async function with
    current state and gets back new state.  Supports conditional routing
    via ``step.next`` predicates.

    Parameters
    ----------
    steps
        List of StepDef from the Workflow.
    initial_state
        The initial Pydantic state model.
    max_steps
        Safety limit to prevent infinite loops.

    Returns
    -------
    ExecutionResult
        Final state + execution timeline.
    """
    # Build lookup tables
    step_map: dict[str, Any] = {s.name: s for s in steps}
    step_order: list[str] = [s.name for s in steps]

    # Identify steps that are conditional branch targets — these should not
    # fall through to the next sequential step unless they define their own
    # explicit routing.
    branch_targets: set[str] = set()
    for s in steps:
        if s.next:
            for target in s.next:
                branch_targets.add(target)

    state = initial_state
    events: list[ExecutionEvent] = []
    current = step_order[0] if step_order else None
    executed = 0
    t_start = time.perf_counter_ns()

    while current and current != "end" and executed < max_steps:
        step_def = step_map.get(current)
        if step_def is None:
            break

        step_start = time.perf_counter_ns()
        try:
            result = step_def.fn(state)
            if inspect.isawaitable(result):
                result = await result
            state = result
            duration_us = (time.perf_counter_ns() - step_start) / 1000
            events.append(
                ExecutionEvent(
                    step=current,
                    timestamp_ns=step_start,
                    duration_us=duration_us,
                    status="completed",
                )
            )
        except Exception as exc:
            duration_us = (time.perf_counter_ns() - step_start) / 1000
            events.append(
                ExecutionEvent(
                    step=current,
                    timestamp_ns=step_start,
                    duration_us=duration_us,
                    status="error",
                    error=str(exc),
                )
            )
            raise

        executed += 1

        # Determine next step
        next_step = _resolve_next(step_def, state, step_order, current, branch_targets)
        current = next_step

    total_us = (time.perf_counter_ns() - t_start) / 1000
    return ExecutionResult(
        state=state,
        events=events,
        steps_executed=executed,
        total_duration_us=total_us,
    )


def execute_workflow_sync(
    steps: list[Any],
    initial_state: BaseModel,
    max_steps: int = 100,
) -> ExecutionResult:
    """Synchronous wrapper around :func:`execute_workflow`."""
    return asyncio.run(execute_workflow(steps, initial_state, max_steps))


def _resolve_next(
    step_def: Any,
    state: BaseModel,
    step_order: list[str],
    current: str,
    branch_targets: set[str],
) -> str | None:
    """Resolve the next step to execute."""
    # Check explicit conditional routing
    if step_def.next:
        for target, predicate in step_def.next.items():
            try:
                if predicate(state):
                    return target
            except Exception:
                continue
        # If no predicate matched, fall through to sequential
        return _next_in_order(step_order, current)

    # If this step is a conditional branch target (was jumped-to, not
    # reached sequentially), treat it as terminal — don't fall through
    # to the next step in declaration order.
    if current in branch_targets:
        return None

    # Default: next step in declaration order
    return _next_in_order(step_order, current)


def _next_in_order(step_order: list[str], current: str) -> str | None:
    """Return the next step in declaration order, or None if last."""
    try:
        idx = step_order.index(current)
        if idx + 1 < len(step_order):
            return step_order[idx + 1]
    except ValueError:
        pass
    return None
