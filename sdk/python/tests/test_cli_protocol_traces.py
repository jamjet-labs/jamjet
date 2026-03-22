"""Tests for protocol-level trace CLI views."""

from jamjet.cli.main import _filter_events, _build_protocol_tree


def test_filter_events_by_protocol_mcp():
    events = [
        {"kind": "NodeStarted", "node_id": "step_1"},
        {"kind": "AgentToolInvoked", "node_id": "mcp_call", "protocol": "mcp"},
        {"kind": "AgentToolCompleted", "node_id": "mcp_call", "protocol": "mcp"},
        {"kind": "AgentToolInvoked", "node_id": "a2a_call", "protocol": "a2a"},
    ]
    filtered = _filter_events(events, protocol="mcp")
    assert len(filtered) == 2
    assert all(e.get("protocol") == "mcp" for e in filtered)


def test_filter_events_by_node():
    events = [
        {"kind": "NodeStarted", "node_id": "step_1"},
        {"kind": "NodeCompleted", "node_id": "step_1"},
        {"kind": "NodeStarted", "node_id": "step_2"},
    ]
    filtered = _filter_events(events, node="step_1")
    assert len(filtered) == 2
    assert all(e.get("node_id") == "step_1" for e in filtered)


def test_filter_events_by_protocol_and_node():
    events = [
        {"kind": "AgentToolInvoked", "node_id": "call_1", "protocol": "mcp"},
        {"kind": "AgentToolInvoked", "node_id": "call_2", "protocol": "mcp"},
        {"kind": "AgentToolInvoked", "node_id": "call_1", "protocol": "a2a"},
    ]
    filtered = _filter_events(events, protocol="mcp", node="call_1")
    assert len(filtered) == 1


def test_build_protocol_tree_groups_by_invocation():
    events = [
        {"kind": "AgentToolInvoked", "node_id": "call_1", "protocol": "a2a", "sequence": 1},
        {"kind": "AgentToolProgress", "node_id": "call_1", "sequence": 2},
        {"kind": "AgentToolTurn", "node_id": "call_1", "turn": 1, "sequence": 3},
        {"kind": "AgentToolCompleted", "node_id": "call_1", "sequence": 4},
    ]
    tree = _build_protocol_tree(events)
    assert "call_1" in tree
    assert len(tree["call_1"]) == 4


def test_filter_events_no_filters():
    events = [{"kind": "A"}, {"kind": "B"}]
    assert _filter_events(events) == events


def test_build_protocol_tree_empty():
    assert _build_protocol_tree([]) == {}


def test_build_protocol_tree_skips_events_without_node_id():
    events = [{"kind": "ExecutionStarted"}, {"kind": "NodeStarted", "node_id": "a"}]
    tree = _build_protocol_tree(events)
    assert "a" in tree
    assert len(tree) == 1
