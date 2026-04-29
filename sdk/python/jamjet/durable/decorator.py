"""
The @durable decorator — auto-detects sync vs. async, wraps to consult an
idempotency cache before executing the wrapped function.
"""
from __future__ import annotations

import functools
import inspect
import os
from collections.abc import Callable
from pathlib import Path
from typing import Any, TypeVar, overload

from jamjet.durable.cache import Cache, SqliteCache
from jamjet.durable.context import get_execution_context
from jamjet.durable.keys import generate_key

F = TypeVar("F", bound=Callable[..., Any])


def _default_cache_path() -> Path:
    """Default cache lives at $JAMJET_DURABLE_DIR/cache.db, or ~/.jamjet/durable/cache.db."""
    base = os.environ.get("JAMJET_DURABLE_DIR")
    if base:
        return Path(base) / "cache.db"
    return Path.home() / ".jamjet" / "durable" / "cache.db"


_default_cache: Cache | None = None


def _get_default_cache() -> Cache:
    global _default_cache
    if _default_cache is None:
        _default_cache = SqliteCache(_default_cache_path())
    return _default_cache


@overload
def durable(fn: F) -> F: ...
@overload
def durable(*, cache: Cache | None = None) -> Callable[[F], F]: ...


def durable(
    fn: Callable[..., Any] | None = None,
    *,
    cache: Cache | None = None,
) -> Any:
    """
    Decorator: cache the result of `fn` against an idempotency key derived
    from (execution_id, fn_qualname, args, kwargs). On a second call within
    the same execution context with the same args, returns the cached value
    without re-executing `fn`.

    Note: functions that return `None` are treated as cache misses and will
    re-execute on every call. To cache nullable results, return a sentinel
    (e.g. `{"value": None}`) instead.

    Usage:
        @durable
        def charge_card(amount: float) -> dict: ...

        @durable(cache=my_cache)
        async def send_email(to: str): ...

    Must be called within a `durable_run(execution_id)` block; raises
    RuntimeError otherwise to prevent accidental no-op caching.
    """
    if fn is not None and callable(fn):
        # @durable form (no parens).
        return _wrap(fn, cache=cache or _get_default_cache())

    # @durable(...) form (with parens).
    def deco(f: F) -> F:
        return _wrap(f, cache=cache or _get_default_cache())  # type: ignore[return-value]

    return deco


def _wrap(fn: Callable[..., Any], *, cache: Cache) -> Callable[..., Any]:
    qualname = f"{fn.__module__}.{fn.__qualname__}"

    if inspect.iscoroutinefunction(fn):

        @functools.wraps(fn)
        async def async_wrapper(*args: Any, **kwargs: Any) -> Any:
            eid = get_execution_context()
            if eid is None:
                raise RuntimeError(
                    f"No execution context. @durable function {qualname} must be "
                    "called inside a `with durable_run(...):` block."
                )
            key = generate_key(eid, qualname, args, kwargs)
            cached = cache.get(key)
            if cached is not None:
                return cached
            result = await fn(*args, **kwargs)
            cache.put(key, result)
            return result

        return async_wrapper

    @functools.wraps(fn)
    def sync_wrapper(*args: Any, **kwargs: Any) -> Any:
        eid = get_execution_context()
        if eid is None:
            raise RuntimeError(
                f"No execution context. @durable function {qualname} must be "
                "called inside a `with durable_run(...):` block."
            )
        key = generate_key(eid, qualname, args, kwargs)
        cached = cache.get(key)
        if cached is not None:
            return cached
        result = fn(*args, **kwargs)
        cache.put(key, result)
        return result

    return sync_wrapper
