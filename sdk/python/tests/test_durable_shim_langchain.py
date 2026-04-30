"""LangChain shim contract test — durable_run sets execution context from executor.run_id."""

import pytest

pytest.importorskip("langchain")

from jamjet.durable.context import get_execution_context
from jamjet.langchain import durable_run


class _FakeExecutor:
    """Minimal stand-in for langchain.AgentExecutor — we only read .run_id."""

    def __init__(self, run_id: str | None = None):
        self.run_id = run_id


def test_durable_run_uses_executor_run_id():
    ex = _FakeExecutor(run_id="lc-run-123")
    with durable_run(ex) as eid:
        assert eid == "lc-run-123"
        assert get_execution_context() == "lc-run-123"


def test_durable_run_generates_id_when_none():
    ex = _FakeExecutor(run_id=None)
    with durable_run(ex) as eid:
        assert isinstance(eid, str) and len(eid) > 0
        assert get_execution_context() == eid


def test_durable_run_clears_after_block():
    ex = _FakeExecutor(run_id="lc-run-456")
    with durable_run(ex):
        pass
    assert get_execution_context() is None


def test_durable_run_yields_executor():
    ex = _FakeExecutor(run_id="lc-run-789")
    with durable_run(ex) as eid:
        # eid is the execution_id; the executor itself is the input.
        assert eid == "lc-run-789"
