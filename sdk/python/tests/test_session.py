"""Tests for Session + SessionStore (T4-1).

All tests use a temporary file path so they do not touch ~/.jamjet/sessions.db.
"""

from __future__ import annotations

import uuid
from pathlib import Path

import pytest

from jamjet.agents.session import Session, SessionStore

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


@pytest.fixture()
def db_path(tmp_path: Path) -> str:
    """Return a fresh, unique SQLite path inside pytest's tmp_path."""
    return str(tmp_path / "sessions.db")


# ---------------------------------------------------------------------------
# Session dataclass
# ---------------------------------------------------------------------------


def test_session_defaults():
    s = Session(id="abc")
    assert s.id == "abc"
    assert s.messages == []
    assert s.latest_execution_id is None
    assert s.metadata == {}


def test_session_append_message():
    s = Session(id="abc")
    s.append_message("user", "hello")
    s.append_message("assistant", "hi there")
    assert len(s.messages) == 2
    assert s.messages[0] == {"role": "user", "content": "hello"}
    assert s.messages[1] == {"role": "assistant", "content": "hi there"}


# ---------------------------------------------------------------------------
# SessionStore.create
# ---------------------------------------------------------------------------


def test_create_generates_id(db_path: str):
    store = SessionStore(db_path)
    s = store.create()
    assert s.id  # non-empty
    # Should be a valid UUID4
    parsed = uuid.UUID(s.id)
    assert str(parsed) == s.id


def test_create_with_explicit_id(db_path: str):
    store = SessionStore(db_path)
    s = store.create("s1")
    assert s.id == "s1"


def test_create_returns_empty_session(db_path: str):
    store = SessionStore(db_path)
    s = store.create("s2")
    assert s.messages == []
    assert s.latest_execution_id is None
    assert s.metadata == {}


# ---------------------------------------------------------------------------
# SessionStore.load
# ---------------------------------------------------------------------------


def test_load_unknown_returns_none(db_path: str):
    store = SessionStore(db_path)
    result = store.load("does-not-exist")
    assert result is None


def test_load_after_create(db_path: str):
    store = SessionStore(db_path)
    store.create("x")
    loaded = store.load("x")
    assert loaded is not None
    assert loaded.id == "x"


# ---------------------------------------------------------------------------
# Save / load round-trip
# ---------------------------------------------------------------------------


def test_save_load_roundtrip_messages(db_path: str):
    store = SessionStore(db_path)
    s = Session(id="rt1")
    s.messages = [
        {"role": "user", "content": "first"},
        {"role": "assistant", "content": "second"},
    ]
    store.save(s)

    loaded = store.load("rt1")
    assert loaded is not None
    assert loaded.messages == s.messages


def test_save_load_roundtrip_latest_execution_id(db_path: str):
    store = SessionStore(db_path)
    s = Session(id="rt2", latest_execution_id="exec-abc-123")
    store.save(s)

    loaded = store.load("rt2")
    assert loaded is not None
    # user session id is distinct from engine lineage id
    assert loaded.id == "rt2"
    assert loaded.latest_execution_id == "exec-abc-123"
    assert loaded.id != loaded.latest_execution_id


def test_save_load_roundtrip_metadata(db_path: str):
    store = SessionStore(db_path)
    s = Session(id="rt3", metadata={"agent": "my-agent", "version": 2})
    store.save(s)

    loaded = store.load("rt3")
    assert loaded is not None
    assert loaded.metadata == {"agent": "my-agent", "version": 2}


def test_save_upserts(db_path: str):
    """Saving the same session twice keeps the latest state."""
    store = SessionStore(db_path)
    s = Session(id="upsert-1")
    s.messages = [{"role": "user", "content": "v1"}]
    store.save(s)

    s.messages.append({"role": "assistant", "content": "v2"})
    s.latest_execution_id = "exec-v2"
    store.save(s)

    loaded = store.load("upsert-1")
    assert loaded is not None
    assert len(loaded.messages) == 2
    assert loaded.latest_execution_id == "exec-v2"


# ---------------------------------------------------------------------------
# append_message + save/load
# ---------------------------------------------------------------------------


def test_append_message_then_save_load(db_path: str):
    store = SessionStore(db_path)
    s = store.create("am-1")
    s.append_message("user", "question")
    s.append_message("assistant", "answer")
    store.save(s)

    loaded = store.load("am-1")
    assert loaded is not None
    assert loaded.messages == [
        {"role": "user", "content": "question"},
        {"role": "assistant", "content": "answer"},
    ]


# ---------------------------------------------------------------------------
# Survives restart
# ---------------------------------------------------------------------------


def test_survives_restart(db_path: str):
    """Persist a session, then a FRESH store instance must load it correctly."""
    # First store instance saves a 2-message thread
    store1 = SessionStore(db_path)
    s = store1.create("persist-1")
    s.append_message("user", "turn one")
    s.append_message("assistant", "reply one")
    s.latest_execution_id = "exec-restart-001"
    s.metadata = {"key": "val"}
    store1.save(s)

    # Discard store1 — simulate process restart with a new store instance
    store2 = SessionStore(db_path)
    loaded = store2.load("persist-1")

    assert loaded is not None
    assert loaded.id == "persist-1"
    assert len(loaded.messages) == 2
    assert loaded.messages[0] == {"role": "user", "content": "turn one"}
    assert loaded.messages[1] == {"role": "assistant", "content": "reply one"}
    assert loaded.latest_execution_id == "exec-restart-001"
    assert loaded.metadata == {"key": "val"}


# ---------------------------------------------------------------------------
# SessionStore.list
# ---------------------------------------------------------------------------


def test_list_empty(db_path: str):
    store = SessionStore(db_path)
    assert store.list() == []


def test_list_returns_saved_ids(db_path: str):
    store = SessionStore(db_path)
    store.create("a")
    store.create("b")
    store.create("c")
    ids = store.list()
    assert set(ids) == {"a", "b", "c"}


def test_list_excludes_unsaved(db_path: str):
    store = SessionStore(db_path)
    store.create("saved")
    # create a Session object in memory but never save it
    _ = Session(id="not-saved")
    ids = store.list()
    assert "not-saved" not in ids
    assert "saved" in ids


# ---------------------------------------------------------------------------
# Top-level import
# ---------------------------------------------------------------------------


def test_importable_from_jamjet():
    import jamjet

    assert jamjet.Session is Session
    assert jamjet.SessionStore is SessionStore


def test_importable_from_jamjet_agents():
    import jamjet.agents

    assert jamjet.agents.Session is Session
    assert jamjet.agents.SessionStore is SessionStore
