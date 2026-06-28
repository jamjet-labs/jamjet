"""The model-call middleware chain: the seam's enforcement point.

Track 1 ships the protocol, a no-op base, and the allowlist. Budget-cap and
PII-redaction middleware land in Track 3 against this same protocol.
"""

from __future__ import annotations

from typing import Protocol, runtime_checkable

from jamjet.model.types import ModelRequest, ModelResponse


class ModelDeniedError(Exception):
    """Raised by a ``before`` hook to deny a model call before it reaches a provider."""

    def __init__(self, reason: str, *, code: str = "denied") -> None:
        super().__init__(reason)
        self.reason = reason
        self.code = code


class BudgetExceededError(ModelDeniedError):
    """Raised by ``BudgetMiddleware`` when accumulated spend has reached the budget.

    Fail-closed: the call is denied *before* it reaches the provider.
    Both the limit and the consumed amount are named in the message so callers
    can surface the exact figures to the user or an audit log.
    """

    def __init__(
        self,
        *,
        limit_usd: float | None,
        limit_tokens: int | None,
        consumed_usd: float,
        consumed_tokens: int,
    ) -> None:
        parts: list[str] = []
        if limit_usd is not None:
            parts.append(f"cost ${consumed_usd:.6f} >= limit ${limit_usd:.6f}")
        if limit_tokens is not None:
            parts.append(f"tokens {consumed_tokens} >= limit {limit_tokens}")
        reason = "budget exceeded: " + "; ".join(parts) if parts else "budget exceeded"
        super().__init__(reason, code="budget_exceeded")
        self.limit_usd = limit_usd
        self.limit_tokens = limit_tokens
        self.consumed_usd = consumed_usd
        self.consumed_tokens = consumed_tokens


@runtime_checkable
class ModelMiddleware(Protocol):
    async def before(self, request: ModelRequest) -> ModelRequest: ...
    async def after(self, request: ModelRequest, response: ModelResponse) -> ModelResponse: ...


class BaseModelMiddleware:
    """No-op base so middleware override only the hook they need."""

    async def before(self, request: ModelRequest) -> ModelRequest:
        return request

    async def after(self, request: ModelRequest, response: ModelResponse) -> ModelResponse:
        return response


class ModelAllowlistMiddleware(BaseModelMiddleware):
    """Deny model calls whose provider or full model string is not allowed.

    ``allowed=None`` allows everything (the Track 1 default; Track 3 wires the
    real policy-derived allowlist).
    """

    def __init__(self, allowed: set[str] | None) -> None:
        self._allowed = allowed

    async def before(self, request: ModelRequest) -> ModelRequest:
        if self._allowed is None:
            return request
        ref = request.ref
        if ref.provider in self._allowed or ref.litellm_model in self._allowed:
            return request
        raise ModelDeniedError(
            f"model {ref.litellm_model!r} is not in the allowlist",
            code="model_not_allowed",
        )
