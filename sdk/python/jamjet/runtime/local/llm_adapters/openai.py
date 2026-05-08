"""OpenAI LLM adapter. Wraps AsyncOpenAI for chat.completions.create."""
from __future__ import annotations

import os
from typing import Any

from jamjet.spec import LLMConfig


class OpenAIAdapter:
    def __init__(self, config: LLMConfig) -> None:
        self.config = config

    async def generate(
        self,
        messages: list[dict[str, Any]],
        *,
        tools: list[dict[str, Any]] | None = None,
    ) -> Any:
        try:
            from openai import AsyncOpenAI
        except ImportError as exc:
            raise ImportError(
                "openai package required for OpenAI provider. Install: pip install openai"
            ) from exc

        client = AsyncOpenAI(
            api_key=os.environ.get(self.config.api_key_env, ""),
            base_url=self.config.base_url,
        )
        kwargs: dict[str, Any] = {"model": self.config.model, "messages": messages}
        if tools:
            kwargs["tools"] = tools
        if self.config.temperature is not None:
            kwargs["temperature"] = self.config.temperature
        if self.config.max_tokens is not None:
            kwargs["max_tokens"] = self.config.max_tokens
        response = await client.chat.completions.create(**kwargs)
        return response.choices[0].message
