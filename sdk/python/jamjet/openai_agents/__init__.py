"""
jamjet.openai_agents — durable_run() shim for the OpenAI Agents SDK.

Usage:
    from agents import Runner, Agent
    from jamjet.openai_agents import durable_run

    runner = Runner.run_sync(agent, "book a flight")
    with durable_run(runner):
        # tools called within this run share an idempotency namespace
        ...
"""
from __future__ import annotations

import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from typing import Any

from jamjet.durable.context import durable_run as _durable_run


@contextmanager
def durable_run(runner: Any) -> Iterator[str]:
    """Set jamjet's execution context from an OpenAI Agents SDK runner."""
    eid = (
        getattr(runner, "run_id", None)
        or getattr(runner, "id", None)
        or f"openai-agents-{uuid.uuid4()}"
    )
    if not isinstance(eid, str):
        eid = str(eid)
    with _durable_run(eid) as resolved:
        yield resolved
