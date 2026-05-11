"""LLM adapter protocol. One adapter per provider; OpenAI is the only complete one this phase."""

from __future__ import annotations

from typing import Any, Protocol, runtime_checkable

from jamjet.spec import LLMConfig


@runtime_checkable
class LLMAdapter(Protocol):
    config: LLMConfig

    async def generate(
        self,
        messages: list[dict[str, Any]],
        *,
        tools: list[dict[str, Any]] | None = None,
    ) -> Any: ...
