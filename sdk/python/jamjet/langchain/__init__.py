"""
jamjet.langchain — durable_run() shim for LangChain agent executors.

Bridges LangChain's run identity to JamJet's execution context, so that
@durable-wrapped tools share an idempotency namespace with the LangChain
run that called them.

Usage:
    from langchain.agents import AgentExecutor
    from jamjet.langchain import durable_run
    from jamjet import durable

    @durable
    def charge_card(amount): ...

    executor = AgentExecutor(...)
    with durable_run(executor):
        executor.invoke({"input": "book a flight"})
"""
from __future__ import annotations

import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from typing import Any

from jamjet.durable.context import durable_run as _durable_run


@contextmanager
def durable_run(executor: Any) -> Iterator[str]:
    """
    Set jamjet's execution context for the duration of a LangChain agent run.

    Reads `executor.run_id` if present and non-None; otherwise generates one.
    The yielded value is the execution_id in use.
    """
    eid = getattr(executor, "run_id", None) or f"langchain-{uuid.uuid4()}"
    if not isinstance(eid, str):
        eid = str(eid)
    with _durable_run(eid) as resolved:
        yield resolved
