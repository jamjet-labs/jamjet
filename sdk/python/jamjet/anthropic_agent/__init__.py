"""
jamjet.anthropic_agent — durable_run() shim for the Anthropic Agent SDK
(Claude Agent SDK).

Note: package is named `anthropic_agent` (not `anthropic`) to avoid namespace
confusion with the top-level `anthropic` Python SDK package.

Usage:
    from anthropic import Anthropic
    from jamjet.anthropic_agent import durable_run

    client = Anthropic()
    run = client.beta.messages.runs.create(...)
    with durable_run(run):
        # tools called within this run share an idempotency namespace
        ...
"""
from __future__ import annotations

import uuid
from contextlib import contextmanager
from typing import Any, Iterator

from jamjet.durable.context import durable_run as _durable_run


@contextmanager
def durable_run(run: Any) -> Iterator[str]:
    """Set jamjet's execution context from an Anthropic Agent SDK run handle."""
    eid = (
        getattr(run, "run_id", None)
        or getattr(run, "id", None)
        or f"anthropic-{uuid.uuid4()}"
    )
    if not isinstance(eid, str):
        eid = str(eid)
    with _durable_run(eid) as resolved:
        yield resolved
