"""Tests for the @durable decorator — sync, async, cache hit/miss, error paths."""

import asyncio

import pytest

from jamjet.durable import durable, durable_run
from jamjet.durable.cache import SqliteCache


@pytest.fixture
def cache(tmp_path):
    return SqliteCache(tmp_path / "cache.db")


def test_sync_no_context_raises(cache):
    """Calling @durable function outside durable_run raises clear error."""

    @durable(cache=cache)
    def my_tool(x: int) -> int:
        return x + 1

    with pytest.raises(RuntimeError, match="No execution context"):
        my_tool(5)


def test_sync_first_call_executes(cache):
    calls = []

    @durable(cache=cache)
    def my_tool(x: int) -> int:
        calls.append(x)
        return x + 1

    with durable_run("run-1"):
        result = my_tool(5)

    assert result == 6
    assert calls == [5]


def test_sync_second_call_returns_cached(cache):
    """Second call with same args returns cached result without re-executing."""
    calls = []

    @durable(cache=cache)
    def my_tool(x: int) -> int:
        calls.append(x)
        return x * 100

    with durable_run("run-1"):
        r1 = my_tool(5)
        r2 = my_tool(5)

    assert r1 == r2 == 500
    assert calls == [5]  # underlying function called exactly once


def test_sync_different_args_execute_separately(cache):
    calls = []

    @durable(cache=cache)
    def my_tool(x: int) -> int:
        calls.append(x)
        return x

    with durable_run("run-1"):
        my_tool(1)
        my_tool(2)
        my_tool(1)  # cache hit

    assert calls == [1, 2]


def test_sync_different_runs_execute_separately(cache):
    calls = []

    @durable(cache=cache)
    def my_tool(x: int) -> int:
        calls.append(x)
        return x

    with durable_run("run-A"):
        my_tool(1)
    with durable_run("run-B"):
        my_tool(1)  # different execution_id → different key → re-executes

    assert calls == [1, 1]


def test_sync_exception_not_cached(cache):
    """If the wrapped fn raises, the next call re-executes (no error caching)."""
    calls = []

    @durable(cache=cache)
    def my_tool(x: int) -> int:
        calls.append(x)
        if len(calls) == 1:
            raise RuntimeError("first call fails")
        return x

    with durable_run("run-1"):
        with pytest.raises(RuntimeError):
            my_tool(5)
        result = my_tool(5)  # should re-execute, not return cached error

    assert result == 5
    assert calls == [5, 5]


def test_async_first_call_executes(cache):
    calls = []

    @durable(cache=cache)
    async def my_tool(x: int) -> int:
        calls.append(x)
        return x + 1

    async def main():
        with durable_run("run-1"):
            return await my_tool(5)

    assert asyncio.run(main()) == 6
    assert calls == [5]


def test_async_second_call_returns_cached(cache):
    calls = []

    @durable(cache=cache)
    async def my_tool(x: int) -> int:
        calls.append(x)
        return x * 100

    async def main():
        with durable_run("run-1"):
            r1 = await my_tool(5)
            r2 = await my_tool(5)
            return r1, r2

    r1, r2 = asyncio.run(main())
    assert r1 == r2 == 500
    assert calls == [5]


def test_cache_persists_across_decorator_instances(cache):
    """A second @durable wrapper of the same fn name sees the prior cache."""
    calls = []

    def make_wrapper():
        @durable(cache=cache)
        def my_tool(x: int) -> int:
            calls.append(x)
            return x + 1

        return my_tool

    with durable_run("run-1"):
        first = make_wrapper()
        first(5)
        second = make_wrapper()
        second(5)  # same qualname + args + execution_id → cache hit

    assert calls == [5]
