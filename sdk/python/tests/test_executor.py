"""Tests for in-process workflow and agent execution (no runtime server needed)."""

import asyncio

import pytest
from pydantic import BaseModel

from jamjet import Agent, Workflow, tool
from jamjet.workflow.executor import ExecutionResult

# ── Shared fixtures ──────────────────────────────────────────────────────────


@tool
async def search_tool(query: str) -> str:
    return f"Results for: {query}"


@tool
async def uppercase_tool(query: str) -> str:
    return query.upper()


# ── Workflow.run() tests ─────────────────────────────────────────────────────


class TestWorkflowRun:
    def test_single_step(self):
        wf = Workflow("single")

        @wf.state
        class State(BaseModel):
            value: str
            result: str | None = None

        @wf.step
        async def process(state: State) -> State:
            return state.model_copy(update={"result": state.value.upper()})

        result = asyncio.run(wf.run(State(value="hello")))
        assert isinstance(result, ExecutionResult)
        assert result.state.result == "HELLO"
        assert result.steps_executed == 1
        assert result.total_duration_us > 0

    def test_two_steps_sequential(self):
        wf = Workflow("two_step")

        @wf.state
        class State(BaseModel):
            text: str
            step1_done: bool = False
            step2_done: bool = False

        @wf.step
        async def first(state: State) -> State:
            return state.model_copy(update={"step1_done": True})

        @wf.step
        async def second(state: State) -> State:
            return state.model_copy(update={"step2_done": True})

        result = asyncio.run(wf.run(State(text="test")))
        assert result.state.step1_done is True
        assert result.state.step2_done is True
        assert result.steps_executed == 2
        assert len(result.events) == 2
        assert result.events[0].step == "first"
        assert result.events[1].step == "second"

    def test_three_steps(self):
        wf = Workflow("three_step")

        @wf.state
        class State(BaseModel):
            value: int

        @wf.step
        async def add_one(state: State) -> State:
            return state.model_copy(update={"value": state.value + 1})

        @wf.step
        async def double(state: State) -> State:
            return state.model_copy(update={"value": state.value * 2})

        @wf.step
        async def add_ten(state: State) -> State:
            return state.model_copy(update={"value": state.value + 10})

        result = asyncio.run(wf.run(State(value=1)))
        # (1 + 1) * 2 + 10 = 14
        assert result.state.value == 14
        assert result.steps_executed == 3

    def test_step_with_tool_call(self):
        wf = Workflow("with_tools")

        @wf.state
        class State(BaseModel):
            query: str
            answer: str | None = None

        @wf.step
        async def do_search(state: State) -> State:
            result = await search_tool(query=state.query)
            return state.model_copy(update={"answer": result})

        result = asyncio.run(wf.run(State(query="test")))
        assert result.state.answer == "Results for: test"
        assert result.steps_executed == 1

    def test_conditional_routing(self):
        wf = Workflow("conditional")

        @wf.state
        class State(BaseModel):
            value: int
            path: str = ""

        @wf.step(next={"high": lambda s: s.value > 10, "low": lambda s: s.value <= 10})
        async def check(state: State) -> State:
            return state

        @wf.step(name="high")
        async def high_path(state: State) -> State:
            return state.model_copy(update={"path": "high"})

        @wf.step(name="low")
        async def low_path(state: State) -> State:
            return state.model_copy(update={"path": "low"})

        # High path
        result = asyncio.run(wf.run(State(value=20)))
        assert result.state.path == "high"

        # Low path
        result = asyncio.run(wf.run(State(value=5)))
        assert result.state.path == "low"

    def test_run_sync(self):
        wf = Workflow("sync_test")

        @wf.state
        class State(BaseModel):
            x: int

        @wf.step
        async def inc(state: State) -> State:
            return state.model_copy(update={"x": state.x + 1})

        result = wf.run_sync(State(x=0))
        assert result.state.x == 1

    def test_max_steps_guard(self):
        wf = Workflow("loop_guard")

        @wf.state
        class State(BaseModel):
            counter: int = 0

        @wf.step(next={"increment": lambda _: True})
        async def increment(state: State) -> State:
            return state.model_copy(update={"counter": state.counter + 1})

        result = asyncio.run(wf.run(State(), max_steps=5))
        assert result.steps_executed == 5
        assert result.state.counter == 5

    def test_error_in_step_raises(self):
        wf = Workflow("error_test")

        @wf.state
        class State(BaseModel):
            x: int

        @wf.step
        async def fail(state: State) -> State:
            raise ValueError("step failed")

        with pytest.raises(ValueError, match="step failed"):
            asyncio.run(wf.run(State(x=0)))

    def test_no_state_raises(self):
        wf = Workflow("no_state")

        @wf.step
        async def s(state: dict) -> dict:
            return state

        with pytest.raises(ValueError, match="no @workflow.state"):
            asyncio.run(wf.run({}))

    def test_no_steps_raises(self):
        wf = Workflow("no_steps")

        @wf.state
        class State(BaseModel):
            x: int

        with pytest.raises(ValueError, match="no @workflow.step"):
            asyncio.run(wf.run(State(x=0)))

    def test_events_have_timing(self):
        wf = Workflow("timing")

        @wf.state
        class State(BaseModel):
            x: int

        @wf.step
        async def step_a(state: State) -> State:
            return state

        @wf.step
        async def step_b(state: State) -> State:
            return state

        result = asyncio.run(wf.run(State(x=0)))
        for event in result.events:
            assert event.status == "completed"
            assert event.duration_us >= 0
            assert event.timestamp_ns > 0

    def test_result_str(self):
        wf = Workflow("str_test")

        @wf.state
        class State(BaseModel):
            x: int = 42

        @wf.step
        async def noop(state: State) -> State:
            return state

        result = wf.run_sync(State())
        assert "42" in str(result)


# ── Agent.run() in-process tests ─────────────────────────────────────────────


class TestAgentRun:
    def test_agent_run_in_process(self):
        agent = Agent("local", model="test-model", tools=[search_tool])
        result = asyncio.run(agent.run("test query"))
        assert "Results for: test query" in result.output
        assert result.duration_us > 0
        assert len(result.tool_calls) == 1
        assert result.tool_calls[0]["duration_us"] > 0

    def test_agent_multiple_tools(self):
        agent = Agent("multi", model="test-model", tools=[search_tool, uppercase_tool])
        result = agent.run_sync("hello")
        assert "Results for: hello" in result.output
        assert "HELLO" in result.output
        assert len(result.tool_calls) == 2

    def test_agent_has_ir(self):
        agent = Agent("ir_test", model="test-model", tools=[search_tool])
        result = agent.run_sync("query")
        assert "nodes" in result.ir
        assert "edges" in result.ir


# ── @task in-process tests ───────────────────────────────────────────────────


class TestTaskRun:
    def test_task_runs_in_process(self):
        from jamjet import task

        @task(model="test-model", tools=[search_tool])
        async def research(question: str) -> str:
            """You are a research assistant."""

        result = asyncio.run(research("what is JamJet?"))
        assert "Results for: what is JamJet?" in result
