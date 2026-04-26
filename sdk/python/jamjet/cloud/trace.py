from __future__ import annotations

import functools
import threading
import uuid
from collections.abc import Callable
from contextvars import ContextVar
from typing import Any, TypeVar

from .agent import get_current_agent
from .events import emit
from .models import Span
from .propagation import get_originating
from .user_context import get_process_context, get_user_context

F = TypeVar("F", bound=Callable[..., Any])


class TraceContext:
    """Holds the current trace ID and creates child spans with incrementing sequence."""

    def __init__(self) -> None:
        self.trace_id: str = "tr_" + uuid.uuid4().hex
        self._seq: int = 0
        self._lock = threading.Lock()

    def new_span(self, kind: str, name: str) -> Span:
        """Create a new Span belonging to this trace, tagged with the current
        agent and (when applicable) cross-trace lineage from the upstream caller."""
        with self._lock:
            self._seq += 1
            seq = self._seq
        span_id = "sp_" + uuid.uuid4().hex
        # Current agent — Plan 5 Phase 1.
        current_agent = get_current_agent()
        # Originating span (set by extract_headers on the receiver) — Phase 2.
        originating = get_originating()
        # Session / end-user / environment / release / tags — Phase 2 bonus.
        proc = get_process_context()
        user_ctx = get_user_context()
        return Span(
            trace_id=self.trace_id,
            span_id=span_id,
            kind=kind,
            name=name,
            sequence=seq,
            agent_name=current_agent.name if current_agent else None,
            agent_card_uri=current_agent.card_uri if current_agent else None,
            originating_trace_id=originating.trace_id if originating else None,
            originating_span_id=originating.span_id if originating else None,
            originating_agent_name=originating.agent_name if originating else None,
            session_id=user_ctx.session_id if user_ctx else None,
            environment=proc.environment,
            release_version=proc.release_version,
            end_user_id=user_ctx.end_user_id if user_ctx else None,
            end_user_email=user_ctx.end_user_email if user_ctx else None,
            tags=user_ctx.tags if user_ctx else (),
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
    """Decorator that wraps a function in a span and emits an event on completion.

    On exception, classifies via the same heuristic the LLM patchers use so
    @trace-decorated wrappers around custom code show up in the failure-mode
    pie chart with a sensible category.
    """

    @functools.wraps(fn)
    def wrapper(*args: Any, **kwargs: Any) -> Any:
        ctx = get_context()
        span = ctx.new_span(kind="custom", name=fn.__qualname__)
        try:
            result = fn(*args, **kwargs)
            span.finish(status="ok")
            return result
        except Exception as exc:
            # Lazy import to avoid pulling patcher into trace.py's import graph.
            from .patcher import _classify_exception

            span.fail(mode=_classify_exception(exc))
            raise
        finally:
            emit(span.to_event_dict())

    return wrapper  # type: ignore[return-value]
