"""Unit tests for the agent-loop tool-dispatch helper (Track 2j-3).

Drives :func:`jamjet.agents.tool_runtime.dispatch_tool_calls` with a fake tool
registry (a ``module:function`` map pointing at this test module) and asserts the
returned messages carry the appended assistant + tool messages.
"""

from __future__ import annotations

import json

import pytest

from jamjet.agents.tool_runtime import dispatch_tool_calls
from jamjet.tools.decorators import tool


# Fake tools resolved via the {name: "module:function"} map. `__name__` is the
# live, importable name of this test module (already in sys.modules).
async def echo_tool(text: str) -> str:
    return f"echo: {text}"


async def add_tool(a: int, b: int) -> int:
    return a + b


# A @tool-decorated function exercises the registry fallback (resolved by name).
@tool
async def registry_tool(value: str) -> str:
    """Registered via @tool."""
    return f"registry: {value}"


_ECHO_REF = f"{__name__}:echo_tool"
_ADD_REF = f"{__name__}:add_tool"


async def test_appends_assistant_then_tool_message():
    result = await dispatch_tool_calls(
        {
            "messages": [{"role": "user", "content": "hi"}],
            "assistant_content": "let me check",
            "tool_calls": [{"id": "c1", "name": "echo", "arguments": {"text": "hello"}}],
            "tools": {"echo": _ECHO_REF},
        }
    )
    msgs = result["messages"]
    assert len(msgs) == 3  # original user + assistant + tool

    assistant = msgs[1]
    assert assistant["role"] == "assistant"
    assert assistant["content"] == "let me check"
    assert assistant["tool_calls"][0]["id"] == "c1"
    assert assistant["tool_calls"][0]["type"] == "function"
    assert assistant["tool_calls"][0]["function"]["name"] == "echo"
    # Arguments are serialised to a JSON string (OpenAI shape).
    assert json.loads(assistant["tool_calls"][0]["function"]["arguments"]) == {"text": "hello"}

    tool_msg = msgs[2]
    assert tool_msg["role"] == "tool"
    assert tool_msg["tool_call_id"] == "c1"
    assert tool_msg["name"] == "echo"
    assert tool_msg["content"] == "echo: hello"


async def test_reads_2j2_state_keys_as_fallback():
    """When called with the engine's full state, read last_model_* keys."""
    result = await dispatch_tool_calls(
        {
            "messages": [],
            "last_model_output": "thinking",
            "last_model_tool_calls": [{"id": "c9", "name": "echo", "arguments": {"text": "x"}}],
            "tools": {"echo": _ECHO_REF},
        }
    )
    msgs = result["messages"]
    assert msgs[0]["role"] == "assistant"
    assert msgs[0]["content"] == "thinking"
    assert msgs[1]["role"] == "tool"
    assert msgs[1]["content"] == "echo: x"


async def test_multiple_tool_calls_in_one_turn():
    result = await dispatch_tool_calls(
        {
            "messages": [],
            "tool_calls": [
                {"id": "a", "name": "echo", "arguments": {"text": "one"}},
                {"id": "b", "name": "add", "arguments": {"a": 2, "b": 3}},
            ],
            "tools": {"echo": _ECHO_REF, "add": _ADD_REF},
        }
    )
    msgs = result["messages"]
    # assistant (with 2 tool_calls) + 2 tool messages
    assert len(msgs) == 3
    assert len(msgs[0]["tool_calls"]) == 2
    assert msgs[1]["content"] == "echo: one"
    assert msgs[2]["content"] == "5"  # add result stringified


async def test_json_string_arguments_are_parsed():
    result = await dispatch_tool_calls(
        {
            "messages": [],
            "tool_calls": [{"id": "c1", "name": "echo", "arguments": '{"text": "parsed"}'}],
            "tools": {"echo": _ECHO_REF},
        }
    )
    assert result["messages"][1]["content"] == "echo: parsed"


async def test_registry_fallback_when_not_in_map():
    """A tool absent from the map resolves via the in-process @tool registry."""
    result = await dispatch_tool_calls(
        {
            "messages": [],
            "tool_calls": [{"id": "c1", "name": "registry_tool", "arguments": {"value": "v"}}],
            "tools": {},
        }
    )
    assert result["messages"][1]["content"] == "registry: v"


async def test_unknown_tool_raises():
    with pytest.raises(KeyError, match="nope"):
        await dispatch_tool_calls(
            {
                "messages": [],
                "tool_calls": [{"id": "c1", "name": "nope", "arguments": {}}],
                "tools": {},
            }
        )


async def test_no_tool_calls_returns_assistant_only():
    result = await dispatch_tool_calls(
        {"messages": [{"role": "user", "content": "hi"}], "last_model_output": "done", "tools": {}}
    )
    msgs = result["messages"]
    assert len(msgs) == 2
    assert msgs[1]["role"] == "assistant"
    assert msgs[1]["content"] == "done"
    assert msgs[1]["tool_calls"] == []
