"""Tests for the Python SDK workflow compiler."""

import pytest
from pydantic import BaseModel

from jamjet import Workflow, tool
from jamjet.workflow.ir_compiler import compile_yaml

# ── @tool decorator ───────────────────────────────────────────────────────────


def test_tool_registration():
    @tool
    async def my_tool(query: str) -> str:
        return query

    from jamjet.tools.decorators import get_tool

    t = get_tool("my_tool")
    assert t is not None
    assert t.name == "my_tool"


def test_tool_callable():
    @tool
    async def echo(msg: str) -> str:
        return msg

    import asyncio

    result = asyncio.run(echo(msg="hello"))
    assert result == "hello"


# ── Workflow compilation ───────────────────────────────────────────────────────


def test_workflow_compile_basic():
    wf = Workflow("test_wf", version="0.1.0")

    @wf.state
    class State(BaseModel):
        value: str

    @wf.step
    async def step_a(state: State) -> State:
        return state

    @wf.step
    async def step_b(state: State) -> State:
        return state

    ir = wf.compile()
    assert ir["workflow_id"] == "test_wf"
    assert ir["version"] == "0.1.0"
    assert "step_a" in ir["nodes"]
    assert "step_b" in ir["nodes"]
    assert ir["start_node"] == "step_a"
    # step_a → step_b edge
    edges = {(e["from"], e["to"]) for e in ir["edges"]}
    assert ("step_a", "step_b") in edges


def test_workflow_requires_state():
    wf = Workflow("no_state")

    @wf.step
    async def s(state: dict) -> dict:  # type: ignore
        return state

    with pytest.raises(ValueError, match="no @workflow.state"):
        wf.compile()


def test_workflow_requires_steps():
    wf = Workflow("no_steps")

    @wf.state
    class State(BaseModel):
        x: int

    with pytest.raises(ValueError, match="no @workflow.step"):
        wf.compile()


# ── YAML compiler ─────────────────────────────────────────────────────────────

SAMPLE_YAML = """
workflow:
  id: yaml_wf
  version: 0.1.0
  state_schema: schemas.State
  start: fetch

nodes:
  fetch:
    type: tool
    tool_ref: get_data
    input:
      id: "{{ state.id }}"
    output_schema: schemas.Data
    next: analyze

  analyze:
    type: model
    model: default_chat
    prompt: prompts/analyze.md
    output_schema: schemas.Result
    next: end
"""


def test_yaml_compile():
    ir = compile_yaml(SAMPLE_YAML)
    assert ir["workflow_id"] == "yaml_wf"
    assert ir["start_node"] == "fetch"
    assert "fetch" in ir["nodes"]
    assert "analyze" in ir["nodes"]
    edges = {(e["from"], e["to"]) for e in ir["edges"]}
    assert ("fetch", "analyze") in edges
    assert ("analyze", "end") in edges


def test_yaml_node_kinds():
    ir = compile_yaml(SAMPLE_YAML)
    assert ir["nodes"]["fetch"]["kind"]["type"] == "tool"
    assert ir["nodes"]["analyze"]["kind"]["type"] == "model"


# ── IR graph builder ──────────────────────────────────────────────────────────


def test_graph_builder_compile():
    from jamjet.workflow.graph import WorkflowGraph
    from jamjet.workflow.nodes import ModelNode, ToolNode

    graph = WorkflowGraph("graph_wf")
    graph.add_node("fetch", ToolNode(tool_ref="get_data"))
    graph.add_node("analyze", ModelNode(model="default_chat"))
    graph.add_edge("fetch", "analyze")
    graph.add_edge("analyze", "end")

    ir = graph.compile()
    assert ir["workflow_id"] == "graph_wf"
    assert "fetch" in ir["nodes"]
    assert "analyze" in ir["nodes"]
    edges = {(e["from"], e["to"]) for e in ir["edges"]}
    assert ("fetch", "analyze") in edges
