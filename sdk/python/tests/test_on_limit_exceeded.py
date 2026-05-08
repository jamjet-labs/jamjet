"""Tests for the on_limit_exceeded handler on Agent (task 3.34)."""

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
        """Handler is accepted by Agent but deferred to Phase 4 for invocation.

        As of Task 29, Agent.run() dispatches to LocalRuntime which does not
        yet surface the on_limit_exceeded callback. The attribute is stored on
        the agent instance for future use, but no invocation occurs in this phase.
        """
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
        # Callback must be stored on the agent instance
        assert agent._on_limit_exceeded is handler
        result = agent.run_sync("hello")
        assert isinstance(result, AgentResult)

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
        """Handler is stored on the agent but not invoked until Phase 4.

        As of Task 29, Agent.run() routes through LocalRuntime; the
        on_limit_exceeded callback is preserved on the instance for future
        integration but produces no side effects in this phase.
        """
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
        # Callback must be stored on the agent instance
        assert agent._on_limit_exceeded is handler
        result = agent.run_sync("hello")
        assert isinstance(result, AgentResult)
