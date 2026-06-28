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


# ---------------------------------------------------------------------------
# Finding 2 — seed_history must preserve the injected MEMORY block when the
# instruction slot is absent (empty instructions).  seed_messages_for_run emits
# the instruction system slot ONLY for non-empty instructions; for a
# memory-enabled run with EMPTY instructions the FIRST system message is the
# retrieved-memory block.  The pre-fix seed_history blindly REPLACED the leading
# system message with the strategy persona, dropping recall for every non-react
# strategy.  These pin the fix.
# ---------------------------------------------------------------------------


def test_seed_history_preserves_memory_block_when_it_leads() -> None:
    """seed_history: a leading MEMORY block (empty instructions) is NOT clobbered.

    Pre-fix, prefix[0] (the memory block, role=system) was replaced with the
    persona and recall was lost.  Post-fix the persona is PREPENDED and the memory
    block survives.
    """
    from jamjet.runtime.local.strategies.base import MEMORY_BLOCK_PREFIX, seed_history

    memory_block = {"role": "system", "content": f"{MEMORY_BLOCK_PREFIX}\nThe codeword is {SECRET}."}
    initial_messages = [
        memory_block,
        {"role": "user", "content": "earlier turn"},
        {"role": "assistant", "content": "earlier reply"},
        {"role": "user", "content": "new prompt"},  # trailing, dropped by seed_history
    ]
    out = seed_history(initial_messages, "You are the proposer.")

    # The persona is present as the FIRST system message ...
    assert out[0] == {"role": "system", "content": "You are the proposer."}
    # ... and the memory block is preserved (not clobbered).
    assert any(m.get("role") == "system" and str(m.get("content", "")).startswith(MEMORY_BLOCK_PREFIX) for m in out), (
        f"memory block dropped by seed_history: {out}"
    )
    assert SECRET in " ".join(m.get("content", "") for m in out)


def test_seed_history_swaps_instruction_slot_keeps_memory() -> None:
    """seed_history: with an instruction slot present it is swapped (not duplicated)
    for the persona, and the memory block right after it is kept."""
    from jamjet.runtime.local.strategies.base import MEMORY_BLOCK_PREFIX, seed_history

    initial_messages = [
        {"role": "system", "content": "Original agent instructions."},
        {"role": "system", "content": f"{MEMORY_BLOCK_PREFIX}\nremember {SECRET}"},
        {"role": "user", "content": "earlier"},
        {"role": "user", "content": "new prompt"},  # trailing, dropped
    ]
    out = seed_history(initial_messages, "You are agent 1 of 3. Think independently.")

    systems = [m for m in out if m.get("role") == "system"]
    # Exactly the persona + the memory block (instruction slot swapped, not duplicated).
    assert systems[0]["content"] == "You are agent 1 of 3. Think independently."
    assert "Original agent instructions." not in " ".join(m.get("content", "") for m in out)
    assert any(str(m["content"]).startswith(MEMORY_BLOCK_PREFIX) for m in systems), out


def test_non_react_strategy_carries_memory_block_with_empty_instructions() -> None:
    """End-to-end through a non-react strategy: a memory-first seed (empty
    instructions) carries the memory block INTO the model call.

    Drives the real ``plan_and_execute`` generative phase with a memory-first
    seed and a capturing adapter; asserts the retrieved-memory block reached the
    model.  Fails pre-fix (seed_history dropped the leading memory block).
    """
    from jamjet.runtime.local.strategies import plan_and_execute
    from jamjet.runtime.local.strategies.base import MEMORY_BLOCK_PREFIX

    captured: list[list[dict]] = []

    class _FakeAdapter:
        async def generate(self, messages: list[dict[str, Any]], *, tools: Any = None) -> Any:
            captured.append([dict(m) if isinstance(m, dict) else m for m in messages])
            msg = MagicMock()
            msg.content = _GENERIC_REPLY
            msg.role = "assistant"
            msg.tool_calls = []
            return msg

    # Build a valid spec with EMPTY instructions via the real compile path.
    spec = Agent("m", model="gpt-4o-mini", tools=[echo], instructions="", strategy="plan-and-execute").compile()

    memory_block = {"role": "system", "content": f"{MEMORY_BLOCK_PREFIX}\nThe codeword is {SECRET}."}
    initial_messages = [
        memory_block,  # leads (no instruction slot — empty instructions)
        {"role": "user", "content": "earlier turn"},
        {"role": "assistant", "content": "earlier reply"},
        {"role": "user", "content": "what is my codeword?"},
    ]

    asyncio.run(
        plan_and_execute.run(
            adapter=_FakeAdapter(),
            spec=spec,
            prompt="what is my codeword?",
            tools=[],
            tool_calls_log=[],
            initial_messages=initial_messages,
        )
    )

    assert captured, "the strategy must have hit the model"
    joined = "\n".join(" ".join((m.get("content") or "") for m in call if isinstance(m, dict)) for call in captured)
    assert MEMORY_BLOCK_PREFIX in joined, f"memory block never reached the model: {captured}"
    assert SECRET in joined, f"recalled secret dropped before the model call: {captured}"
