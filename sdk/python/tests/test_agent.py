"""Tests for the Agent and @task syntactic sugar."""

import asyncio

import pytest

from jamjet import Agent, tool, task
from jamjet.agents.agent import AgentResult


# ── Fixtures ──────────────────────────────────────────────────────────────────


@tool
async def search(query: str) -> str:
    return f"Results for: {query}"


@tool
async def calculator(query: str) -> str:
    return f"42"


# ── Agent tests ───────────────────────────────────────────────────────────────


class TestAgent:
    def test_create_minimal(self):
        agent = Agent("test", model="gpt-5.2", tools=[search])
        assert agent.name == "test"
        assert agent.model == "gpt-5.2"
        assert agent.tool_names == ["search"]

    def test_create_with_instructions(self):
        agent = Agent(
            "helper",
            model="gpt-5.2",
            tools=[search],
            instructions="Be helpful.",
        )
        assert agent.instructions == "Be helpful."

    def test_create_multiple_tools(self):
        agent = Agent("multi", model="gpt-5.2", tools=[search, calculator])
        assert agent.tool_names == ["search", "calculator"]

    def test_rejects_non_tool_functions(self):
        def plain_fn(x: str) -> str:
            return x

        with pytest.raises(TypeError, match="not a @tool-decorated function"):
            Agent("bad", model="gpt-5.2", tools=[plain_fn])

    def test_compile_produces_ir(self):
        agent = Agent("compile_test", model="gpt-5.2", tools=[search])
        ir = agent.compile()
        assert "nodes" in ir
        assert "edges" in ir
        assert "start_node" in ir

    def test_run_returns_result(self):
        agent = Agent(
            "runner",
            model="gpt-5.2",
            tools=[search],
            instructions="Search and summarize.",
        )
        result = asyncio.run(agent.run("test query"))
        assert isinstance(result, AgentResult)
        assert "Results for: test query" in result.output
        assert len(result.tool_calls) > 0

    def test_run_sync(self):
        agent = Agent("sync", model="gpt-5.2", tools=[search])
        result = agent.run_sync("test query")
        assert isinstance(result, AgentResult)
        assert "Results for: test query" in result.output

    def test_result_str(self):
        agent = Agent("str_test", model="gpt-5.2", tools=[search])
        result = agent.run_sync("hello")
        assert str(result) == result.output

    def test_repr(self):
        agent = Agent("repr_test", model="gpt-5.2", tools=[search])
        r = repr(agent)
        assert "repr_test" in r
        assert "gpt-5.2" in r

    def test_custom_limits(self):
        agent = Agent(
            "limits",
            model="gpt-5.2",
            tools=[search],
            max_iterations=5,
            max_cost_usd=0.5,
            timeout_seconds=60,
        )
        assert agent.limits.max_iterations == 5
        assert agent.limits.max_cost_usd == 0.5
        assert agent.limits.timeout_seconds == 60


# ── @task tests ───────────────────────────────────────────────────────────────


class TestTask:
    def test_basic_task(self):
        @task(model="gpt-5.2", tools=[search])
        async def research(question: str) -> str:
            """You are a research assistant."""

        result = asyncio.run(research("test question"))
        assert "Results for: test question" in result

    def test_task_preserves_name(self):
        @task(model="gpt-5.2", tools=[search])
        async def named_task(q: str) -> str:
            """Do stuff."""

        assert named_task.__name__ == "named_task"

    def test_task_uses_docstring_as_instructions(self):
        @task(model="gpt-5.2", tools=[search])
        async def documented(q: str) -> str:
            """These are my instructions."""

        agent = documented._jamjet_agent
        assert agent.instructions == "These are my instructions."

    def test_task_with_kwargs(self):
        @task(model="gpt-5.2", tools=[search])
        async def kw_task(question: str) -> str:
            """Answer questions."""

        result = asyncio.run(kw_task(question="test"))
        assert "Results for: test" in result

    def test_task_requires_argument(self):
        @task(model="gpt-5.2", tools=[search])
        async def empty_task(q: str) -> str:
            """Do something."""

        with pytest.raises(TypeError, match="requires at least one argument"):
            asyncio.run(empty_task())

    def test_task_no_tools(self):
        @task(model="gpt-5.2")
        async def no_tools_task(q: str) -> str:
            """Think hard."""

        # Should not raise on creation — it's valid to have a model-only task
        agent = no_tools_task._jamjet_agent
        assert agent.tool_names == []
