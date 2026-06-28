"""Fail-closed budget-enforcement middleware (T3-2).

``BudgetMiddleware`` tracks cumulative cost and tokens across a run (per
instance) and denies the next model call — before it reaches the provider —
when the configured ``Budget`` is already met or exceeded.

Enforcement rule
----------------
Check BEFORE the call: if accumulated spend (tokens and/or cost_usd) is
already >= the configured limit, raise ``BudgetExceededError`` without calling
the backend.  After a successful provider response, accumulate the response's
``cost_usd`` and total tokens (``input_tokens + output_tokens``) for future
checks.

Run-scoped state
----------------
Each ``BudgetMiddleware`` instance carries its own accumulator.  For the
in-process path, ``LocalRuntime`` creates a new ``SeamAdapter`` (and therefore
a new ``Model`` with fresh middleware) per execution, so instance state gives
per-run scoping automatically.  For the sidecar (durable path), the Rust
engine enforces the budget compiled into the IR (T3-5) and is the
authoritative check; the seam-level ``BudgetMiddleware`` on the sidecar
singleton has ``budget=None`` by default and is a no-op pass-through there.

A ``budget`` of ``None`` -> no enforcement; all calls pass through (metering
via ``MeteringMiddleware`` is separate and still records).
"""

from __future__ import annotations

from jamjet.agents.governance import Budget
from jamjet.model.middleware import BaseModelMiddleware, BudgetExceededError
from jamjet.model.types import ModelRequest, ModelResponse


class BudgetMiddleware(BaseModelMiddleware):
    """Per-run budget enforcement at the model seam.

    Parameters
    ----------
    budget:
        The per-run spending cap.  ``None`` disables enforcement (allow-all).
        ``Budget(cost_usd=...)`` enforces cost only.
        ``Budget(tokens=...)`` enforces total tokens only.
        ``Budget(tokens=..., cost_usd=...)`` enforces either — whichever
        cap is hit first triggers denial.
    """

    def __init__(self, budget: Budget | None) -> None:
        self._budget = budget
        self._consumed_tokens: int = 0
        self._consumed_cost_usd: float = 0.0

    # -- Read-only introspection ----------------------------------------------

    @property
    def consumed_tokens(self) -> int:
        """Total tokens accumulated so far (input + output across all calls)."""
        return self._consumed_tokens

    @property
    def consumed_cost_usd(self) -> float:
        """Total cost_usd accumulated so far across all calls."""
        return self._consumed_cost_usd

    # -- Middleware hooks ------------------------------------------------------

    async def before(self, request: ModelRequest) -> ModelRequest:
        """Deny the call if the accumulated budget is already exhausted."""
        if self._budget is None:
            return request

        cost_cap = self._budget.cost_usd
        token_cap = self._budget.tokens

        cost_exceeded = cost_cap is not None and self._consumed_cost_usd >= cost_cap
        token_exceeded = token_cap is not None and self._consumed_tokens >= token_cap

        if cost_exceeded or token_exceeded:
            raise BudgetExceededError(
                limit_usd=cost_cap,
                limit_tokens=token_cap,
                consumed_usd=self._consumed_cost_usd,
                consumed_tokens=self._consumed_tokens,
            )

        return request

    async def after(self, request: ModelRequest, response: ModelResponse) -> ModelResponse:
        """Accumulate the cost and tokens from a completed call."""
        if self._budget is not None:
            self._consumed_tokens += response.input_tokens + response.output_tokens
            self._consumed_cost_usd += response.cost_usd
        return response
