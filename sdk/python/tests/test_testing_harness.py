"""Tests for the FakeJamjetClient and FakeEventBus test harness."""

from __future__ import annotations

import pytest

from jamjet.testing import FakeEventBus, FakeJamjetClient


def test_fake_client_create_workflow():
    client = FakeJamjetClient()
    ir = {"metadata": {"id": "wf-1", "version": "0.1.0"}, "nodes": [], "edges": []}
    result = client.create_workflow(ir)
    assert result["workflow_id"] == "wf-1"
    assert len(client.workflows) == 1


def test_fake_client_start_and_get_execution():
    client = FakeJamjetClient()
    ir = {"metadata": {"id": "wf-1", "version": "0.1.0"}, "nodes": [], "edges": []}
    client.create_workflow(ir)
    exec_result = client.start_execution("wf-1", {"input": "test"})
    assert "execution_id" in exec_result
    execution = client.get_execution(exec_result["execution_id"])
    assert execution["status"] == "completed"
    assert execution["input"] == {"input": "test"}


def test_fake_client_list_executions():
    client = FakeJamjetClient()
    ir = {"metadata": {"id": "wf-1", "version": "0.1.0"}, "nodes": [], "edges": []}
    client.create_workflow(ir)
    client.start_execution("wf-1", {"a": 1})
    client.start_execution("wf-1", {"b": 2})
    result = client.list_executions()
    assert len(result["executions"]) == 2


def test_fake_event_bus_captures_events():
    bus = FakeEventBus()
    bus.emit("exec-1", {"kind": "NodeStarted", "node_id": "step_1"})
    bus.emit("exec-1", {"kind": "NodeCompleted", "node_id": "step_1"})
    events = bus.events_for("exec-1")
    assert len(events) == 2
    assert events[0]["kind"] == "NodeStarted"


def test_fake_event_bus_filters_by_kind():
    bus = FakeEventBus()
    bus.emit("exec-1", {"kind": "NodeStarted", "node_id": "a"})
    bus.emit("exec-1", {"kind": "NodeCompleted", "node_id": "a"})
    bus.emit("exec-1", {"kind": "NodeStarted", "node_id": "b"})
    started = bus.events_for("exec-1", kind="NodeStarted")
    assert len(started) == 2


def test_fake_client_with_event_bus():
    bus = FakeEventBus()
    client = FakeJamjetClient(event_bus=bus)
    ir = {"metadata": {"id": "wf-1", "version": "0.1.0"}, "nodes": [], "edges": []}
    client.create_workflow(ir)
    client.start_execution("wf-1", {"x": 1})
    events = bus.all_events()
    assert len(events) > 0


def test_fake_event_bus_clear():
    bus = FakeEventBus()
    bus.emit("exec-1", {"kind": "NodeStarted", "node_id": "a"})
    assert len(bus.all_events()) == 1
    bus.clear()
    assert len(bus.all_events()) == 0


def test_fake_client_get_execution_not_found():
    client = FakeJamjetClient()
    with pytest.raises(KeyError, match="not-exist"):
        client.get_execution("not-exist")


def test_fake_client_health():
    client = FakeJamjetClient()
    result = client.health()
    assert result["status"] == "ok"


def test_fake_client_get_events():
    bus = FakeEventBus()
    client = FakeJamjetClient(event_bus=bus)
    ir = {"metadata": {"id": "wf-1", "version": "0.1.0"}, "nodes": [], "edges": []}
    client.create_workflow(ir)
    exec_result = client.start_execution("wf-1", {"x": 1})
    events_response = client.get_events(exec_result["execution_id"])
    assert "events" in events_response
    assert len(events_response["events"]) == 2  # ExecutionStarted + ExecutionCompleted


def test_fake_client_list_executions_filter_by_status():
    client = FakeJamjetClient()
    ir = {"metadata": {"id": "wf-1", "version": "0.1.0"}, "nodes": [], "edges": []}
    client.create_workflow(ir)
    client.start_execution("wf-1", {"a": 1})
    result = client.list_executions(status="completed")
    assert len(result["executions"]) == 1
    result = client.list_executions(status="running")
    assert len(result["executions"]) == 0


def test_fake_event_bus_events_for_unknown_execution():
    bus = FakeEventBus()
    events = bus.events_for("does-not-exist")
    assert events == []


def test_fixture_fake_client(fake_client):
    """Verify the pytest fixture provides a working FakeJamjetClient."""
    ir = {"metadata": {"id": "wf-1", "version": "0.1.0"}, "nodes": [], "edges": []}
    result = fake_client.create_workflow(ir)
    assert result["workflow_id"] == "wf-1"


def test_fixture_fake_event_bus(fake_event_bus):
    """Verify the pytest fixture provides a working FakeEventBus."""
    fake_event_bus.emit("exec-1", {"kind": "Test"})
    assert len(fake_event_bus.all_events()) == 1


def test_fixtures_are_wired(fake_client, fake_event_bus):
    """Verify the fake_client fixture uses the fake_event_bus fixture."""
    ir = {"metadata": {"id": "wf-1", "version": "0.1.0"}, "nodes": [], "edges": []}
    fake_client.create_workflow(ir)
    fake_client.start_execution("wf-1", {"x": 1})
    # Events should appear on the shared event bus
    assert len(fake_event_bus.all_events()) > 0
