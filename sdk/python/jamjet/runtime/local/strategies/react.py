"""React strategy executor -- Reason + Act loop."""

from __future__ import annotations

from typing import Any

from jamjet.runtime.local.llm_adapters.base import LLMAdapter
from jamjet.runtime.local.strategies.base import (
    _last_assistant_content,
    execute_tool_calls,
    resolve_tool_map,
)
from jamjet.spec import AgentSpec


async def run(
    *,
    adapter: LLMAdapter,
    spec: AgentSpec,
    prompt: str,
    tools: list[dict[str, Any]],
    tool_calls_log: list[dict[str, Any]],
) -> str:
    tool_map = resolve_tool_map(spec.tools)
    max_iter = spec.limits.get("max_iterations", 10)

    messages: list[Any] = []
    if spec.instructions:
        messages.append({"role": "system", "content": spec.instructions})
    messages.append({"role": "user", "content": prompt})

    for _ in range(max_iter):
        msg = await adapter.generate(messages, tools=tools or None)
        if not getattr(msg, "tool_calls", None):
            return msg.content or ""
        messages.append(msg)
        messages.extend(await execute_tool_calls(msg, tool_map, tool_calls_log))

    return _last_assistant_content(messages)
