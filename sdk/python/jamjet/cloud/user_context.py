"""Session and end-user attribution for spans.

These fields are NEVER auto-sniffed. The user (i.e. the developer using the
SDK) sets them explicitly via ``configure(...)`` or per-call ``set_user_context``.

Privacy posture:
- ``session_id``, ``end_user_id`` — opaque, safe to store and aggregate.
- ``environment``, ``release_version`` — server-config, no privacy concern.
- ``end_user_email`` — PII. Stored in a separate ``end_users`` table by the
  cloud, hidden in the dashboard by default. Subject to GDPR right-to-erasure
  via a future ``DELETE /v1/end_users/{id}`` endpoint.
"""

from __future__ import annotations

from contextvars import ContextVar
from dataclasses import dataclass


@dataclass(frozen=True)
class ProcessContext:
    """Per-process defaults, set once at ``configure()``."""

    environment: str | None = None
    release_version: str | None = None


@dataclass(frozen=True)
class UserContext:
    """Per-request session/user attribution, scoped via ContextVar.

    Override per-request with ``set_user_context()`` (a context manager) or
    set directly on the current context for the lifetime of an asyncio task.
    """

    session_id: str | None = None
    end_user_id: str | None = None
    end_user_email: str | None = None  # PII — see module docstring
    tags: tuple[str, ...] = ()


_process_context: ProcessContext = ProcessContext()
_user_var: ContextVar[UserContext | None] = ContextVar("jamjet_user_context", default=None)


def get_process_context() -> ProcessContext:
    return _process_context


def set_process_context(*, environment: str | None = None, release_version: str | None = None) -> None:
    """Called from ``configure()``. Process-wide; overrides only on subsequent
    set_process_context() calls (rare)."""
    global _process_context
    _process_context = ProcessContext(
        environment=environment,
        release_version=release_version,
    )


def get_user_context() -> UserContext | None:
    return _user_var.get()


class user_context:
    """Context manager that scopes session/user attribution to a block.

    Example::

        with jamjet.cloud.user_context(session_id="conv_42", end_user_id="cust_7"):
            client.chat.completions.create(...)
    """

    def __init__(
        self,
        *,
        session_id: str | None = None,
        end_user_id: str | None = None,
        end_user_email: str | None = None,
        tags: list[str] | tuple[str, ...] | None = None,
    ) -> None:
        self._ctx = UserContext(
            session_id=session_id,
            end_user_id=end_user_id,
            end_user_email=end_user_email,
            tags=tuple(tags or ()),
        )
        self._token: object | None = None

    def __enter__(self) -> UserContext:
        self._token = _user_var.set(self._ctx)
        return self._ctx

    def __exit__(self, *exc: object) -> None:
        if self._token is not None:
            _user_var.reset(self._token)  # type: ignore[arg-type]
            self._token = None


def set_user_context(
    *,
    session_id: str | None = None,
    end_user_id: str | None = None,
    end_user_email: str | None = None,
    tags: list[str] | tuple[str, ...] | None = None,
) -> None:
    """Set the user context for the current asyncio task / thread without a
    ``with`` block. Useful at the top of a request handler. Subsequent
    ``user_context(...)`` blocks nest correctly."""
    _user_var.set(
        UserContext(
            session_id=session_id,
            end_user_id=end_user_id,
            end_user_email=end_user_email,
            tags=tuple(tags or ()),
        )
    )
