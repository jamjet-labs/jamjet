"""
jamjet.crewai — durable_run() shim for CrewAI crews.

Usage:
    from crewai import Crew
    from jamjet.crewai import durable_run

    crew = Crew(...)
    with durable_run(crew):
        crew.kickoff()
"""
from __future__ import annotations

import uuid
from contextlib import contextmanager
from typing import Any, Iterator

from jamjet.durable.context import durable_run as _durable_run


@contextmanager
def durable_run(crew: Any) -> Iterator[str]:
    """
    Set jamjet's execution context for the duration of a CrewAI kickoff.

    Reads `crew.id` if present and non-None; otherwise generates one.
    """
    eid = getattr(crew, "id", None) or f"crewai-{uuid.uuid4()}"
    if not isinstance(eid, str):
        eid = str(eid)
    with _durable_run(eid) as resolved:
        yield resolved
