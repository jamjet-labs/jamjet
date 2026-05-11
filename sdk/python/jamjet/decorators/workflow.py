"""@workflow function decorator — free-function alternative to @DurableAgent."""

from __future__ import annotations

import functools
from collections.abc import Callable
from typing import Any, TypeVar

from jamjet.spec import NodeSpec, WorkflowSpec

F = TypeVar("F", bound=Callable[..., Any])


def workflow(fn: F) -> F:
    spec = WorkflowSpec(
        name=fn.__name__,
        nodes=[NodeSpec(id=fn.__name__, handler_ref=f"{fn.__module__}:{fn.__qualname__}")],
        edges=[],
        entry_node=fn.__name__,
    )

    @functools.wraps(fn)
    async def wrapped(*args: Any, **kwargs: Any) -> Any:
        return await fn(*args, **kwargs)

    wrapped.__jamjet_spec__ = spec  # type: ignore[attr-defined]
    return wrapped  # type: ignore[return-value]
