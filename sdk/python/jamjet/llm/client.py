"""Shared LLM client — routed through the JamJet Model seam."""

from __future__ import annotations

from dataclasses import dataclass

from jamjet.model import (
    Model,
    ModelRequest,
    default_model_middleware,
    parse_model_ref,
)


@dataclass
class LlmResponse:
    text: str
    input_tokens: int = 0
    output_tokens: int = 0
    gen_ai_model: str = ""
    finish_reason: str | None = None


async def call_llm(model: str, prompt: str, max_tokens: int = 512) -> LlmResponse:
    """Call an LLM through the JamJet Model seam."""
    request = ModelRequest(
        ref=parse_model_ref(model),
        messages=[{"role": "user", "content": prompt}],
        max_tokens=max_tokens,
    )
    resp = await Model(middleware=default_model_middleware()).complete(request)
    return LlmResponse(
        text=resp.message.content or "",
        input_tokens=resp.input_tokens,
        output_tokens=resp.output_tokens,
    )
