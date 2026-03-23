"""Test utilities for JamJet workflows.

Provides in-memory fakes for testing without a live runtime server:

- ``FakeEventBus``  — captures emitted events for assertion.
- ``FakeJamjetClient`` — in-memory mock of :class:`JamjetClient`.

Usage::

    from jamjet.testing import FakeJamjetClient, FakeEventBus

    bus = FakeEventBus()
    client = FakeJamjetClient(event_bus=bus)
    ir = {"metadata": {"id": "wf-1", "version": "0.1.0"}, "nodes": [], "edges": []}
    client.create_workflow(ir)
    exec_result = client.start_execution("wf-1", {"input": "hello"})
    assert bus.all_events()  # events were recorded
"""

from __future__ import annotations

import uuid
from typing import Any


class FakeEventBus:
    """In-memory event bus that captures events for assertion."""

    def __init__(self) -> None:
        self._events: dict[str, list[dict[str, Any]]] = {}

    def emit(self, execution_id: str, event: dict[str, Any]) -> None:
        """Record an event for the given execution."""
        self._events.setdefault(execution_id, []).append(event)

    def events_for(self, execution_id: str, *, kind: str | None = None) -> list[dict[str, Any]]:
        """Return events for *execution_id*, optionally filtered by *kind*."""
        events = self._events.get(execution_id, [])
        if kind is not None:
            return [e for e in events if e.get("kind") == kind]
        return list(events)

    def all_events(self) -> list[dict[str, Any]]:
        """Return every recorded event across all executions."""
        return [e for events in self._events.values() for e in events]

    def clear(self) -> None:
        """Discard all recorded events."""
        self._events.clear()


class FakeJamjetClient:
    """In-memory mock of JamjetClient for testing without a runtime server."""

    def __init__(self, *, event_bus: FakeEventBus | None = None) -> None:
        self.workflows: dict[str, dict[str, Any]] = {}
        self.executions: dict[str, dict[str, Any]] = {}
        self._event_bus = event_bus or FakeEventBus()

    def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
        """Register a workflow IR and return its ID."""
        wf_id = ir.get("metadata", {}).get("id", str(uuid.uuid4()))
        self.workflows[wf_id] = ir
        return {"workflow_id": wf_id}

    def start_execution(
        self,
        workflow_id: str,
        input_data: dict[str, Any],
        workflow_version: str | None = None,
    ) -> dict[str, Any]:
        """Simulate starting an execution (completes immediately)."""
        exec_id = str(uuid.uuid4())
        self.executions[exec_id] = {
            "execution_id": exec_id,
            "workflow_id": workflow_id,
            "status": "completed",
            "input": input_data,
            "output": input_data,
        }
        self._event_bus.emit(exec_id, {"kind": "ExecutionStarted", "workflow_id": workflow_id})
        self._event_bus.emit(exec_id, {"kind": "ExecutionCompleted", "workflow_id": workflow_id})
        return {"execution_id": exec_id}

    def get_execution(self, execution_id: str) -> dict[str, Any]:
        """Return execution state, or raise ``KeyError`` if not found."""
        if execution_id not in self.executions:
            raise KeyError(f"Execution {execution_id} not found")
        return self.executions[execution_id]

    def list_executions(
        self,
        status: str | None = None,
        limit: int = 50,
        offset: int = 0,
    ) -> dict[str, Any]:
        """List recorded executions with optional status filter and pagination."""
        execs = list(self.executions.values())
        if status:
            execs = [e for e in execs if e["status"] == status]
        return {"executions": execs[offset : offset + limit]}

    def get_events(self, execution_id: str) -> dict[str, Any]:
        """Return all events for an execution."""
        return {"events": self._event_bus.events_for(execution_id)}

    def health(self) -> dict[str, Any]:
        """Return a fake health-check response."""
        return {"status": "ok", "version": "test"}
