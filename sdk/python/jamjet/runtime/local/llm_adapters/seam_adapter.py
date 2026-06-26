"""LLMAdapter over the governed Model seam. Drop-in for strategy runners.

Strategy runners call ``adapter.generate(messages, tools=...)`` and read
``msg.content`` / ``msg.tool_calls``. We return the seam's OpenAI-shaped message
unchanged so those runners need no edits.
"""

from __future__ import annotations

from typing import Any

from jamjet.model.metering import MeteringMiddleware
from jamjet.model.middleware import ModelAllowlistMiddleware
from jamjet.model.seam import Model
from jamjet.model.types import ModelRequest, parse_model_ref
from jamjet.spec import LLMConfig


class SeamAdapter:
    config: LLMConfig

    def __init__(self, config: LLMConfig, *, model: Model | None = None) -> None:
        self.config = config
        self._ref = parse_model_ref(config.model)
        # Track 1 default chain: allow-all + metering. Track 3 derives the
        # allowlist (and adds budget + PII) from the agent's policy.
        self._model = model or Model(
            middleware=[ModelAllowlistMiddleware(None), MeteringMiddleware()]
        )

    async def generate(
        self,
        messages: list[dict[str, Any]],
        *,
        tools: list[dict[str, Any]] | None = None,
    ) -> Any:
        request = ModelRequest(
            ref=self._ref,
            messages=messages,
            tools=tools,
            temperature=self.config.temperature,
            max_tokens=self.config.max_tokens,
        )
        response = await self._model.complete(request)
        return response.message
