"""LiteLLM-backed model calls. THE ONLY module on the hot path that imports a
provider engine. Keep it that way: the seam is bypassable if this leaks.
"""

from __future__ import annotations

from collections.abc import AsyncIterator
from typing import Any

from jamjet.model.types import ModelRequest, ModelResponse, StreamChunk

_INSTALL_HINT = "litellm is required for the model seam. Install with: pip install 'jamjet[model]'"


def _import_litellm() -> Any:
    try:
        import litellm
    except ImportError as exc:  # pragma: no cover - exercised via sys.modules patch
        raise ImportError(_INSTALL_HINT) from exc
    if litellm is None:
        raise ImportError(_INSTALL_HINT)
    return litellm


def _base_kwargs(request: ModelRequest) -> dict[str, Any]:
    kwargs: dict[str, Any] = {"model": request.ref.litellm_model, "messages": request.messages}
    if request.temperature is not None:
        kwargs["temperature"] = request.temperature
    if request.max_tokens is not None:
        kwargs["max_tokens"] = request.max_tokens
    return kwargs


class LiteLLMBackend:
    async def complete(self, request: ModelRequest) -> ModelResponse:
        litellm = _import_litellm()
        kwargs = _base_kwargs(request)
        if request.tools:
            kwargs["tools"] = request.tools
        resp = await litellm.acompletion(**kwargs)
        usage = getattr(resp, "usage", None)
        try:
            cost = float(litellm.completion_cost(completion_response=resp))
        except Exception:
            cost = 0.0
        return ModelResponse(
            message=resp.choices[0].message,
            input_tokens=getattr(usage, "prompt_tokens", 0) if usage else 0,
            output_tokens=getattr(usage, "completion_tokens", 0) if usage else 0,
            cost_usd=cost,
            raw=resp,
        )

    async def stream(self, request: ModelRequest) -> AsyncIterator[StreamChunk]:
        litellm = _import_litellm()
        kwargs = _base_kwargs(request)
        kwargs["stream"] = True
        response_stream = await litellm.acompletion(**kwargs)
        async for part in response_stream:
            delta = part.choices[0].delta
            text = getattr(delta, "content", None) or ""
            yield StreamChunk(delta=text, raw=part)
