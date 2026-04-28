"""
jamjet.durable — exactly-once tool execution across agent frameworks.

Wrap any side-effecting tool with @durable; the result is cached against
an idempotency key so that repeated invocations within the same execution
context return the cached value instead of re-running the side effect.

    from jamjet.durable import durable, durable_run

    @durable
    def charge_card(amount: float) -> dict:
        return stripe.charges.create(amount=amount)

    with durable_run("agent-run-123"):
        # If the agent crashes and restarts, re-calling charge_card
        # within the same run will return the cached charge instead
        # of charging the card again.
        result = charge_card(847.0)
"""

from jamjet.durable.cache import Cache, SqliteCache
from jamjet.durable.context import (
    durable_run,
    get_execution_context,
    set_execution_context,
)
from jamjet.durable.decorator import durable

__all__ = [
    "durable",
    "durable_run",
    "set_execution_context",
    "get_execution_context",
    "Cache",
    "SqliteCache",
]
