"""@task decorator — marks a method as a workflow step (checkpoint boundary)."""

from __future__ import annotations

from collections.abc import Callable
from typing import Any, TypeVar, overload

F = TypeVar("F", bound=Callable[..., Any])


@overload
def task(fn: F) -> F: ...
@overload
def task(*, entry: bool = False, retry: int = 0, timeout_s: int | None = None) -> Callable[[F], F]: ...


def task(
    fn: Callable[..., Any] | None = None,
    *,
    entry: bool = False,
    retry: int = 0,
    timeout_s: int | None = None,
) -> Any:
    def _wrap(f: F) -> F:
        meta = {"is_step": True, "is_entrypoint": entry, "retry": retry, "timeout_s": timeout_s}
        f.__jamjet_task__ = meta  # type: ignore[attr-defined]
        return f

    if fn is not None and callable(fn):
        return _wrap(fn)
    return _wrap
