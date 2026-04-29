"""OpenAI Agents SDK shim contract test."""

import pytest

pytest.importorskip("agents")  # OpenAI Agents SDK package import name

from jamjet.durable.context import get_execution_context
from jamjet.openai_agents import durable_run


class _FakeRunner:
    def __init__(self, run_id: str | None = None):
        self.run_id = run_id


def test_durable_run_uses_run_id():
    r = _FakeRunner(run_id="oa-run-1")
    with durable_run(r) as eid:
        assert eid == "oa-run-1"
        assert get_execution_context() == "oa-run-1"


def test_durable_run_generates_id_when_none():
    r = _FakeRunner(run_id=None)
    with durable_run(r) as eid:
        assert isinstance(eid, str) and len(eid) > 0


def test_durable_run_clears_after_block():
    r = _FakeRunner(run_id="oa-run-2")
    with durable_run(r):
        pass
    assert get_execution_context() is None


class _FakeRunnerIdOnly:
    """Stand-in runner exposing only `.id`, not `.run_id` — exercises the fallback chain."""

    def __init__(self, id_: str):
        self.id = id_


def test_durable_run_falls_back_to_id_when_run_id_missing():
    r = _FakeRunnerIdOnly(id_="oa-id-only")
    with durable_run(r) as eid:
        assert eid == "oa-id-only"
        assert get_execution_context() == "oa-id-only"
