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
