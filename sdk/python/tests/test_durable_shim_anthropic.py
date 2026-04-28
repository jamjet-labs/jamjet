"""Anthropic Agent SDK shim contract test."""
import pytest

pytest.importorskip("anthropic")

from jamjet.durable.context import get_execution_context
from jamjet.anthropic_agent import durable_run


class _FakeRun:
    """Stand-in for an Anthropic Agent SDK run handle."""

    def __init__(self, run_id: str | None = None):
        self.run_id = run_id


def test_durable_run_uses_run_id():
    r = _FakeRun(run_id="anthropic-run-1")
    with durable_run(r) as eid:
        assert eid == "anthropic-run-1"
        assert get_execution_context() == "anthropic-run-1"


def test_durable_run_generates_id_when_none():
    r = _FakeRun(run_id=None)
    with durable_run(r) as eid:
        assert isinstance(eid, str) and len(eid) > 0


def test_durable_run_clears_after_block():
    r = _FakeRun(run_id="anthropic-run-2")
    with durable_run(r):
        pass
    assert get_execution_context() is None
