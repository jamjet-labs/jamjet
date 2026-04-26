"""Agent identity for JamJet Cloud.

Multi-agent systems route work between named agents (e.g. ``research-bot`` calls
``writer-bot``). The cloud captures every span tagged with the agent that
produced it, then renders them as nodes in the dashboard's network graph.

The default flow is two lines: ``configure(...)`` seeds an implicit ``default``
agent. Most users never call ``agent()`` explicitly until they spin up a
second agent.

Three idiomatic ways to assign work to a named agent:

    # 1. Implicit (most users): every span belongs to "default".
    jamjet.configure(api_key=..., project="my-app")

    # 2. Per-process default: name set once.
    jamjet.configure(api_key=..., project="my-app", agent="research-bot")

    # 3. Explicit, per-call: scope spans to a specific agent inside a block.
    researcher = jamjet.agent("research-bot", card_uri="https://acme.com/agents/research")
    with researcher:
        client.chat.completions.create(...)        # tagged research-bot

    # 4. Decorator: scope spans inside a function.
    @jamjet.agent("writer-bot")
    def write_post(topic: str) -> str:
        ...

The handle is also a context manager and a decorator, picked up by spans via
a ``contextvars.ContextVar`` so it propagates correctly across asyncio tasks.
"""

from __future__ import annotations

import functools
import threading
from contextvars import ContextVar
from dataclasses import dataclass
from typing import Any, Callable, TypeVar

F = TypeVar("F", bound=Callable[..., Any])


@dataclass(frozen=True)
class Agent:
    """A named agent identity. Equal-by-name within a project.

    The cloud API resolves ``name`` to a UUID on first ingest (idempotent
    INSERT keyed on ``(project_id, name)``). The SDK stays simple — it only
    sends the name; the server holds the canonical id.
    """

    name: str
    card_uri: str | None = None
    description: str | None = None

    def __enter__(self) -> "Agent":
        _push_current(self)
        return self

    def __exit__(self, *exc: object) -> None:
        _pop_current()

    def __call__(self, fn: F) -> F:
        """Decorator form — every call to ``fn`` runs under this agent."""

        @functools.wraps(fn)
        def wrapper(*args: Any, **kwargs: Any) -> Any:
            token = _agent_var.set(self)
            try:
                return fn(*args, **kwargs)
            finally:
                _agent_var.reset(token)

        return wrapper  # type: ignore[return-value]


# ---------------------------------------------------------------------------
# Current-agent context (per asyncio task / per thread)
# ---------------------------------------------------------------------------

_agent_var: ContextVar[Agent | None] = ContextVar("jamjet_agent", default=None)
_default_agent: Agent | None = None
_lock = threading.Lock()


def _push_current(agent: Agent) -> None:
    _agent_var.set(agent)


def _pop_current() -> None:
    # ContextVar reset is per-token; for nested ``with`` scopes the caller
    # would normally hold the token. We use a simpler model: a ``with`` block
    # always restores to whatever default was set at configure() time.
    _agent_var.set(_default_agent)


def get_current_agent() -> Agent | None:
    """Return the current agent for this context, or the configured default."""
    explicit = _agent_var.get()
    if explicit is not None:
        return explicit
    return _default_agent


def set_default_agent(agent: Agent | None) -> None:
    """Used by ``configure()`` to seed the implicit default agent."""
    global _default_agent
    with _lock:
        _default_agent = agent
        # Reset contextvar so subsequent get_current_agent() picks the new default.
        _agent_var.set(None)


# ---------------------------------------------------------------------------
# Public factory — `jamjet.cloud.agent(name="...")`
# ---------------------------------------------------------------------------


def agent(
    name: str,
    *,
    card_uri: str | None = None,
    description: str | None = None,
) -> Agent:
    """Create or retrieve an Agent handle by name.

    The handle is a context manager AND a decorator. Server resolves the
    name to a UUID on first ingest.
    """
    if not name or not name.strip():
        raise ValueError("agent name cannot be empty")
    return Agent(name=name.strip(), card_uri=card_uri, description=description)
