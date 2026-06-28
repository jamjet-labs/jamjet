"""Tests for T4-2: Agent.run / run_durable continue a Session thread.

Tests:
- thread_continues: the second run's input messages contain the first turn.
- survives_restart: a fresh Agent + fresh SessionStore loads the session and
  the third run sees the first two turns (restart across process boundary).
- no_session: agent.run() without session= behaves exactly as before.
- parity: seed_messages_for_run + persist_session_turn helpers are the same
  for both run() and run_durable() (unit-tested directly).
- session_run_method: Session.run(agent, prompt) ergonomic form.
- str_session_id: session= may be a str; the agent resolves via the store.
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
from jamjet.agents.session import (
    Session,
    SessionStore,
    persist_session_turn,
    seed_messages_for_run,
)

# ---------------------------------------------------------------------------
# Minimal tool for all tests
# ---------------------------------------------------------------------------


@tool
async def echo(text: str) -> str:
    """Echo the input."""
    return text


# ---------------------------------------------------------------------------
# Capturing mock: records every messages list the model sees
# ---------------------------------------------------------------------------


class _CapturingMockClient:
    """Mock OpenAI client that records the messages list for every call.

    On each call it returns a simple text reply (no tool calls) so the
    react strategy terminates immediately.
    """

    def __init__(self, *args: object, **kwargs: object) -> None:
        self.chat = MagicMock()
        self.chat.completions = MagicMock()
        self.chat.completions.create = self._create
        self.calls: list[list[dict]] = []

    async def _create(
        self,
        model: str,
        messages: list,
        tools: list | None = None,
        **kwargs: object,
    ) -> Any:
        # Record a deep copy of the messages for inspection.
        self.calls.append(
            [
                dict(m)
                if isinstance(m, dict)
                else {"role": getattr(m, "role", "?"), "content": getattr(m, "content", "")}
                for m in messages
            ]
        )

        # Determine a canned reply based on the last user message.
        last_user = ""
        for m in reversed(messages):
            role = m.get("role") if isinstance(m, dict) else getattr(m, "role", "")
            content = m.get("content") if isinstance(m, dict) else getattr(m, "content", "")
            if role == "user":
                last_user = content or ""
                break

        # Map prompts to canned replies for determinism.
        if "alice" in last_user.lower() or "my name is" in last_user.lower():
            reply = "Got it — your name is Alice."
        elif "what is my name" in last_user.lower():
            reply = "Your name is Alice."
        elif "third" in last_user.lower():
            reply = "This is the third turn."
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
# Fixture: replace litellm.acompletion with the capturing client for a test
# ---------------------------------------------------------------------------


@pytest.fixture()
def capturing_mock(monkeypatch: pytest.MonkeyPatch) -> _CapturingMockClient:
    """Patch litellm.acompletion so we can inspect the messages per call."""
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
    return str(tmp_path / "sessions_t42.db")


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_agent(strategy: str = "react") -> Agent:
    return Agent("tester", model="gpt-4o-mini", tools=[echo], strategy=strategy)


# ---------------------------------------------------------------------------
# T4-2 core: thread continues
# ---------------------------------------------------------------------------


def test_thread_continues_second_run_sees_first_turn(
    capturing_mock: _CapturingMockClient,
    db_path: str,
) -> None:
    """Second agent.run() with session= must send prior turns to the model."""
    store = SessionStore(db_path)
    s = store.create("s1")
    agent = Agent("tester", model="gpt-4o-mini", tools=[echo], strategy="react", session_store=store)

    asyncio.run(agent.run("my name is Alice", session=s))
    asyncio.run(agent.run("what is my name?", session=s))

    # The capturing mock should have at least two calls.
    assert len(capturing_mock.calls) >= 2

    # The SECOND call's messages must contain a turn about "Alice".
    second_call_messages = capturing_mock.calls[1]
    roles = [m["role"] for m in second_call_messages]
    contents = " ".join(m.get("content", "") for m in second_call_messages)

    # Prior user prompt must appear in the second run's input.
    assert "alice" in contents.lower() or "my name is" in contents.lower(), (
        f"Expected 'alice' in second call messages, got: {second_call_messages}"
    )
    # The second run's prompt also appears.
    assert "what is my name" in contents.lower(), (
        f"Expected 'what is my name' in second call, got: {second_call_messages}"
    )
    # There's at least one prior assistant turn in the messages.
    assert "assistant" in roles, f"Expected prior assistant turn in second call: {second_call_messages}"


def test_session_messages_after_both_runs(
    capturing_mock: _CapturingMockClient,
    db_path: str,
) -> None:
    """After two runs the session contains both turns (user+assistant each)."""
    store = SessionStore(db_path)
    s = store.create("s2")
    agent = Agent("tester", model="gpt-4o-mini", tools=[echo], strategy="react", session_store=store)

    asyncio.run(agent.run("my name is Alice", session=s))
    asyncio.run(agent.run("what is my name?", session=s))

    # Reload from store to verify persistence.
    reloaded = store.load("s2")
    assert reloaded is not None
    roles = [m["role"] for m in reloaded.messages]
    # Must have at least two user turns and two assistant turns.
    assert roles.count("user") >= 2, f"Expected >=2 user turns, got: {roles}"
    assert roles.count("assistant") >= 2, f"Expected >=2 assistant turns, got: {roles}"


# ---------------------------------------------------------------------------
# T4-2: survives restart
# ---------------------------------------------------------------------------


def test_survives_restart(
    capturing_mock: _CapturingMockClient,
    db_path: str,
) -> None:
    """A fresh Agent + fresh SessionStore load continues the thread correctly.

    Run 1 + 2: first Agent instance.
    Run 3: brand-new Agent + brand-new SessionStore pointing at same db.
    The third run's messages must contain turns from runs 1 and 2.
    """
    store1 = SessionStore(db_path)
    s = store1.create("restart-1")
    agent1 = Agent("a1", model="gpt-4o-mini", tools=[echo], strategy="react", session_store=store1)

    asyncio.run(agent1.run("my name is Alice", session=s))
    asyncio.run(agent1.run("what is my name?", session=s))

    # Simulate a process restart: discard agent1 + store1, create fresh instances.
    del agent1
    del store1

    store2 = SessionStore(db_path)  # same db path
    s_reloaded = store2.load("restart-1")
    assert s_reloaded is not None, "Session not found after restart"

    agent2 = Agent("a2", model="gpt-4o-mini", tools=[echo], strategy="react", session_store=store2)

    # Reset the capturing mock's call log before the third run.
    capturing_mock.calls.clear()
    asyncio.run(agent2.run("this is the third turn", session=s_reloaded))

    assert len(capturing_mock.calls) >= 1
    third_call_messages = capturing_mock.calls[0]
    contents = " ".join(m.get("content", "") for m in third_call_messages)

    # The third run must see the prior "Alice" turns from runs 1 and 2.
    assert "alice" in contents.lower() or "my name is" in contents.lower(), (
        f"Third run did not see prior turns. Messages: {third_call_messages}"
    )


# ---------------------------------------------------------------------------
# T4-2: no session — default run path unchanged
# ---------------------------------------------------------------------------


def test_no_session_default_path_unchanged(
    capturing_mock: _CapturingMockClient,
    db_path: str,
) -> None:
    """agent.run() without session= sends only the system + user prompt."""
    agent = Agent("tester", model="gpt-4o-mini", tools=[echo], strategy="react")
    asyncio.run(agent.run("hello world"))

    assert len(capturing_mock.calls) >= 1
    first_call = capturing_mock.calls[0]
    roles = [m["role"] for m in first_call]
    # No prior assistant turns — fresh seed.
    assert "assistant" not in roles, f"No-session run should not have assistant messages: {first_call}"
    assert roles.count("user") == 1, f"No-session run should have exactly 1 user message: {first_call}"


# ---------------------------------------------------------------------------
# T4-2: parity — shared helper unit tests
# ---------------------------------------------------------------------------


def test_seed_messages_no_session() -> None:
    """seed_messages_for_run without a session returns the default scratch seed."""
    msgs = seed_messages_for_run(None, "Be helpful.", "hello")
    assert msgs == [
        {"role": "system", "content": "Be helpful."},
        {"role": "user", "content": "hello"},
    ]


def test_seed_messages_with_session() -> None:
    """seed_messages_for_run with a session prepends prior non-system turns."""
    s = Session("x")
    s.messages = [
        {"role": "user", "content": "turn 1"},
        {"role": "assistant", "content": "reply 1"},
    ]
    msgs = seed_messages_for_run(s, "Be helpful.", "turn 2")
    assert msgs == [
        {"role": "system", "content": "Be helpful."},
        {"role": "user", "content": "turn 1"},
        {"role": "assistant", "content": "reply 1"},
        {"role": "user", "content": "turn 2"},
    ]


def test_seed_messages_strips_stale_system() -> None:
    """seed_messages_for_run drops any system messages stored in the session."""
    s = Session("y")
    s.messages = [
        {"role": "system", "content": "OLD instructions"},
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": "hello"},
    ]
    msgs = seed_messages_for_run(s, "NEW instructions", "next")
    roles = [m["role"] for m in msgs]
    assert roles.count("system") == 1, "Should have exactly one system message"
    assert msgs[0]["content"] == "NEW instructions", "System must come from current instructions"


def test_persist_session_turn_no_full_messages(db_path: str) -> None:
    """persist_session_turn with full_messages=None appends user+assistant."""
    store = SessionStore(db_path)
    s = store.create("p1")

    persist_session_turn(s, "hello", "hi there", "exec-001", full_messages=None, store=store)

    loaded = store.load("p1")
    assert loaded is not None
    assert loaded.messages == [
        {"role": "user", "content": "hello"},
        {"role": "assistant", "content": "hi there"},
    ]
    assert loaded.latest_execution_id == "exec-001"


def test_persist_session_turn_with_full_messages(db_path: str) -> None:
    """persist_session_turn with full_messages uses them (strips system)."""
    store = SessionStore(db_path)
    s = store.create("p2")

    full_msgs = [
        {"role": "system", "content": "sys"},
        {"role": "user", "content": "hello"},
        {"role": "assistant", "content": "world"},
        {"role": "tool", "content": "tool result"},
        {"role": "assistant", "content": "final"},
    ]
    persist_session_turn(s, "hello", "final", "exec-002", full_messages=full_msgs, store=store)

    loaded = store.load("p2")
    assert loaded is not None
    # System stripped; the rest preserved.
    assert all(m["role"] != "system" for m in loaded.messages)
    assert loaded.messages[0] == {"role": "user", "content": "hello"}
    assert loaded.latest_execution_id == "exec-002"


class _FakeDurableClient:
    """Async-context-manager fake of JamjetClient for the run_durable seam test.

    Captures the ``initial_input`` passed to ``start_execution`` (so a test can
    assert the session thread was SEEDED) and returns a terminal execution whose
    ``current_state.messages`` is the durable ledger (so the persist seam runs).
    """

    def __init__(self, execution: dict[str, Any]) -> None:
        self._execution = execution
        self.started: list[tuple[str, dict[str, Any]]] = []

    async def __aenter__(self) -> _FakeDurableClient:
        return self

    async def __aexit__(self, *args: Any) -> None:
        return None

    async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
        return {"workflow_id": ir["workflow_id"]}

    async def start_execution(
        self, workflow_id: str, input: dict[str, Any], workflow_version: str | None = None
    ) -> dict[str, Any]:
        self.started.append((workflow_id, input))
        return {"execution_id": "exec-durable-1"}

    async def get_execution(self, execution_id: str) -> dict[str, Any]:
        return self._execution

    async def get_events(self, execution_id: str) -> dict[str, Any]:
        return {"events": []}


def test_run_durable_seeds_and_persists_session_thread(
    db_path: str,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The REAL run_durable() path seeds the carried thread + persists the ledger.

    Drives ``Agent.run_durable`` against a mocked durable engine (no live engine)
    and asserts BOTH ends of the carried-state seam: the prior session thread is
    seeded into the execution's ``initial_input``, and the durable ledger is
    persisted back to the session.  Unlike the old test (which called
    ``seed_messages_for_run`` directly and stayed green even if run_durable
    stopped seeding/persisting), this exercises the actual run_durable seam.
    """
    store = SessionStore(db_path)
    s = store.create("durable-seam")
    s.append_message("user", "my name is Alice")
    s.append_message("assistant", "your name is Alice")
    store.save(s)

    completed = {
        "execution_id": "exec-durable-1",
        "status": "completed",
        "current_state": {
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "my name is Alice"},
                {"role": "assistant", "content": "your name is Alice"},
                {"role": "user", "content": "what is my name?"},
                {"role": "assistant", "content": "Your name is Alice."},
            ],
            "last_model_output": "Your name is Alice.",
        },
    }
    fake = _FakeDurableClient(completed)
    # run_durable does `from jamjet.client import JamjetClient` at call time, so
    # patching the module attribute swaps in our fake (mirrors run_durable tests).
    monkeypatch.setattr("jamjet.client.JamjetClient", lambda *a, **k: fake)

    agent = Agent("d", model="gpt-4o-mini", tools=[echo], session_store=store)
    result = asyncio.run(agent.run_durable("what is my name?", session=s))

    # SEED: the prior session thread reached start_execution's initial_input.
    assert fake.started, "run_durable must have started an execution"
    _wf_id, initial_input = fake.started[0]
    seeded = " ".join(str(m.get("content", "")) for m in initial_input["messages"])
    assert "my name is Alice" in seeded, initial_input["messages"]
    assert "what is my name?" in seeded, initial_input["messages"]

    # PERSIST: the session now carries the durable ledger (system stripped) and
    # the execution id — written through the real run_durable persist seam.
    reloaded = store.load("durable-seam")
    assert reloaded is not None
    assert all(m["role"] != "system" for m in reloaded.messages)
    assert {"role": "user", "content": "what is my name?"} in reloaded.messages
    assert {"role": "assistant", "content": "Your name is Alice."} in reloaded.messages
    assert reloaded.latest_execution_id == "exec-durable-1"
    assert result.output == "Your name is Alice."


# ---------------------------------------------------------------------------
# T4-2: Session.run() ergonomic method
# ---------------------------------------------------------------------------


def test_session_run_method(
    capturing_mock: _CapturingMockClient,
    db_path: str,
) -> None:
    """Session.run(agent, prompt) is equivalent to agent.run(prompt, session=self)."""
    store = SessionStore(db_path)
    s = store.create("erg-1")
    agent = Agent("erg", model="gpt-4o-mini", tools=[echo], strategy="react", session_store=store)

    result = asyncio.run(s.run(agent, "my name is Alice"))
    assert result.output  # non-empty

    # Session should be updated.
    loaded = store.load("erg-1")
    assert loaded is not None
    roles = [m["role"] for m in loaded.messages]
    assert "user" in roles
    assert "assistant" in roles


# ---------------------------------------------------------------------------
# T4-2: str session id form
# ---------------------------------------------------------------------------


def test_str_session_id(
    capturing_mock: _CapturingMockClient,
    db_path: str,
) -> None:
    """session= may be a str; the agent resolves it from the session_store."""
    store = SessionStore(db_path)
    store.create("str-session-1")

    agent = Agent("str-test", model="gpt-4o-mini", tools=[echo], strategy="react", session_store=store)

    result = asyncio.run(agent.run("my name is Alice", session="str-session-1"))
    assert result.output

    loaded = store.load("str-session-1")
    assert loaded is not None
    assert any(m["role"] == "user" for m in loaded.messages)


def test_str_session_id_not_found(db_path: str) -> None:
    """str session id not in store raises ValueError (fail-loud)."""
    store = SessionStore(db_path)
    agent = Agent("err", model="gpt-4o-mini", tools=[echo], session_store=store)
    with pytest.raises(ValueError, match="not found"):
        asyncio.run(agent.run("hello", session="no-such-session"))


# ---------------------------------------------------------------------------
# Finding 1 — create() is get-or-create; it must never clobber an existing
# session (save() is INSERT OR REPLACE, so a naive create() wiped the thread).
# ---------------------------------------------------------------------------


def test_create_does_not_clobber_existing_session(db_path: str) -> None:
    """create() over an existing id returns it UNCHANGED (no silent data loss)."""
    store = SessionStore(db_path)
    s = store.create("s1")
    s.append_message("user", "remember me")
    s.append_message("assistant", "noted")
    s.metadata["k"] = "v"
    store.save(s)

    # create("s1") again must NOT wipe the thread/metadata.
    again = store.create("s1")
    assert again.messages == [
        {"role": "user", "content": "remember me"},
        {"role": "assistant", "content": "noted"},
    ], "create() clobbered the existing thread (silent data loss)"
    assert again.metadata == {"k": "v"}

    # The persisted row is intact too (not just the returned object).
    reloaded = store.load("s1")
    assert reloaded is not None
    assert reloaded.messages == again.messages
    assert reloaded.metadata == {"k": "v"}


def test_create_new_id_still_creates_empty_session(db_path: str) -> None:
    """create() for a genuinely new id still creates+persists an empty session."""
    store = SessionStore(db_path)
    s = store.create("fresh")
    assert s.messages == []
    assert store.load("fresh") is not None


# ---------------------------------------------------------------------------
# Finding 4 — a session loaded from store X is persisted back to X, not to the
# agent's default store.
# ---------------------------------------------------------------------------


def test_session_persists_back_to_its_originating_store(
    capturing_mock: _CapturingMockClient,
    tmp_path: Path,
) -> None:
    """A Session loaded from store A is saved back to A, never the agent's store B."""
    store_a = SessionStore(str(tmp_path / "store_a.db"))  # the session's origin
    store_b = SessionStore(str(tmp_path / "store_b.db"))  # the agent's default store
    store_a.create("cross")

    loaded = store_a.load("cross")  # carries a back-ref to store_a
    assert loaded is not None
    agent = Agent("x", model="gpt-4o-mini", tools=[echo], strategy="react", session_store=store_b)

    asyncio.run(agent.run("my name is Alice", session=loaded))

    # The turn landed in store A (the originating store) ...
    from_a = store_a.load("cross")
    assert from_a is not None
    assert any(m["role"] == "user" for m in from_a.messages), "run did not persist to the originating store A"
    # ... and NOT in the agent's default store B.
    assert store_b.load("cross") is None, "run leaked the session into the agent's default store B"
