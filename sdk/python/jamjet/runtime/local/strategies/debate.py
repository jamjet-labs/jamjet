"""Debate strategy: propose -> counter -> judge."""

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
    initial_messages: list[dict[str, Any]] | None = None,
) -> str:
    tool_map = resolve_tool_map(spec.tools)
    max_iter = spec.limits.get("max_iterations", 10)
    system = spec.instructions or "You are a helpful assistant."

    proposal = ""
    prop_msgs: list[Any] = [
        {"role": "system", "content": f"{system}\nYou are the proposer. Give your best answer."},
        {"role": "user", "content": prompt},
    ]
    for _ in range(max_iter):
        msg = await adapter.generate(prop_msgs, tools=tools or None)
        if not getattr(msg, "tool_calls", None):
            proposal = msg.content or ""
            break
        prop_msgs.append(msg)
        prop_msgs.extend(await execute_tool_calls(msg, tool_map, tool_calls_log))

    counter = proposal
    for _ in range(min(2, max_iter)):
        counter_msgs: list[Any] = [
            {
                "role": "system",
                "content": (
                    "You are a devil's advocate. Challenge the answer below. "
                    "Point out flaws, missing perspectives, or errors. Be constructive."
                ),
            },
            {"role": "user", "content": f"Task: {prompt}\n\nProposed answer:\n{counter}"},
        ]
        counter_msg = await adapter.generate(counter_msgs)
        critique = counter_msg.content or ""

        revise_msgs: list[Any] = [
            {
                "role": "system",
                "content": f"{system}\nRevise your answer addressing the critique.",
            },
            {
                "role": "user",
                "content": f"Task: {prompt}\n\nYour answer:\n{counter}\n\nCritique:\n{critique}",
            },
        ]
        for _ in range(max_iter):
            msg = await adapter.generate(revise_msgs, tools=tools or None)
            if not getattr(msg, "tool_calls", None):
                counter = msg.content or ""
                break
            revise_msgs.append(msg)
            revise_msgs.extend(await execute_tool_calls(msg, tool_map, tool_calls_log))

    judge_msgs: list[Any] = [
        {
            "role": "system",
            "content": "You are the judge. Synthesize the best final answer from the debate.",
        },
        {
            "role": "user",
            "content": f"Task: {prompt}\n\nFinal proposal after debate:\n{counter}",
        },
    ]
    judge_msg = await adapter.generate(judge_msgs)
    return judge_msg.content or counter
