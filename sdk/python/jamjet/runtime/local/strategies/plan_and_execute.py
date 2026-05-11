"""Plan-and-execute strategy: numbered plan -> per-step ReAct -> synthesis."""

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

    plan_messages: list[Any] = [
        {"role": "system", "content": system},
        {
            "role": "user",
            "content": (
                f"Goal: {prompt}\n\n"
                "Before executing, write a concise numbered plan (3-5 steps) "
                "that you will follow to complete this goal. "
                "Return only the numbered list, nothing else."
            ),
        },
    ]
    plan_msg = await adapter.generate(plan_messages)
    plan_text = plan_msg.content or ""

    steps = [line.strip() for line in plan_text.splitlines() if line.strip() and line.strip()[0].isdigit()]
    if not steps:
        steps = [plan_text]

    step_results: list[str] = []
    for step in steps[:max_iter]:
        step_messages: list[Any] = [
            {"role": "system", "content": system},
            {
                "role": "user",
                "content": (
                    f"Overall goal: {prompt}\n\n"
                    f"Execute this step: {step}\n\n"
                    "Use any available tools as needed. "
                    "Return the result of this step only."
                ),
            },
        ]
        for _ in range(max_iter):
            msg = await adapter.generate(step_messages, tools=tools or None)
            if not getattr(msg, "tool_calls", None):
                step_results.append(msg.content or "")
                break
            step_messages.append(msg)
            step_messages.extend(await execute_tool_calls(msg, tool_map, tool_calls_log))
        else:
            step_results.append("")

    synthesis_messages: list[Any] = [
        {"role": "system", "content": system},
        {
            "role": "user",
            "content": (
                f"Goal: {prompt}\n\n"
                f"Plan executed:\n{plan_text}\n\n"
                "Step results:\n"
                + "\n\n".join(f"Step {i + 1}: {r}" for i, r in enumerate(step_results))
                + "\n\nSynthesize these results into a final, well-structured answer."
            ),
        },
    ]
    final_msg = await adapter.generate(synthesis_messages)
    return final_msg.content or ""
