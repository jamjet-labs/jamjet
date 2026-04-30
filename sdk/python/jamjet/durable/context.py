"""
Execution-context management for jamjet.durable.

The `execution_id` is the namespace under which idempotency keys are scoped.
A single agent run sets a context; tool calls within that run share the same
execution_id, so that cache hits work across crash/restart boundaries.

Implemented with contextvars so context propagates correctly across asyncio
tasks (each task has its own logical context).
"""

from __future__ import annotations

from collections.abc import Iterator
from contextlib import contextmanager
from contextvars import ContextVar, Token

_execution_id: ContextVar[str | None] = ContextVar("jamjet_durable_execution_id", default=None)


def get_execution_context() -> str | None:
    """Return the current execution_id, or None if not in a durable_run."""
    return _execution_id.get()


def set_execution_context(execution_id: str) -> Token:
    """
    Manually set the execution_id. Returns a Token that can be passed to
    `_execution_id.reset(token)` to restore the previous context.

    Prefer `durable_run()` for normal use — it handles cleanup automatically.
    """
    if not isinstance(execution_id, str):
        raise TypeError(f"execution_id must be str, got {type(execution_id).__name__}")
    return _execution_id.set(execution_id)


def reset_execution_context(token: Token) -> None:
    """
    Restore the execution_id to its prior value, given a Token returned
    from `set_execution_context()`.

    Prefer `durable_run()` for normal use — it pairs set + reset automatically.
    """
    _execution_id.reset(token)


@contextmanager
def durable_run(execution_id: str) -> Iterator[str]:
    """
    Set the execution_id for the duration of the `with` block.

    Usage:
        with durable_run("agent-run-abc-123"):
            charge_card(847)  # @durable wraps this
    """
    if not isinstance(execution_id, str):
        raise TypeError(f"execution_id must be str, got {type(execution_id).__name__}")
    token = _execution_id.set(execution_id)
    try:
        yield execution_id
    finally:
        _execution_id.reset(token)
