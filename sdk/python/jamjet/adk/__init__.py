"""
jamjet.adk — durable_run() shim for Google Agent Development Kit.

Usage:
    from google.adk.agents import Agent
    from jamjet.adk import durable_run

    agent = Agent(...)
    with durable_run(agent):
        agent.run("book a flight")
"""
from __future__ import annotations

import uuid
from contextlib import contextmanager
from typing import Any, Iterator

from jamjet.durable.context import durable_run as _durable_run


@contextmanager
def durable_run(agent: Any) -> Iterator[str]:
    """Set jamjet's execution context for a Google ADK agent run."""
    eid = getattr(agent, "session_id", None) or f"adk-{uuid.uuid4()}"
    if not isinstance(eid, str):
        eid = str(eid)
    with _durable_run(eid) as resolved:
        yield resolved
