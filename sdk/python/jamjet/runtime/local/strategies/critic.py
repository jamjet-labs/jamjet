"""Critic strategy: draft -> evaluate -> revise loop."""
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

    draft = ""
    for round_i in range(max_iter):
        if round_i == 0:
            draft_prompt = prompt
        else:
            draft_prompt = (
                f"Original goal: {prompt}\n\n"
                f"Previous draft:\n{draft}\n\n"
                "Revise the draft to address the critic's feedback above. "
                "Return only the improved draft."
            )
        draft_messages: list[Any] = [
            {"role": "system", "content": system},
            {"role": "user", "content": draft_prompt},
        ]
        for _ in range(max_iter):
            msg = await adapter.generate(draft_messages, tools=tools or None)
            if not getattr(msg, "tool_calls", None):
                draft = msg.content or ""
                break
            draft_messages.append(msg)
            draft_messages.extend(await execute_tool_calls(msg, tool_map, tool_calls_log))

        critic_messages: list[Any] = [
            {
                "role": "system",
                "content": (
                    "You are a strict critic. Evaluate the draft against the goal. "
                    "Reply with either PASS (if the draft is good enough) or "
                    "REVISE: <specific feedback>."
                ),
            },
            {"role": "user", "content": f"Goal: {prompt}\n\nDraft:\n{draft}"},
        ]
        critic_msg = await adapter.generate(critic_messages)
        verdict = (critic_msg.content or "").strip()
        if verdict.upper().startswith("PASS"):
            break
        draft = f"{draft}\n\n[Critic feedback]: {verdict}"

    return draft
