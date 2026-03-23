"""Tests for the on_limit_exceeded handler on Agent (task 3.34)."""

import asyncio

import pytest

from jamjet import Agent, tool
from jamjet.agents.agent import AgentResult


# ── Tools ────────────────────────────────────────────────────────────────────


@tool
async def echo(text: str) -> str:
    """Echo text back."""
    return text


@tool
async def slow_tool(text: str) -> str:
    """A tool that always returns a value, forcing continued tool calls."""
    return f"processed: {text}"


# ── Tests ────────────────────────────────────────────────────────────────────


class TestOnLimitExceeded:
    def test_on_limit_exceeded_called_on_iteration_limit(self):
        """Handler fires when max_iterations is hit and transforms output."""
        handler_calls: list[tuple] = []

        def handler(
            partial_output: str | None,
            limit_type: str,
            limit_value: object,
            actual_value: object,
        ) -> str:
            handler_calls.append((partial_output, limit_type, limit_value, actual_value))
            return f"[LIMIT HIT] {partial_output or ''}"

        agent = Agent(
            "limit_test",
            model="gpt-5.2",
            tools=[echo],
            strategy="react",
            max_iterations=1,
            on_limit_exceeded=handler,
        )
        result = agent.run_sync("hello")
        assert isinstance(result, AgentResult)
        # Handler should have been called exactly once
        assert len(handler_calls) == 1
        assert handler_calls[0][1] == "max_iterations"
        assert handler_calls[0][2] == 1  # limit_value
        # Output should have the transformed prefix
        assert result.output.startswith("[LIMIT HIT]")

    def test_on_limit_exceeded_none_returns_partial(self):
        """Handler returning None preserves the partial output unchanged."""

        def handler(
            partial_output: str | None,
            limit_type: str,
            limit_value: object,
            actual_value: object,
        ) -> None:
            return None

        agent = Agent(
            "none_test",
            model="gpt-5.2",
            tools=[echo],
            strategy="react",
            max_iterations=1,
            on_limit_exceeded=handler,
        )
        result = agent.run_sync("hello")
        assert isinstance(result, AgentResult)
        # Output should be whatever partial output was produced (not transformed)
        assert not result.output.startswith("[LIMIT HIT]")

    def test_on_limit_exceeded_not_called_within_limits(self):
        """Handler is NOT called when the agent completes within its limits."""
        handler_calls: list[tuple] = []

        def handler(
            partial_output: str | None,
            limit_type: str,
            limit_value: object,
            actual_value: object,
        ) -> str:
            handler_calls.append((partial_output, limit_type, limit_value, actual_value))
            return f"[LIMIT HIT] {partial_output or ''}"

        agent = Agent(
            "within_limits",
            model="gpt-5.2",
            tools=[echo],
            strategy="react",
            max_iterations=10,
            on_limit_exceeded=handler,
        )
        result = agent.run_sync("hello")
        assert isinstance(result, AgentResult)
        # Handler should NOT have been called
        assert len(handler_calls) == 0
        assert not result.output.startswith("[LIMIT HIT]")

    def test_on_limit_exceeded_partial_output_accessible(self):
        """Handler receives whatever partial output was produced before the limit."""
        received_partials: list[str | None] = []

        def handler(
            partial_output: str | None,
            limit_type: str,
            limit_value: object,
            actual_value: object,
        ) -> str | None:
            received_partials.append(partial_output)
            return partial_output

        agent = Agent(
            "partial_test",
            model="gpt-5.2",
            tools=[echo],
            strategy="react",
            max_iterations=1,
            on_limit_exceeded=handler,
        )
        result = agent.run_sync("hello")
        # The handler should have received the partial output
        assert len(received_partials) == 1
        # Partial output should be a string (possibly empty, but not None in typical case)
        assert isinstance(received_partials[0], str)
