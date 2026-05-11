"""Consensus strategy: independent N responses -> judge picks best."""

from __future__ import annotations

from typing import Any

from jamjet.runtime.local.llm_adapters.base import LLMAdapter
from jamjet.runtime.local.strategies.base import execute_tool_calls, resolve_tool_map
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
    system = spec.instructions or "You are a helpful assistant."
    n_agents = min(3, max_iter)

    responses: list[str] = []
    for i in range(n_agents):
        msgs: list[Any] = [
            {
                "role": "system",
                "content": f"{system}\nYou are agent {i + 1} of {n_agents}. Think independently.",
            },
            {"role": "user", "content": prompt},
        ]
        for _ in range(max_iter):
            msg = await adapter.generate(msgs, tools=tools or None)
            if not getattr(msg, "tool_calls", None):
                responses.append(msg.content or "")
                break
            msgs.append(msg)
            msgs.extend(await execute_tool_calls(msg, tool_map, tool_calls_log))

    candidates = "\n\n".join(f"--- Response {i + 1} ---\n{r}" for i, r in enumerate(responses))
    judge_msgs: list[Any] = [
        {
            "role": "system",
            "content": (
                "You are a judge. Review the candidate responses below and synthesize "
                "the best answer. Take the strongest elements from each."
            ),
        },
        {"role": "user", "content": f"Task: {prompt}\n\n{candidates}"},
    ]
    judge_msg = await adapter.generate(judge_msgs)
    return judge_msg.content or (responses[0] if responses else "")
