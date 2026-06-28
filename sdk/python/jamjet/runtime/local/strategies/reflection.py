"""Reflection strategy: execute -> self-reflect -> revise loop."""

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

    output = ""
    reflection = ""
    for round_i in range(max_iter):
        if round_i == 0:
            exec_prompt = prompt
        else:
            exec_prompt = (
                f"Original task: {prompt}\n\n"
                f"Your previous answer:\n{output}\n\n"
                f"Your self-reflection:\n{reflection}\n\n"
                "Revise your answer based on the reflection. Return only the improved answer."
            )
        exec_msgs: list[Any] = [
            {"role": "system", "content": system},
            {"role": "user", "content": exec_prompt},
        ]
        for _ in range(max_iter):
            msg = await adapter.generate(exec_msgs, tools=tools or None)
            if not getattr(msg, "tool_calls", None):
                output = msg.content or ""
                break
            exec_msgs.append(msg)
            exec_msgs.extend(await execute_tool_calls(msg, tool_map, tool_calls_log))

        reflect_msgs: list[Any] = [
            {
                "role": "system",
                "content": (
                    "You are a careful self-evaluator. Reflect on the answer below. "
                    "Identify any errors, gaps, or improvements. "
                    "Reply SATISFIED if the answer is good, or describe specific issues."
                ),
            },
            {"role": "user", "content": f"Task: {prompt}\n\nAnswer:\n{output}"},
        ]
        reflect_msg = await adapter.generate(reflect_msgs)
        reflection = (reflect_msg.content or "").strip()
        if "SATISFIED" in reflection.upper():
            break

    return output
