"""Tests for ``Agent.run_durable`` (Track 2j-4) — the durable run entrypoint.

No real engine: a ``_FakeDurableClient`` stands in for ``JamjetClient`` and
returns a terminal execution with a known final ``current_state``. We assert that
``run_durable`` compiles + registers + starts an execution seeded with the
messages/tools, polls to terminal, and extracts an ``AgentResult`` whose fields
(final content, tool-call trace, ir) match the in-process ``Agent.run`` shape.
"""

from __future__ import annotations

from typing import Any

import pytest

from jamjet.agents.agent import Agent, AgentResult
from jamjet.tools.decorators import tool


@tool
async def get_weather(city: str) -> str:
    """Return the weather for a city."""
    return f"sunny in {city}"


def _agent() -> Agent:
    return Agent(
        "weatherbot",
        model="anthropic/claude-sonnet-4-6",
        tools=[get_weather],
        instructions="You are a weather assistant.",
    )


# A terminal execution whose final state is a tool-using turn followed by the
# model's final answer (the answer lives in last_model_output, NOT appended to
# messages — only tool turns append to messages in the loop).
def _completed_execution() -> dict[str, Any]:
    return {
        "execution_id": "exec_test",
        "status": "completed",
        "current_state": {
            "messages": [
                {"role": "system", "content": "You are a weather assistant."},
                {"role": "user", "content": "weather in London?"},
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": '{"city": "London"}',
                            },
                        }
                    ],
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "name": "get_weather",
                    "content": "sunny in London",
                },
            ],
            "last_model_output": "It is sunny in London.",
            "last_model_finish_reason": "stop",
            "tools": {"get_weather": "tests.test_agent_run_durable:get_weather"},
        },
    }


class _FakeDurableClient:
    """Async-context-manager fake of JamjetClient for run_durable unit tests."""

    def __init__(self, execution: dict[str, Any], events: list[dict[str, Any]] | None = None) -> None:
        self._execution = execution
        self._events = events or []
        self.created: list[dict[str, Any]] = []
        self.started: list[tuple[str, dict[str, Any]]] = []

    async def __aenter__(self) -> _FakeDurableClient:
        return self

    async def __aexit__(self, *args: Any) -> None:
        return None

    async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
        self.created.append(ir)
        return {"workflow_id": ir["workflow_id"]}

    async def start_execution(
        self, workflow_id: str, input: dict[str, Any], workflow_version: str | None = None
    ) -> dict[str, Any]:
        self.started.append((workflow_id, input))
        return {"execution_id": "exec_test"}

    async def get_execution(self, execution_id: str) -> dict[str, Any]:
        return self._execution

    async def get_events(self, execution_id: str) -> dict[str, Any]:
        return {"events": self._events}


def _patch_client(monkeypatch: pytest.MonkeyPatch, fake: _FakeDurableClient) -> None:
    # run_durable does `from jamjet.client import JamjetClient` at call time, so
    # patching the module attribute swaps in our fake.
    monkeypatch.setattr("jamjet.client.JamjetClient", lambda *a, **k: fake)


async def test_run_durable_extracts_final_answer(monkeypatch: pytest.MonkeyPatch) -> None:
    """run_durable returns an AgentResult whose final content is last_model_output."""
    fake = _FakeDurableClient(_completed_execution())
    _patch_client(monkeypatch, fake)

    result = await _agent().run_durable("weather in London?", max_turns=3)

    assert isinstance(result, AgentResult)
    assert result.output == "It is sunny in London."


async def test_run_durable_reconstructs_tool_call_trace(monkeypatch: pytest.MonkeyPatch) -> None:
    """The tool calls made during the loop are reconstructed with in-process shape."""
    fake = _FakeDurableClient(_completed_execution())
    _patch_client(monkeypatch, fake)

    result = await _agent().run_durable("weather in London?", max_turns=3)

    assert len(result.tool_calls) == 1
    call = result.tool_calls[0]
    # Same keys as the in-process ToolCallRecord.model_dump().
    assert set(call) == {"tool", "input", "output", "duration_us"}
    assert call["tool"] == "get_weather"
    assert call["input"] == {"city": "London"}
    assert call["output"] == "sunny in London"


async def test_run_durable_seeds_messages_and_tools(monkeypatch: pytest.MonkeyPatch) -> None:
    """run_durable registers the IR and starts an execution seeded with the
    system+user messages and the tool-resolver map."""
    fake = _FakeDurableClient(_completed_execution())
    _patch_client(monkeypatch, fake)

    result = await _agent().run_durable("weather in London?", max_turns=3)

    # Registered exactly one workflow; the result.ir is the compiled agent-loop IR.
    assert len(fake.created) == 1
    assert result.ir["labels"]["jamjet.agent.loop"] == "true"
    assert result.ir["workflow_id"] == "weatherbot"

    # Started exactly one execution, seeded with messages + tools.
    assert len(fake.started) == 1
    workflow_id, initial_input = fake.started[0]
    assert workflow_id == "weatherbot"
    assert [m["role"] for m in initial_input["messages"]] == ["system", "user"]
    assert initial_input["messages"][1]["content"] == "weather in London?"
    assert initial_input["tools"]["get_weather"].endswith(":get_weather")


async def test_run_durable_no_tool_turns_uses_last_model_output(monkeypatch: pytest.MonkeyPatch) -> None:
    """A run that answers directly (no tool turns) still returns the answer with
    an empty tool-call trace."""
    execution = {
        "execution_id": "exec_direct",
        "status": "completed",
        "current_state": {
            "messages": [
                {"role": "system", "content": "You are a weather assistant."},
                {"role": "user", "content": "hello"},
            ],
            "last_model_output": "Hi there!",
            "last_model_finish_reason": "stop",
        },
    }
    fake = _FakeDurableClient(execution)
    _patch_client(monkeypatch, fake)

    result = await _agent().run_durable("hello", max_turns=3)

    assert result.output == "Hi there!"
    assert result.tool_calls == []


async def test_run_durable_raises_on_failed_terminal(monkeypatch: pytest.MonkeyPatch) -> None:
    """A failed terminal state raises RuntimeError rather than returning a result."""
    execution = {"execution_id": "exec_fail", "status": "failed", "current_state": {}}
    fake = _FakeDurableClient(execution)
    _patch_client(monkeypatch, fake)

    with pytest.raises(RuntimeError, match="non-completed"):
        await _agent().run_durable("weather?", max_turns=3)
