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

        Compiles the agent to IR, then executes the tool loop locally —
        each tool is called with the prompt and results are collected.
        For production, submit the compiled IR to the JamJet runtime.
        """
        t_start = time.perf_counter_ns()
        ir = self.compile()

        messages: list[dict[str, str]] = []
        if self.instructions:
            messages.append({"role": "system", "content": self.instructions})
        messages.append({"role": "user", "content": prompt})

        tool_calls: list[dict[str, Any]] = []
        final_output: str = ""

        for _iteration in range(self.limits.max_iterations):
            tool_outputs: list[dict[str, Any]] = []
            for td in self._tools:
                t_call = time.perf_counter_ns()
                # Call the tool — support both keyword and positional styles
                sig = inspect.signature(td.fn)
                params = list(sig.parameters.keys())
                if params:
                    result = td.fn(**{params[0]: prompt})
                else:
                    result = td.fn()
                if inspect.isawaitable(result):
                    result = await result
                duration_us = (time.perf_counter_ns() - t_call) / 1000
                output_str = str(result)
                tool_outputs.append({"tool": td.name, "output": output_str})
                tool_calls.append(
                    {
                        "tool": td.name,
                        "input": prompt,
                        "output": output_str,
                        "duration_us": duration_us,
                    }
                )

            parts = [to["output"] for to in tool_outputs]
            final_output = "\n\n".join(parts)
            break  # single-pass for local execution without a live model

        total_us = (time.perf_counter_ns() - t_start) / 1000
        return AgentResult(
            output=final_output,
            tool_calls=tool_calls,
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
