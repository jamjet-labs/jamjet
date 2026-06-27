"""Structural tests for the agent-loop IR builder (Track 2j-3).

No engine: assert the compiled ``WorkflowIr`` dict has the model/condition/
tool-dispatch nodes, the correct edges (model -> gate -> (tools -> next | end)),
and the canonical top-level shape the Rust engine deserializes.
"""

from __future__ import annotations

import pytest

from jamjet.agents.agent import Agent
from jamjet.compiler.agent_ir import build_initial_state, compile_agent_to_ir
from jamjet.tools.decorators import tool


@tool
async def get_weather(city: str) -> str:
    """Return the weather for a city."""
    return f"sunny in {city}"


@tool
async def search_web(query: str, limit: int = 5) -> str:
    """Search the web."""
    return f"results for {query} (limit {limit})"


def _agent() -> Agent:
    return Agent(
        "research",
        model="anthropic/claude-sonnet-4-6",
        tools=[get_weather, search_web],
        instructions="You are a research assistant.",
    )


# ── Top-level shape ───────────────────────────────────────────────────────────

# The canonical WorkflowIr keys produced by jamjet.workflow.ir_compiler — the
# shape the Rust engine deserializes (there is no Python validator for the
# kind-based IR, so we assert required keys + node/edge invariants).
_REQUIRED_KEYS = {
    "workflow_id",
    "version",
    "name",
    "description",
    "state_schema",
    "start_node",
    "nodes",
    "edges",
    "retry_policies",
    "timeouts",
    "models",
    "tools",
    "mcp_servers",
    "remote_agents",
    "labels",
}


def test_ir_has_required_top_level_keys():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    assert _REQUIRED_KEYS <= set(ir), f"missing: {_REQUIRED_KEYS - set(ir)}"
    assert ir["start_node"] == "__model_0__"
    assert ir["start_node"] in ir["nodes"]
    assert ir["workflow_id"] == "research"


def test_node_and_edge_invariants():
    """Every node is normalised; every edge target is a known node or `end`."""
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    node_ids = set(ir["nodes"]) | {"end"}
    for node_id, node in ir["nodes"].items():
        assert node["id"] == node_id
        assert "kind" in node and "type" in node["kind"]
        assert {"retry_policy", "node_timeout_secs", "description", "labels"} <= set(node)
    for e in ir["edges"]:
        assert e["from"] in ir["nodes"], f"edge from unknown node: {e['from']}"
        assert e["to"] in node_ids, f"edge to unknown node: {e['to']}"


# ── Model nodes ───────────────────────────────────────────────────────────────


def test_three_model_nodes_each_carry_both_tool_schemas():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    model_ids = [f"__model_{t}__" for t in range(3)]
    assert all(mid in ir["nodes"] for mid in model_ids)
    for mid in model_ids:
        kind = ir["nodes"][mid]["kind"]
        assert kind["type"] == "model"
        assert kind["model_ref"] == "anthropic/claude-sonnet-4-6"
        assert kind["system_prompt"] == "You are a research assistant."
        # Empty prompt_ref => the executor reads `messages` from state.
        assert kind["prompt_ref"] == ""
        names = [s["function"]["name"] for s in kind["tools"]]
        assert names == ["get_weather", "search_web"]
        # OpenAI function-schema shape; parameters are the tool's input_schema
        # verbatim (this SDK's @tool maps a str param to the bare token "string").
        assert kind["tools"][0]["type"] == "function"
        params = kind["tools"][0]["function"]["parameters"]
        assert params["type"] == "object"
        assert params["properties"]["city"] == "string"
        assert params["required"] == ["city"]


# ── Condition gates ───────────────────────────────────────────────────────────


def test_three_condition_gates_branch_on_tool_calls():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    expr = 'state.last_model_finish_reason == "tool_calls"'
    for t in range(3):
        kind = ir["nodes"][f"__tool_gate_{t}__"]["kind"]
        assert kind["type"] == "condition"
        branches = kind["branches"]
        # true branch -> the turn's tool node; default branch -> terminal.
        assert branches[0] == {"condition": expr, "target": f"__tools_{t}__"}
        assert branches[1] == {"condition": None, "target": "end"}


# ── Tool-dispatch PythonFn nodes ──────────────────────────────────────────────


def test_three_tool_dispatch_nodes_with_resolver_map():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    for t in range(3):
        kind = ir["nodes"][f"__tools_{t}__"]["kind"]
        assert kind["type"] == "python_fn"
        assert kind["module"] == "jamjet.agents.tool_runtime"
        assert kind["function"] == "dispatch_tool_calls"
        tools_map = kind["input"]["tools"]
        assert set(tools_map) == {"get_weather", "search_web"}
        # name -> "module:qualname" (mirrors Agent.compile handler_ref).
        assert tools_map["get_weather"].endswith(":get_weather")
        assert tools_map["search_web"].endswith(":search_web")
        assert ":" in tools_map["get_weather"]
        # The dispatch input references the 2j-2 state keys.
        assert kind["input"]["tool_calls"] == "$state.last_model_tool_calls"
        assert kind["input"]["assistant_content"] == "$state.last_model_output"
        assert kind["input"]["messages"] == "$state.messages"


# ── Edges / loop topology ─────────────────────────────────────────────────────


def _edge_set(ir: dict) -> set[tuple[str, str]]:
    return {(e["from"], e["to"]) for e in ir["edges"]}


def test_loop_edges_route_model_gate_tools_next():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    edges = _edge_set(ir)
    # model -> gate, gate -> tools, gate -> end for every turn.
    for t in range(3):
        assert (f"__model_{t}__", f"__tool_gate_{t}__") in edges
        assert (f"__tool_gate_{t}__", f"__tools_{t}__") in edges
        assert (f"__tool_gate_{t}__", "end") in edges
    # tools loop forward to the next model node...
    assert (f"__tools_{0}__", "__model_1__") in edges
    assert (f"__tools_{1}__", "__model_2__") in edges
    # ...except the last turn, which is bounded -> end.
    assert (f"__tools_{2}__", "end") in edges
    assert ("__tools_2__", "__model_3__") not in edges
    assert "__model_3__" not in ir["nodes"]


def test_terminal_is_reachable_end_sentinel():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=2)
    targets = {e["to"] for e in ir["edges"]}
    assert "end" in targets
    assert "end" not in ir["nodes"]  # `end` is the graph terminal sentinel


def test_node_counts_scale_with_max_turns():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=4)
    kinds = [n["kind"]["type"] for n in ir["nodes"].values()]
    assert kinds.count("model") == 4
    assert kinds.count("condition") == 4
    assert kinds.count("python_fn") == 4


# ── max_turns=1 + validation ──────────────────────────────────────────────────


def test_single_turn_dispatch_routes_to_end():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=1)
    assert set(ir["nodes"]) == {"__model_0__", "__tool_gate_0__", "__tools_0__"}
    assert ("__tools_0__", "end") in _edge_set(ir)


def test_max_turns_must_be_positive():
    with pytest.raises(ValueError, match="max_turns"):
        compile_agent_to_ir(_agent(), "hi", max_turns=0)


# ── Initial state ─────────────────────────────────────────────────────────────


def test_build_initial_state_seeds_messages_and_tools():
    state = build_initial_state(_agent(), "what's the weather?")
    assert state["messages"][0] == {"role": "system", "content": "You are a research assistant."}
    assert state["messages"][1] == {"role": "user", "content": "what's the weather?"}
    assert set(state["tools"]) == {"get_weather", "search_web"}
    assert state["tools"]["get_weather"].endswith(":get_weather")
