"""Consolidated integration test for T4-5: session + memory + artifact together.

One test drives a single agent + session through ALL THREE headline guarantees
across a simulated process restart, using scripted fakes (no real model, no real
Engram, no real runtime):

  Guarantee 1 — Thread continuity
    Run 2 (after restart) receives the full thread from run 1 so the model sees
    prior turns.

  Guarantee 2 — Memory recall across restart
    The fake AgentMemory records after run 1; run 2 retrieves the context block
    keyed by session.id and it is injected into the model's messages.

  Guarantee 3 — Artifact round-trip
    Bytes stored via session.artifacts.put() before restart are fetched by hash
    from the same fake client after restart.

This test does NOT duplicate:
  - test_session.py          (T4-1: Session/SessionStore unit tests)
  - test_agent_session.py    (T4-2: thread-continues / survives-restart / parity)
  - test_agent_memory.py     (T4-3: memory round-trip / PII / fail-loud)
  - test_artifacts.py        (T4-4: ArtifactRef / ArtifactStore / Session.artifacts)

All three harnesses (capturing model, fake memory, fake artifact client) are
defined inline so this file is self-contained.
"""

from __future__ import annotations

import hashlib
import sys
import types
from collections.abc import Generator
from contextlib import contextmanager
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

import pytest

from jamjet import Agent, ArtifactRef, tool
from jamjet.agents.session import SessionStore

# ---------------------------------------------------------------------------
# Tool (minimal — just needs a defined @tool for Agent to accept)
# ---------------------------------------------------------------------------


@tool
async def echo(text: str) -> str:
    """Echo the input."""
    return text


# ---------------------------------------------------------------------------
# _CapturingModel — records every messages list the model receives
# ---------------------------------------------------------------------------


class _CapturingModel:
    """Scripted LLM stand-in that records calls and returns canned replies."""

    def __init__(self) -> None:
        self.calls: list[list[dict]] = []

    async def _acompletion(
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

        if "alice" in last_user.lower() or "my name is" in last_user.lower():
            reply = "Got it — your name is Alice."
        elif "what is my name" in last_user.lower():
            reply = "Your name is Alice."
        else:
            reply = f"OK: {last_user}"

        msg = MagicMock()
        msg.content = reply
        msg.role = "assistant"
        msg.tool_calls = []
        resp = MagicMock()
        resp.choices = [MagicMock(message=msg)]
        return resp


# ---------------------------------------------------------------------------
# _FakeMemory — duck-typed AgentMemory that captures record/context calls
# ---------------------------------------------------------------------------


class _FakeMemory:
    """Stand-in for AgentMemory; no real Engram required.

    Captures (scoped_user_id, text, role) tuples in .records so a test can
    assert the loop keyed memory by session.id.
    """

    def __init__(self, context_reply: str = "") -> None:
        self.records: list[tuple[str | None, str, str | None]] = []
        self.context_calls: list[tuple[str | None, str]] = []
        self.context_reply = context_reply
        self._scoped_user_id: str | None = None

    @contextmanager
    def as_scope(
        self,
        *,
        user_id: str | None = None,
        org_id: str | None = None,
    ) -> Generator[_FakeMemory, None, None]:
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


# ---------------------------------------------------------------------------
# _FakeArtifactClient — in-memory content-addressed artifact backend
# ---------------------------------------------------------------------------


class _FakeArtifactClient:
    """In-memory stand-in exposing the JamjetClient artifact surface."""

    def __init__(self) -> None:
        self._store: dict[str, bytes] = {}

    async def put_artifact(self, data: bytes, media_type: str | None = None) -> ArtifactRef:
        digest = hashlib.sha256(data).hexdigest()
        self._store[digest] = data
        return ArtifactRef(hash=digest, size=len(data), media_type=media_type)

    async def get_artifact(self, hash: str) -> bytes:
        return self._store[hash]


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture()
def capturing_model(monkeypatch: pytest.MonkeyPatch) -> _CapturingModel:
    """Patch litellm.acompletion with a capturing scripted model."""
    m = _CapturingModel()

    async def _acompletion(
        model: str,
        messages: list,
        tools: list | None = None,
        **kwargs: object,
    ) -> Any:
        return await m._acompletion(model=model, messages=messages, tools=tools, **kwargs)

    mock_litellm = types.ModuleType("litellm")
    mock_litellm.acompletion = _acompletion  # type: ignore[attr-defined]
    mock_litellm.completion_cost = lambda *a, **kw: 0.0  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "litellm", mock_litellm)
    return m


# ---------------------------------------------------------------------------
# The consolidated integration test
# ---------------------------------------------------------------------------


async def test_combined_session_memory_artifact(
    capturing_model: _CapturingModel,
    tmp_path: Path,
) -> None:
    """Three headline guarantees exercised together across a simulated restart.

    Flow
    ----
    1. Run 1  — creates session "si-1", runs agent, persists thread + memory.
    2. Restart — discards first Agent + store instances (simulating a new process).
    3. Run 2  — fresh Agent + fresh SessionStore; continues the session.
    4. Assertions — model received (a) prior thread AND (b) memory block; artifact
                    stored before restart is retrievable by hash after.
    """
    db_path = str(tmp_path / "integration.db")
    fake_mem = _FakeMemory(context_reply="MEMORY-BLOCK: user mentioned their name is Alice")
    fake_artifacts = _FakeArtifactClient()

    # ---- Run 1 ---------------------------------------------------------- #
    store1 = SessionStore(db_path)
    s1 = store1.create("si-1")
    s1.attach_client(fake_artifacts)  # wire artifact backend before the run

    agent1 = Agent(
        "si-agent",
        model="gpt-4o-mini",
        tools=[echo],
        strategy="react",
        memory=fake_mem,
        session_store=store1,
    )

    r1 = await agent1.run("my name is Alice", session=s1)
    assert r1.output, "run 1 must produce output"

    # Memory loop ran: record-at-end was called, keyed by the stable session.id
    assert fake_mem.records, "run 1 must record the turn to memory"
    record_keys = [k for k, _, _ in fake_mem.records]
    assert all(k == "si-1" for k in record_keys), (
        f"memory records must be keyed by session.id 'si-1', got: {record_keys}"
    )

    # Store an artifact (the in-process 'si-1' session carries the fake client)
    ref = await s1.artifacts.put(b"run-1-summary", "text/plain")
    assert ref.hash, "artifact put must return a non-empty hash"
    assert ref.size == len(b"run-1-summary")

    # ---- Restart -------------------------------------------------------- #
    # Discard the first Agent + SessionStore — simulates a new process startup.
    del agent1, store1
    capturing_model.calls.clear()  # reset call log; only run-2 calls matter

    store2 = SessionStore(db_path)  # same db path -> same sessions
    s2 = store2.load("si-1")
    assert s2 is not None, "session 'si-1' must survive the simulated restart"
    s2.attach_client(fake_artifacts)  # rewire to the same in-memory artifact backend

    agent2 = Agent(
        "si-agent",
        model="gpt-4o-mini",
        tools=[echo],
        strategy="react",
        memory=fake_mem,
        session_store=store2,
    )

    # ---- Run 2 ---------------------------------------------------------- #
    r2 = await agent2.run("what is my name?", session=s2)
    assert r2.output, "run 2 must produce output"

    # ---- Guarantee 1: thread continuity --------------------------------- #
    assert capturing_model.calls, "run 2 must hit the model"
    run2_msgs = capturing_model.calls[0]
    run2_content = " ".join(m.get("content", "") or "" for m in run2_msgs)
    run2_roles = [m.get("role", "") for m in run2_msgs]

    assert "alice" in run2_content.lower() or "my name is" in run2_content.lower(), (
        f"Thread continuity FAILED: run 2 did not receive run 1 turns.\nmessages={run2_msgs}"
    )
    assert "assistant" in run2_roles, (
        f"Thread continuity FAILED: no prior assistant turn in run 2 messages.\nmessages={run2_msgs}"
    )

    # ---- Guarantee 2: memory recall across restart ---------------------- #
    assert "MEMORY-BLOCK" in run2_content, (
        f"Memory recall FAILED: retrieved context block not injected into run 2.\nmessages={run2_msgs}"
    )
    assert any(k == "si-1" and "what is my name" in q for k, q in fake_mem.context_calls), (
        f"Memory recall FAILED: context() was not called with session key 'si-1'.\n"
        f"context_calls={fake_mem.context_calls}"
    )

    # ---- Guarantee 3: artifact round-trip ------------------------------- #
    fetched = await s2.artifacts.get(ref.hash)
    assert fetched == b"run-1-summary", f"Artifact round-trip FAILED: expected b'run-1-summary', got {fetched!r}"
