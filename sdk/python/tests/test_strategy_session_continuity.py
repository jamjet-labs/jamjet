"""C1 regression: EVERY strategy seeds a session run from the carried thread.

The whole-branch review found that ``Agent.run(prompt, session=s)`` only threaded
the prior conversation into the ``react`` strategy.  The DEFAULT strategy is
``plan-and-execute``, and it (plus ``critic`` / ``debate`` / ``consensus`` /
``reflection``) accepted ``initial_messages`` and IGNORED it — so a default-agent
session run was AMNESIAC: the model never saw turn N-1 (or the retrieved memory
block) even though persistence kept running, making storage look continuous while
inference was not.

These tests assert the model INPUT on the SECOND turn carries the first turn's
content for the default strategy and every non-react strategy.  They FAIL before
the fix (the strategies build messages from ``prompt`` + ``instructions`` only)
and PASS after (each generative phase seeds from the carried thread).

Design notes
------------
- The secret marker (``SECRET``) appears ONLY in the user's first prompt.  The
  scripted model NEVER echoes it (replies are constant), so the marker can reach
  a second-turn model call ONLY via the seeded conversation history — not via a
  canned reply leaking back in.  This is what makes the test fail pre-fix.
- The model returns a terminating verdict for the two meta personas (critic
  "strict critic" -> ``PASS``; reflection "self-evaluator" -> ``SATISFIED``) so
  those review loops stop after one round; every other phase gets a generic
  reply with no tool calls.
"""

from __future__ import annotations

import asyncio
import sys
import types
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

import pytest

from jamjet import Agent, tool
from jamjet.agents.session import SessionStore

SECRET = "Zorblax"
_GENERIC_REPLY = "Understood; proceeding."


@tool
async def echo(text: str) -> str:
    """Echo the input."""
    return text


class _CapturingModel:
    """Records every messages list the model sees; never echoes the secret.

    Returns a terminating verdict for the two review personas so critic /
    reflection loops stop after one round; otherwise a constant generic reply
    with no tool calls.
    """

    def __init__(self) -> None:
        self.calls: list[list[dict]] = []

    async def acompletion(
        self,
        model: str,
        messages: list,
        tools: list | None = None,
        **kwargs: object,
    ) -> Any:
        self.calls.append(
            [
                dict(m)
                if isinstance(m, dict)
                else {"role": getattr(m, "role", "?"), "content": getattr(m, "content", "")}
                for m in messages
            ]
        )

        systems = " ".join(
            (m.get("content") if isinstance(m, dict) else getattr(m, "content", "")) or ""
            for m in messages
            if (m.get("role") if isinstance(m, dict) else getattr(m, "role", "")) == "system"
        ).lower()
        if "strict critic" in systems:
            reply = "PASS"
        elif "self-evaluator" in systems:
            reply = "SATISFIED"
        else:
            reply = _GENERIC_REPLY

        msg = MagicMock()
        msg.content = reply
        msg.role = "assistant"
        msg.tool_calls = []
        resp = MagicMock()
        resp.choices = [MagicMock(message=msg)]
        return resp


@pytest.fixture()
def capturing_model(monkeypatch: pytest.MonkeyPatch) -> _CapturingModel:
    _model = _CapturingModel()

    async def _acompletion(
        model: str,
        messages: list,
        tools: list | None = None,
        **kwargs: object,
    ) -> Any:
        return await _model.acompletion(model=model, messages=messages, tools=tools, **kwargs)

    mock_litellm = types.ModuleType("litellm")
    mock_litellm.acompletion = _acompletion  # type: ignore[attr-defined]
    mock_litellm.completion_cost = lambda *a, **kw: 0.0  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "litellm", mock_litellm)
    return _model


@pytest.fixture()
def db_path(tmp_path: Path) -> str:
    return str(tmp_path / "sessions_c1.db")


def _run_two_turns(model: _CapturingModel, store: SessionStore, *, strategy: str | None) -> list[list[dict]]:
    """Run two turns on one session; return ONLY the second turn's model calls."""
    s = store.create("c1")
    kwargs: dict[str, Any] = {"session_store": store}
    if strategy is not None:
        kwargs["strategy"] = strategy
    agent = Agent("c1-agent", model="gpt-4o-mini", tools=[echo], **kwargs)

    asyncio.run(agent.run(f"my codeword is {SECRET}", session=s))
    model.calls.clear()  # only the SECOND turn's inputs matter
    asyncio.run(agent.run("what is my codeword?", session=s))
    return model.calls


def _assert_secret_in_some_call(calls: list[list[dict]], *, strategy: str) -> None:
    assert calls, f"[{strategy}] second turn must hit the model"
    joined = "\n".join(" ".join((m.get("content") or "") for m in call if isinstance(m, dict)) for call in calls)
    assert SECRET.lower() in joined.lower(), (
        f"[{strategy}] AMNESIAC: the prior turn's secret {SECRET!r} never reached the "
        f"model on the second turn — the session thread was dropped.\ncalls={calls}"
    )


def test_default_strategy_is_plan_and_execute() -> None:
    """The default Agent strategy is plan-and-execute (the one C1 was about)."""
    agent = Agent("d", model="gpt-4o-mini", tools=[echo])
    assert agent.strategy == "plan-and-execute"


def test_default_strategy_session_continues_thread(
    capturing_model: _CapturingModel,
    db_path: str,
) -> None:
    """DEFAULT-strategy (plan-and-execute) agent continues a session thread.

    Constructs ``Agent(...)`` with NO ``strategy=`` so the default plan-and-execute
    runner is exercised.  The second run's model input MUST contain the first
    turn's secret.  Fails before the C1 fix; passes after.
    """
    store = SessionStore(db_path)
    calls = _run_two_turns(capturing_model, store, strategy=None)
    _assert_secret_in_some_call(calls, strategy="plan-and-execute (default)")


@pytest.mark.parametrize(
    "strategy",
    ["plan-and-execute", "critic", "debate", "consensus", "reflection"],
)
def test_non_react_strategies_continue_thread(
    capturing_model: _CapturingModel,
    db_path: str,
    strategy: str,
) -> None:
    """Every non-react strategy seeds the session thread into its generative phase."""
    store = SessionStore(db_path)
    calls = _run_two_turns(capturing_model, store, strategy=strategy)
    _assert_secret_in_some_call(calls, strategy=strategy)
