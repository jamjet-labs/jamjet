"""Tests for T4-3: the ``memory=`` knob wires Engram into the friendly Agent.

The friendly :class:`~jamjet.agents.agent.Agent` gains a ``memory=`` knob and an
AUTOMATIC, GOVERNED retrieve-at-start / record-at-end loop keyed by the stable
``session.id``:

- ``memory=True`` constructs the default embedded Engram bridge; if
  ``jamjet-engram`` is not installed it FAILS LOUD (never a silent no-op).
- ``memory=<AgentMemory>`` uses the injected bridge as-is (duck-typed, so a
  fake can be injected in unit tests — no real Engram needed here).
- Before a run the agent retrieves a context block keyed by ``session.id`` and
  injects it into the model's messages; after the run it records the turn,
  keyed by ``session.id``.
- PII is REDACTED (the Track-3 redactor) before any memory write, so raw PII is
  never written into the temporal KG.
- Memory requires a session to key on (fail-loud if memory is on but no
  session); memory is OFF by default (no behaviour change).
"""

from __future__ import annotations

import asyncio
import sys
import types
from contextlib import contextmanager
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

import pytest

from jamjet import Agent, tool
from jamjet.agents.session import SessionStore


@tool
async def echo(text: str) -> str:
    """Echo the input."""
    return text


# ---------------------------------------------------------------------------
# Capturing mock: records every messages list the model sees (so we can prove
# the retrieved memory block was injected into the run).
# ---------------------------------------------------------------------------


class _CapturingMockClient:
    def __init__(self) -> None:
        self.calls: list[list[dict]] = []

    async def _create(
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
        last_user = ""
        for m in reversed(messages):
            role = m.get("role") if isinstance(m, dict) else getattr(m, "role", "")
            content = m.get("content") if isinstance(m, dict) else getattr(m, "content", "")
            if role == "user":
                last_user = content or ""
                break
        msg = MagicMock()
        msg.content = f"OK: {last_user}"
        msg.role = "assistant"
        msg.tool_calls = []
        resp = MagicMock()
        resp.choices = [MagicMock(message=msg)]
        return resp


@pytest.fixture()
def capturing_mock(monkeypatch: pytest.MonkeyPatch) -> _CapturingMockClient:
    client = _CapturingMockClient()

    async def _mock_acompletion(
        model: str,
        messages: list,
        tools: list | None = None,
        **kwargs: object,
    ) -> Any:
        return await client._create(model=model, messages=messages, tools=tools, **kwargs)

    mock_litellm = types.ModuleType("litellm")
    mock_litellm.acompletion = _mock_acompletion  # type: ignore[attr-defined]
    mock_litellm.completion_cost = lambda *a, **kw: 0.0  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "litellm", mock_litellm)
    return client


@pytest.fixture()
def db_path(tmp_path: Path) -> str:
    return str(tmp_path / "sessions_t43.db")


# ---------------------------------------------------------------------------
# Fake AgentMemory — duck-typed, captures the key (scope user_id) + calls.
# No real Engram is opened in these unit tests.
# ---------------------------------------------------------------------------


class _FakeMemory:
    """A stand-in for :class:`AgentMemory`.

    ``record`` / ``context`` / ``recall`` capture the *key* they were scoped to
    (the session id, threaded via :meth:`as_scope`) plus their arguments, so a
    test can assert the loop keyed memory by ``session.id``.
    """

    def __init__(self, context_reply: str = "") -> None:
        # (key, text, role) tuples — the key is the scope user_id at call time.
        self.records: list[tuple[str | None, str, str | None]] = []
        # (key, query) tuples.
        self.context_calls: list[tuple[str | None, str]] = []
        self.context_reply = context_reply
        self._scoped_user_id: str | None = None

    @contextmanager
    def as_scope(self, *, user_id: str | None = None, org_id: str | None = None) -> Any:
        old = self._scoped_user_id
        self._scoped_user_id = user_id
        try:
            yield self
        finally:
            self._scoped_user_id = old

    async def record(
        self,
        text: str,
        *,
        role: str | None = None,
        category: str | None = None,
        confidence: float = 1.0,
        metadata: dict | None = None,
    ) -> None:
        self.records.append((self._scoped_user_id, text, role))

    async def context(
        self,
        query: str,
        *,
        token_budget: int | None = None,
        role_filter: tuple[str, ...] | None = None,
        decompose: bool | None = None,
    ) -> str:
        self.context_calls.append((self._scoped_user_id, query))
        return self.context_reply


def _agent(memory: Any = None, *, store: SessionStore | None = None) -> Agent:
    return Agent(
        "mem-tester",
        model="gpt-4o-mini",
        tools=[echo],
        strategy="react",
        memory=memory,
        session_store=store,
    )


# ---------------------------------------------------------------------------
# 1. memory=True without the engram extra -> FAIL LOUD
# ---------------------------------------------------------------------------


def test_memory_true_without_engram_raises(monkeypatch: pytest.MonkeyPatch) -> None:
    """``memory=True`` with engram missing raises a clear error naming the extra."""
    # Setting sys.modules["engram"] = None makes ``import engram`` raise ImportError.
    monkeypatch.setitem(sys.modules, "engram", None)
    with pytest.raises((ImportError, RuntimeError)) as exc_info:
        _agent(memory=True)
    assert "memory" in str(exc_info.value).lower()
    assert "jamjet[memory]" in str(exc_info.value)


# ---------------------------------------------------------------------------
# 2. Round-trip: record-at-end (run 1) -> retrieve-at-start + inject (run 2)
# ---------------------------------------------------------------------------


def test_memory_round_trip_record_then_retrieve(
    capturing_mock: _CapturingMockClient,
    db_path: str,
) -> None:
    store = SessionStore(db_path)
    s = store.create("mem-rt")
    fake = _FakeMemory(context_reply="MEMORY-BLOCK: the user asked to remember X")
    agent = _agent(memory=fake, store=store)

    # Run 1: the turn is RECORDED, keyed by session.id.
    asyncio.run(agent.run("remember X", session=s))
    assert fake.records, "run 1 should record the turn"
    # Every record is keyed by the stable session id (not an execution id).
    assert all(key == "mem-rt" for (key, _text, _role) in fake.records), fake.records
    # The user prompt was recorded.
    assert any("remember X" in text for (_key, text, _role) in fake.records)

    # Run 2: the context is RETRIEVED (keyed by session.id) and INJECTED.
    capturing_mock.calls.clear()
    asyncio.run(agent.run("recall X", session=s))

    # context() was called keyed by the session id with the new prompt.
    assert any(key == "mem-rt" and "recall X" in q for (key, q) in fake.context_calls), fake.context_calls

    # The retrieved block reached the model on the 2nd run.
    assert capturing_mock.calls, "run 2 should have hit the model"
    second_run_msgs = capturing_mock.calls[0]
    contents = " ".join(m.get("content", "") for m in second_run_msgs)
    assert "MEMORY-BLOCK" in contents, f"retrieved memory not injected into run 2: {second_run_msgs}"


# ---------------------------------------------------------------------------
# 3. PII is REDACTED before a memory write
# ---------------------------------------------------------------------------


def test_pii_redacted_before_memory_write(
    capturing_mock: _CapturingMockClient,
    db_path: str,
) -> None:
    store = SessionStore(db_path)
    s = store.create("mem-pii")
    fake = _FakeMemory()
    agent = _agent(memory=fake, store=store)

    asyncio.run(agent.run("my SSN is 123-45-6789", session=s))

    assert fake.records, "the turn should have been recorded"
    all_recorded = " ".join(text for (_key, text, _role) in fake.records)
    # Raw SSN must NEVER be written into memory.
    assert "123-45-6789" not in all_recorded, f"raw PII leaked into memory write: {fake.records}"
    # The redaction placeholder proves the Track-3 redactor ran on the write.
    assert "[REDACTED:US_SSN]" in all_recorded, f"expected redacted token in memory write: {fake.records}"


# ---------------------------------------------------------------------------
# 4. Memory OFF by default -> no memory calls, behaviour unchanged
# ---------------------------------------------------------------------------


def test_memory_off_by_default(
    capturing_mock: _CapturingMockClient,
    db_path: str,
) -> None:
    store = SessionStore(db_path)
    s = store.create("mem-off")
    agent = _agent(memory=None, store=store)  # default

    asyncio.run(agent.run("hello", session=s))

    assert capturing_mock.calls, "the run should still hit the model"
    first = capturing_mock.calls[0]
    contents = " ".join(m.get("content", "") for m in first)
    # No memory block is injected when memory is off.
    assert "Relevant memory" not in contents, f"memory block injected with memory off: {first}"


def test_memory_off_does_not_touch_injected_fake(
    capturing_mock: _CapturingMockClient,
    db_path: str,
) -> None:
    """A fake passed only as a sanity object is never used when memory is off.

    (We cannot pass a fake via memory= and have it stay off — memory= IS the
    switch — so this asserts the converse: a plain agent with no memory= makes
    zero memory calls by construction.)
    """
    store = SessionStore(db_path)
    s = store.create("mem-off2")
    agent = _agent(memory=None, store=store)
    # The agent holds no memory backend at all.
    assert agent._memory_enabled is False
    asyncio.run(agent.run("hello again", session=s))


# ---------------------------------------------------------------------------
# 5. Memory requires a session (fail-loud, never silently drop)
# ---------------------------------------------------------------------------


def test_memory_without_session_raises(capturing_mock: _CapturingMockClient) -> None:
    fake = _FakeMemory()
    agent = _agent(memory=fake)  # no session_store, and no session= on run
    with pytest.raises(RuntimeError, match="session"):
        asyncio.run(agent.run("hello"))
    # Memory must not have been touched on the failed path.
    assert fake.records == []
    assert fake.context_calls == []


def test_memory_without_session_raises_durable(capturing_mock: _CapturingMockClient) -> None:
    """run_durable enforces the same session requirement (before any engine call)."""
    fake = _FakeMemory()
    agent = _agent(memory=fake)
    with pytest.raises(RuntimeError, match="session"):
        asyncio.run(agent.run_durable("hello"))
    assert fake.records == []
    assert fake.context_calls == []


# ---------------------------------------------------------------------------
# 6. The injected AgentMemory is used as-is (duck-typed, no engram needed)
# ---------------------------------------------------------------------------


def test_injected_memory_is_used_directly(
    capturing_mock: _CapturingMockClient,
    db_path: str,
) -> None:
    store = SessionStore(db_path)
    s = store.create("mem-inject")
    fake = _FakeMemory(context_reply="hi from memory")
    agent = _agent(memory=fake, store=store)
    assert agent._memory_enabled is True
    asyncio.run(agent.run("ping", session=s))
    # The injected fake (not a freshly-built Engram bridge) handled the calls.
    assert fake.context_calls, "the injected memory should have served retrieval"
    assert fake.records, "the injected memory should have served the record"
