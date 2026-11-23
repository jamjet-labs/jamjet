"""What a handler can know about the work item it is running for.

A ``jamjet worker`` sets a :class:`WorkContext` before it calls a handler and
clears it afterwards. The context travels through :func:`asyncio.to_thread`,
so a synchronous handler running in a worker thread sees the same object as an
``async`` one.

The one thing a handler has to check today is ``cancel_requested``: the engine
reports a cancelled execution on the heartbeat, the worker sets the event and
stops waiting, and a long-running step (a model inference loop, say) polls the
event between chunks and returns early. Nothing the handler returns after that
is reported.

``idempotency_key`` is the key the engine gave the item on claim, or ``None``
from an older engine. It is the same on every attempt of the step, so a
handler that writes outside JamJet can pass it on and have a repeat ignored.
``run_key`` is the caller's key for the run when the process working the item
started it with one; ``None`` otherwise.
"""

from __future__ import annotations

import contextvars
import threading
from dataclasses import dataclass, field


@dataclass
class WorkContext:
    item_id: str
    execution_id: str
    node_id: str
    cancel_requested: threading.Event = field(default_factory=threading.Event)
    idempotency_key: str | None = None
    run_key: str | None = None


_CURRENT: contextvars.ContextVar[WorkContext | None] = contextvars.ContextVar("jamjet_work_context", default=None)


def current_work() -> WorkContext:
    """The context of the work item being executed, or a RuntimeError outside one."""
    ctx = _CURRENT.get()
    if ctx is None:
        raise RuntimeError("no work item is being executed in this context")
    return ctx


def set_current_work(ctx: WorkContext) -> contextvars.Token[WorkContext | None]:
    return _CURRENT.set(ctx)


def reset_current_work(token: contextvars.Token[WorkContext | None]) -> None:
    _CURRENT.reset(token)
