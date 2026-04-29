"""Google ADK shim contract test."""

import pytest

pytest.importorskip("google.adk")

from jamjet.adk import durable_run
from jamjet.durable.context import get_execution_context


class _FakeAdkAgent:
    def __init__(self, session_id: str | None = None):
        self.session_id = session_id


def test_durable_run_uses_session_id():
    a = _FakeAdkAgent(session_id="adk-sess-1")
    with durable_run(a) as eid:
        assert eid == "adk-sess-1"
        assert get_execution_context() == "adk-sess-1"


def test_durable_run_generates_id_when_none():
    a = _FakeAdkAgent(session_id=None)
    with durable_run(a) as eid:
        assert isinstance(eid, str) and len(eid) > 0
        assert get_execution_context() == eid


def test_durable_run_clears_after_block():
    a = _FakeAdkAgent(session_id="adk-sess-2")
    with durable_run(a):
        pass
    assert get_execution_context() is None
