"""
Shared test fixtures.

Patches openai.AsyncOpenAI so Agent.run() tests work without a real API key or
network access. The mock calls all provided tools with the extracted query and
returns a response that includes all tool outputs.
"""

from __future__ import annotations

import json
import re
import sys
import types
from unittest.mock import MagicMock

import pytest


# ── Mock OpenAI types ─────────────────────────────────────────────────────────


class _MockToolCall:
    def __init__(self, call_id: str, name: str, arguments: dict) -> None:
        self.id = call_id
        self.function = MagicMock()
        self.function.name = name
        self.function.arguments = json.dumps(arguments)


class _MockMessage:
    def __init__(self, content: str | None = None, tool_calls: list | None = None) -> None:
        self.content = content
        self.role = "assistant"
        self.tool_calls = tool_calls or []


class _MockResponse:
    def __init__(self, message: _MockMessage) -> None:
        self.choices = [MagicMock(message=message)]


# ── Helpers ───────────────────────────────────────────────────────────────────


def _role(m: object) -> str:
    if isinstance(m, dict):
        return m.get("role", "")  # type: ignore[return-value]
    return getattr(m, "role", "")


def _content(m: object) -> str:
    if isinstance(m, dict):
        return m.get("content") or ""  # type: ignore[return-value]
    return getattr(m, "content") or ""


def _extract_query(user_content: str) -> str:
    """Extract the original goal/query from a plan-and-execute-formatted message."""
    # "Overall goal: X\n\n..." → "X"
    match = re.search(r"Overall goal: (.+?)(?:\n|$)", user_content)
    if match:
        return match.group(1).strip()
    # "Goal: X\n\n..." → "X"
    match = re.search(r"Goal: (.+?)(?:\n|$)", user_content)
    if match:
        return match.group(1).strip()
    return user_content.strip()


# ── Smart mock client ─────────────────────────────────────────────────────────


class _SmartMockClient:
    """
    Mock AsyncOpenAI client for unit tests.

    Per-call behaviour:
    - tools provided AND no tool-result messages → issue one tool call per tool
    - tool-result messages present → return joined tool results as content
    - no tools, no tool results → echo user content (plan / synthesis pass-through)
    """

    def __init__(self, *args: object, **kwargs: object) -> None:
        self.chat = MagicMock()
        self.chat.completions = MagicMock()
        self.chat.completions.create = self._create
        self._idx = 0

    async def _create(
        self,
        model: str,
        messages: list,
        tools: list | None = None,
        **kwargs: object,
    ) -> _MockResponse:
        # Last user message content
        user_content = ""
        for m in reversed(messages):
            if _role(m) == "user":
                user_content = _content(m)
                break

        # Collect existing tool results
        tool_results = [_content(m) for m in messages if _role(m) == "tool"]

        if tools and not tool_results:
            # Issue one tool call per tool using the extracted query
            query = _extract_query(user_content)
            calls = []
            for i, tool_def in enumerate(tools):
                name = tool_def["function"]["name"]
                params = tool_def["function"]["parameters"].get("properties", {})
                first_param = next(iter(params), None)
                args = {first_param: query} if first_param else {}
                calls.append(_MockToolCall(f"call_{self._idx}_{i}", name, args))
            self._idx += 1
            return _MockResponse(_MockMessage(tool_calls=calls))

        if tool_results:
            return _MockResponse(_MockMessage(content=" ".join(tool_results)))

        # Planning / synthesis — echo user content.
        # The synthesis user message already embeds step results so the output
        # will contain tool output text (satisfying "Results for: X" assertions).
        return _MockResponse(_MockMessage(content=user_content))


# ── Fixture ───────────────────────────────────────────────────────────────────


@pytest.fixture(autouse=True)
def mock_openai_client(monkeypatch: pytest.MonkeyPatch) -> None:
    """Patch openai.AsyncOpenAI for all tests — no real API calls."""
    if "openai" not in sys.modules:
        mock_module = types.ModuleType("openai")
        mock_module.AsyncOpenAI = _SmartMockClient  # type: ignore[attr-defined]
        sys.modules["openai"] = mock_module
    else:
        monkeypatch.setattr("openai.AsyncOpenAI", _SmartMockClient)
