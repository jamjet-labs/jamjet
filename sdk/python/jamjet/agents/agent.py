"""
Agent — the simplest way to create a JamJet agent.

Compiles to a ReAct workflow under the hood, giving you full observability,
durability, and tool-use without any boilerplate.

Usage::

    from jamjet import Agent, tool

    @tool
    async def web_search(query: str) -> str:
        return f"Search results for: {query}"

    agent = Agent(
        "researcher",
        model="gpt-5.2",
        tools=[web_search],
        instructions="You are a research assistant.",
    )

    result = await agent.run("Summarize the latest trends in agent runtimes")
"""

from __future__ import annotations

import asyncio
import inspect
import time
from collections.abc import Callable
from typing import Any

from jamjet.compiler.strategies import StrategyLimits, compile_strategy
from jamjet.tools.decorators import ToolDefinition


class Agent:
    """
    A minimal JamJet agent — tools + model + instructions → run.

    Under the hood this compiles to a ReAct workflow with sensible defaults.
    For full control, use :class:`jamjet.Workflow` directly.
    """

    def __init__(
        self,
        name: str,
        *,
        model: str,
        tools: list[Callable[..., Any]],
        instructions: str = "",
        strategy: str = "react",
        max_iterations: int = 10,
        max_cost_usd: float = 1.0,
        timeout_seconds: int = 300,
    ) -> None:
        self.name = name
        self.model = model
        self.instructions = instructions
        self.strategy = strategy
        self.limits = StrategyLimits(
            max_iterations=max_iterations,
            max_cost_usd=max_cost_usd,
            timeout_seconds=timeout_seconds,
        )

        # Resolve tool definitions from decorated functions
        self._tools: list[ToolDefinition] = []
        for t in tools:
            defn = getattr(t, "_jamjet_tool", None)
            if defn is None:
                raise TypeError(f"{t!r} is not a @tool-decorated function. Wrap it with @jamjet.tool first.")
            self._tools.append(defn)

    @property
    def tool_names(self) -> list[str]:
        return [t.name for t in self._tools]

    def compile(self) -> dict[str, Any]:
        """Compile this agent to the canonical IR."""
        return compile_strategy(
            strategy_name=self.strategy,
            strategy_config={"goal_template": self.instructions},
            tools=self.tool_names,
            model=self.model,
            limits=self.limits,
            goal=self.instructions,
            agent_id=self.name,
        )

    async def run(self, prompt: str) -> AgentResult:
        """
        Run the agent on a single prompt in-process. No runtime server needed.

        Executes a ReAct loop: call the model → execute any tool calls →
        feed results back → repeat until the model stops calling tools or
        max_iterations is reached.
        """
        import json
        import os

        try:
            from openai import AsyncOpenAI
        except ImportError as exc:
            raise ImportError(
                "openai package is required for Agent.run(). Run: pip install openai"
            ) from exc

        t_start = time.perf_counter_ns()
        ir = self.compile()

        client = AsyncOpenAI(
            api_key=os.environ.get("OPENAI_API_KEY", ""),
            base_url=os.environ.get("OPENAI_BASE_URL"),
        )

        # Build OpenAI-compatible tool schemas
        openai_tools = [
            {
                "type": "function",
                "function": {
                    "name": td.name,
                    "description": td.description,
                    "parameters": td.input_schema,
                },
            }
            for td in self._tools
        ]

        messages: list[dict[str, Any]] = []
        if self.instructions:
            messages.append({"role": "system", "content": self.instructions})
        messages.append({"role": "user", "content": prompt})

        tool_calls_log: list[dict[str, Any]] = []
        final_output: str = ""
        tool_map = {td.name: td for td in self._tools}

        for _iteration in range(self.limits.max_iterations):
            kwargs: dict[str, Any] = {"model": self.model, "messages": messages}
            if openai_tools:
                kwargs["tools"] = openai_tools

            response = await client.chat.completions.create(**kwargs)
            msg = response.choices[0].message

            # No tool calls — model is done
            if not msg.tool_calls:
                final_output = msg.content or ""
                break

            # Execute each tool call and feed results back
            messages.append(msg)
            for tc in msg.tool_calls:
                td = tool_map.get(tc.function.name)
                if td is None:
                    result_str = f"Error: unknown tool {tc.function.name!r}"
                else:
                    t_call = time.perf_counter_ns()
                    args = json.loads(tc.function.arguments or "{}")
                    result = td.fn(**args)
                    if inspect.isawaitable(result):
                        result = await result
                    duration_us = (time.perf_counter_ns() - t_call) / 1000
                    result_str = str(result)
                    tool_calls_log.append({
                        "tool": td.name,
                        "input": args,
                        "output": result_str,
                        "duration_us": duration_us,
                    })
                messages.append({
                    "role": "tool",
                    "tool_call_id": tc.id,
                    "content": result_str,
                })
        else:
            # Hit max_iterations — return last assistant content
            for m in reversed(messages):
                content = m.content if hasattr(m, "content") else m.get("content")
                role = m.role if hasattr(m, "role") else m.get("role")
                if role == "assistant" and content:
                    final_output = content
                    break

        total_us = (time.perf_counter_ns() - t_start) / 1000
        return AgentResult(
            output=final_output,
            tool_calls=tool_calls_log,
            ir=ir,
            duration_us=total_us,
        )

    def run_sync(self, prompt: str) -> AgentResult:
        """Synchronous wrapper around :meth:`run` for scripts and notebooks."""
        return asyncio.run(self.run(prompt))

    def __repr__(self) -> str:
        return f"Agent(name={self.name!r}, model={self.model!r}, tools={self.tool_names}, strategy={self.strategy!r})"


class AgentResult:
    """Result returned by Agent.run()."""

    def __init__(
        self,
        output: str,
        tool_calls: list[dict[str, Any]],
        ir: dict[str, Any],
        duration_us: float = 0.0,
    ) -> None:
        self.output = output
        self.tool_calls = tool_calls
        self.ir = ir
        self.duration_us = duration_us

    def __str__(self) -> str:
        return self.output

    def __repr__(self) -> str:
        return f"AgentResult(output={self.output!r}, tool_calls={len(self.tool_calls)})"
