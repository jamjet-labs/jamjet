"""Tests for jamjet.durable.context — execution-id context management."""
import pytest

from jamjet.durable.context import (
    durable_run,
    get_execution_context,
    set_execution_context,
)


def test_no_context_returns_none():
    assert get_execution_context() is None


def test_set_execution_context_returns_token():
    token = set_execution_context("run-1")
    try:
        assert get_execution_context() == "run-1"
    finally:
        # context.reset_token would be ideal; for now we rely on durable_run
        # in normal usage. Manual set is for advanced use only.
        pass


def test_durable_run_sets_and_clears():
    assert get_execution_context() is None
    with durable_run("run-abc"):
        assert get_execution_context() == "run-abc"
    assert get_execution_context() is None


def test_durable_run_nesting_restores_outer():
    with durable_run("outer"):
        assert get_execution_context() == "outer"
        with durable_run("inner"):
            assert get_execution_context() == "inner"
        assert get_execution_context() == "outer"
    assert get_execution_context() is None


def test_durable_run_isolates_per_task():
    """contextvars must be coroutine-/task-local."""
    import asyncio

    async def task_a():
        with durable_run("a"):
            await asyncio.sleep(0.01)
            return get_execution_context()

    async def task_b():
        with durable_run("b"):
            await asyncio.sleep(0.01)
            return get_execution_context()

    async def main():
        return await asyncio.gather(task_a(), task_b())

    results = asyncio.run(main())
    assert results == ["a", "b"]


def test_durable_run_requires_str():
    with pytest.raises(TypeError):
        with durable_run(123):  # type: ignore[arg-type]
            pass
