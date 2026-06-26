"""
Agent — the simplest way to create a JamJet agent.

Compiles to the chosen reasoning strategy under the hood, giving you full
observability, durability, and tool-use without any boilerplate.

Default strategy is ``plan-and-execute`` (§14.3): generates a structured plan
first, then executes each step in sequence. Use ``strategy="react"`` for
tight tool-loop tasks, or ``strategy="critic"`` for quality-sensitive output.

Usage::

    from jamjet import Agent, tool

    @tool
    async def web_search(query: str) -> str:
        return f"Search results for: {query}"

    agent = Agent(
        "researcher",
        model="claude-sonnet-4-6",
        tools=[web_search],
        instructions="You are a research assistant.",
    )

    result = await agent.run("Summarize the latest trends in agent runtimes")
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator, Callable
from typing import TYPE_CHECKING, Any

from jamjet.compiler.strategies import StrategyLimits
from jamjet.runtime.local import LocalRuntime
from jamjet.tools.decorators import ToolDefinition

if TYPE_CHECKING:
    from jamjet.spec import AgentSpec


class Agent:
    """
    A JamJet agent — tools + model + instructions + strategy → run.

    Default strategy is ``plan-and-execute``: generates a plan first, then
    executes each step. Override with ``strategy="react"`` for tool-heavy
    loops or ``strategy="critic"`` for draft-and-refine quality tasks.

    For full graph control, use :class:`jamjet.Workflow` directly.
    """

    def __init__(
        self,
        name: str,
        *,
        model: str,
        tools: list[Callable[..., Any]],
        instructions: str = "",
        strategy: str = "plan-and-execute",
        max_iterations: int = 10,
        max_cost_usd: float = 1.0,
        timeout_seconds: int = 300,
        on_limit_exceeded: Callable[[str | None, str, Any, Any], str | None] | None = None,
    ) -> None:
        self.name = name
        self.model = model
        self.instructions = instructions
        self.strategy = strategy
        self._on_limit_exceeded = on_limit_exceeded
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

    def compile(self) -> AgentSpec:
        """Compile this agent to an AgentSpec."""
        from jamjet.model.types import api_key_env_for, parse_model_ref, provider_literal_for  # noqa: PLC0415
        from jamjet.spec import AgentSpec, AgentStrategy, LLMConfig, ToolSpec  # noqa: PLC0415

        ref = parse_model_ref(self.model)
        return AgentSpec(
            name=self.name,
            instructions=self.instructions,
            llm=LLMConfig(
                provider=provider_literal_for(self.model),
                model=ref.litellm_model,
                api_key_env=api_key_env_for(ref.provider),
            ),
            tools=[
                ToolSpec(
                    name=td.name,
                    description=td.description,
                    input_schema=td.input_schema,
                    handler_ref=f"{td.fn.__module__}:{td.fn.__qualname__}",
                )
                for td in self._tools
            ],
            strategy=AgentStrategy(name=self.strategy),
            limits={
                "max_iterations": self.limits.max_iterations,
                "max_cost_usd": self.limits.max_cost_usd,
                "timeout_seconds": self.limits.timeout_seconds,
            },
        )

    # ── Public run interface ───────────────────────────────────────────────

    async def run(self, prompt: str) -> AgentResult:
        """
        Run the agent on a single prompt via LocalRuntime.

        Compiles to AgentSpec, hands off to LocalRuntime which dispatches to
        the appropriate strategy runner.
        """
        spec = self.compile()
        rt = LocalRuntime()
        result = await rt.execute(spec, prompt)
        return AgentResult(
            output=result.output,
            tool_calls=[tc.model_dump() for tc in result.tool_calls],
            ir=spec.model_dump(),
            duration_us=result.duration_ms * 1000,
        )

    def run_sync(self, prompt: str) -> AgentResult:
        """Synchronous wrapper around :meth:`run` for scripts and notebooks."""
        return asyncio.run(self.run(prompt))

    async def stream(
        self,
        prompt: str,
        *,
        model: Any | None = None,
    ) -> AsyncIterator[Any]:
        """Stream token deltas for a single turn.

        Streaming is a view; durability records the completed turn (resume
        returns the checkpoint, it does not re-stream). The looped, durable
        stream over the full agent run lands in Track 2.
        """
        from jamjet.model.defaults import default_model_middleware  # noqa: PLC0415
        from jamjet.model.seam import Model  # noqa: PLC0415
        from jamjet.model.types import ModelRequest, parse_model_ref  # noqa: PLC0415

        spec = self.compile()
        seam = model or Model(middleware=default_model_middleware())
        request = ModelRequest(
            ref=parse_model_ref(spec.llm.model),
            messages=[
                {"role": "system", "content": self.instructions or "You are a helpful assistant."},
                {"role": "user", "content": prompt},
            ],
            temperature=spec.llm.temperature,
            max_tokens=spec.llm.max_tokens,
            stream=True,
        )
        async for chunk in seam.stream(request):
            yield chunk

    def __repr__(self) -> str:
        return f"Agent(name={self.name!r}, model={self.model!r}, tools={self.tool_names}, strategy={self.strategy!r})"


class AgentResult:
    """Result returned by Agent.run()."""

    def __init__(
        self,
        output: str,
        tool_calls: list[dict[str, Any]],
        ir: Any,
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
