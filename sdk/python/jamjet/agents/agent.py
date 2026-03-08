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
import inspect
import json
import os
import time
from collections.abc import Callable
from typing import Any

from jamjet.compiler.strategies import StrategyLimits, compile_strategy
from jamjet.tools.decorators import ToolDefinition


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
                raise TypeError(
                    f"{t!r} is not a @tool-decorated function. Wrap it with @jamjet.tool first."
                )
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

    # ── Internal helpers ───────────────────────────────────────────────────

    def _openai_tools(self) -> list[dict[str, Any]]:
        return [
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

    async def _call_model(
        self,
        client: Any,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]] | None = None,
    ) -> Any:
        kwargs: dict[str, Any] = {"model": self.model, "messages": messages}
        if tools:
            kwargs["tools"] = tools
        response = await client.chat.completions.create(**kwargs)
        return response.choices[0].message

    async def _execute_tool_calls(
        self,
        msg: Any,
        tool_map: dict[str, ToolDefinition],
        tool_calls_log: list[dict[str, Any]],
    ) -> list[dict[str, Any]]:
        """Execute all tool calls in a model message, return tool result messages."""
        results = []
        for tc in msg.tool_calls or []:
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
            results.append({
                "role": "tool",
                "tool_call_id": tc.id,
                "content": result_str,
            })
        return results

    # ── Strategy executors ─────────────────────────────────────────────────

    async def _run_plan_and_execute(
        self,
        client: Any,
        prompt: str,
        tool_calls_log: list[dict[str, Any]],
    ) -> str:
        """
        plan-and-execute (§14.3 default):
          1. Ask model to generate a numbered plan.
          2. Execute each step in sequence, giving the model access to tools.
          3. Ask model to synthesize all step results into a final answer.
        """
        openai_tools = self._openai_tools()
        tool_map = {td.name: td for td in self._tools}

        system = self.instructions or "You are a helpful assistant."

        # Step 1 — generate plan
        plan_messages: list[dict[str, Any]] = [
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
        plan_msg = await self._call_model(client, plan_messages)
        plan_text = plan_msg.content or ""

        # Parse steps — each numbered line is one step
        steps = [
            line.strip()
            for line in plan_text.splitlines()
            if line.strip() and line.strip()[0].isdigit()
        ]
        if not steps:
            steps = [plan_text]

        # Step 2 — execute each step
        step_results: list[str] = []
        for step in steps[: self.limits.max_iterations]:
            step_messages: list[dict[str, Any]] = [
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
            # Inner ReAct loop for this step (tool calls until model stops)
            for _ in range(self.limits.max_iterations):
                msg = await self._call_model(client, step_messages, openai_tools or None)
                if not msg.tool_calls:
                    step_results.append(msg.content or "")
                    break
                step_messages.append(msg)
                tool_result_msgs = await self._execute_tool_calls(msg, tool_map, tool_calls_log)
                step_messages.extend(tool_result_msgs)
            else:
                step_results.append("")

        # Step 3 — synthesize
        synthesis_messages: list[dict[str, Any]] = [
            {"role": "system", "content": system},
            {
                "role": "user",
                "content": (
                    f"Goal: {prompt}\n\n"
                    f"Plan executed:\n{plan_text}\n\n"
                    "Step results:\n"
                    + "\n\n".join(
                        f"Step {i+1}: {r}" for i, r in enumerate(step_results)
                    )
                    + "\n\nSynthesize these results into a final, well-structured answer."
                ),
            },
        ]
        final_msg = await self._call_model(client, synthesis_messages)
        return final_msg.content or ""

    async def _run_react(
        self,
        client: Any,
        prompt: str,
        tool_calls_log: list[dict[str, Any]],
    ) -> str:
        """
        react (§14.3): Reason + Act loop — call model, execute tool calls,
        feed results back, repeat until model produces a final response.
        """
        openai_tools = self._openai_tools()
        tool_map = {td.name: td for td in self._tools}

        messages: list[dict[str, Any]] = []
        if self.instructions:
            messages.append({"role": "system", "content": self.instructions})
        messages.append({"role": "user", "content": prompt})

        for _ in range(self.limits.max_iterations):
            msg = await self._call_model(client, messages, openai_tools or None)
            if not msg.tool_calls:
                return msg.content or ""
            messages.append(msg)
            tool_result_msgs = await self._execute_tool_calls(msg, tool_map, tool_calls_log)
            messages.extend(tool_result_msgs)

        # Max iterations — return last assistant content
        for m in reversed(messages):
            content = m.content if hasattr(m, "content") else m.get("content")
            role = m.role if hasattr(m, "role") else m.get("role")
            if role == "assistant" and content:
                return content
        return ""

    async def _run_critic(
        self,
        client: Any,
        prompt: str,
        tool_calls_log: list[dict[str, Any]],
    ) -> str:
        """
        critic (§14.3): Draft → critic evaluation → revise loop.
        Runs up to max_iterations rounds of draft + critique.
        """
        openai_tools = self._openai_tools()
        tool_map = {td.name: td for td in self._tools}
        system = self.instructions or "You are a helpful assistant."

        draft = ""
        for round_i in range(self.limits.max_iterations):
            # Draft (or revise)
            if round_i == 0:
                draft_prompt = prompt
            else:
                draft_prompt = (
                    f"Original goal: {prompt}\n\n"
                    f"Previous draft:\n{draft}\n\n"
                    "Revise the draft to address the critic's feedback above. "
                    "Return only the improved draft."
                )
            draft_messages: list[dict[str, Any]] = [
                {"role": "system", "content": system},
                {"role": "user", "content": draft_prompt},
            ]
            for _ in range(self.limits.max_iterations):
                msg = await self._call_model(client, draft_messages, openai_tools or None)
                if not msg.tool_calls:
                    draft = msg.content or ""
                    break
                draft_messages.append(msg)
                tool_result_msgs = await self._execute_tool_calls(msg, tool_map, tool_calls_log)
                draft_messages.extend(tool_result_msgs)

            # Critic evaluation
            critic_messages: list[dict[str, Any]] = [
                {
                    "role": "system",
                    "content": (
                        "You are a strict critic. Evaluate the draft against the goal. "
                        "Reply with either PASS (if the draft is good enough) or "
                        "REVISE: <specific feedback>."
                    ),
                },
                {
                    "role": "user",
                    "content": f"Goal: {prompt}\n\nDraft:\n{draft}",
                },
            ]
            critic_msg = await self._call_model(client, critic_messages)
            verdict = (critic_msg.content or "").strip()
            if verdict.upper().startswith("PASS"):
                break
            # Append critic feedback for next round
            draft = f"{draft}\n\n[Critic feedback]: {verdict}"

        return draft

    # ── Public run interface ───────────────────────────────────────────────

    async def run(self, prompt: str) -> AgentResult:
        """
        Run the agent on a single prompt in-process. No runtime server needed.

        Dispatches to the appropriate strategy executor:
          - ``plan-and-execute`` (default): plan → execute steps → synthesize
          - ``react``: reason + act tool loop
          - ``critic``: draft → critique → revise
        """
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

        tool_calls_log: list[dict[str, Any]] = []

        if self.strategy == "plan-and-execute":
            final_output = await self._run_plan_and_execute(client, prompt, tool_calls_log)
        elif self.strategy == "react":
            final_output = await self._run_react(client, prompt, tool_calls_log)
        elif self.strategy == "critic":
            final_output = await self._run_critic(client, prompt, tool_calls_log)
        else:
            raise ValueError(
                f"Unknown strategy {self.strategy!r}. "
                "Valid options: 'plan-and-execute', 'react', 'critic'."
            )

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
        return (
            f"Agent(name={self.name!r}, model={self.model!r}, "
            f"tools={self.tool_names}, strategy={self.strategy!r})"
        )


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
