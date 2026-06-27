"""Run the durable ReAct agent against a local JamJet engine.

``Agent.run_durable`` compiles the agent to an agent-loop IR (``model -> tools ->
model``, statically unrolled and bounded by ``max_turns``), registers it on the
engine, starts an execution seeded with the system+user messages and the tool
resolver map, polls to a terminal state, and extracts an ``AgentResult`` with the
SAME shape as the in-process ``Agent.run``.

This is the full-stack demonstration. It needs the running services described in
README.md (the engine, the model sidecar, and a python_tool worker). Run it with::

    python main.py
"""

from __future__ import annotations

import asyncio

from weather_agent import build_agent

# The local dev engine (`jamjet dev`) listens on 7700, which is also
# `run_durable`'s default; we pass it explicitly here to make the target clear.
RUNTIME_URL = "http://localhost:7700"
PROMPT = "What's the weather in Paris right now, and what is 19 plus 23?"


async def main() -> None:
    agent = build_agent()
    print(f"> {PROMPT}\n")
    # `max_turns` bounds the static unroll. Size it to the conversation depth you
    # expect: the v1 loop runs the unroll to completion (the per-turn gate's
    # short-circuit on a final answer is a roadmap item), so the answer is the
    # last model turn's output.
    result = await agent.run_durable(PROMPT, max_turns=3, runtime_url=RUNTIME_URL)

    print(f"answer: {result.output}\n")
    for call in result.tool_calls:
        print(f"  tool {call['tool']}({call['input']}) -> {call['output']}")


if __name__ == "__main__":
    asyncio.run(main())
