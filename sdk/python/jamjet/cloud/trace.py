from __future__ import annotations

import functools
import threading
import uuid
from contextvars import ContextVar
from typing import Any, Callable, TypeVar

from .events import emit
from .models import Span

F = TypeVar("F", bound=Callable[..., Any])


class TraceContext:
    """Holds the current trace ID and creates child spans with incrementing sequence."""

    def __init__(self) -> None:
        self.trace_id: str = "tr_" + uuid.uuid4().hex
        self._seq: int = 0
        self._lock = threading.Lock()

    def new_span(self, kind: str, name: str) -> Span:
        """Create a new Span belonging to this trace."""
        with self._lock:
            self._seq += 1
            seq = self._seq
        span_id = "sp_" + uuid.uuid4().hex
        return Span(
            trace_id=self.trace_id,
            span_id=span_id,
            kind=kind,
            name=name,
            sequence=seq,
        )


# ---------------------------------------------------------------------------
# ContextVar-based current trace
# ---------------------------------------------------------------------------

_trace_var: ContextVar[TraceContext | None] = ContextVar("jamjet_trace", default=None)


def get_context() -> TraceContext:
    """Return the current trace context, creating one if needed."""
    ctx = _trace_var.get()
    if ctx is None:
        ctx = TraceContext()
        _trace_var.set(ctx)
    return ctx


def set_context(ctx: TraceContext | None = None) -> TraceContext:
    """Set (or create) a trace context for the current async/thread context."""
    if ctx is None:
        ctx = TraceContext()
    _trace_var.set(ctx)
    return ctx


# ---------------------------------------------------------------------------
# @trace decorator
# ---------------------------------------------------------------------------


def trace(fn: F) -> F:
    """Decorator that wraps a function in a span and emits an event on completion."""

    @functools.wraps(fn)
    def wrapper(*args: Any, **kwargs: Any) -> Any:
        ctx = get_context()
        span = ctx.new_span(kind="custom", name=fn.__qualname__)
        try:
            result = fn(*args, **kwargs)
            span.finish(status="ok")
            return result
        except Exception as exc:
            span.finish(status="error")
            raise
        finally:
            emit(span.to_event_dict())

    return wrapper  # type: ignore[return-value]
