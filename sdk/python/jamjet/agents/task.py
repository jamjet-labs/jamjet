"""
@task — the absolute simplest way to define an agent task in JamJet.

The function docstring becomes the instruction. The function signature is the
contract. Under the hood it creates an Agent and runs it.

Usage::

    from jamjet import task, tool

    @tool
    async def web_search(query: str) -> str:
        return f"Search results for: {query}"

    @task(model="gpt-5.2", tools=[web_search])
    async def research(question: str) -> str:
        \"\"\"You are a research assistant. Search first, then summarize.\"\"\"

    result = await research("What are the latest trends in agent runtimes?")
"""

from __future__ import annotations

import functools
from collections.abc import Callable
from typing import Any, TypeVar

from jamjet.agents.agent import Agent

F = TypeVar("F", bound=Callable[..., Any])


def task(
    func: F | None = None,
    *,
    model: str = "default",
    tools: list[Callable[..., Any]] | None = None,
    strategy: str = "plan-and-execute",
    max_iterations: int = 10,
    max_cost_usd: float = 1.0,
    timeout_seconds: int = 300,
) -> Any:
    """
    Turn a function into a JamJet task.

    The function's **docstring** becomes the agent's instructions.
    The first positional argument is the user prompt.

    Can be used with or without parentheses::

        @task(model="gpt-5.2", tools=[web_search])
        async def research(question: str) -> str:
            \"\"\"You are a research assistant.\"\"\"

        @task
        async def simple_task(question: str) -> str:
            \"\"\"Answer questions directly.\"\"\"
    """

    def decorator(fn: F) -> F:
        instructions = (fn.__doc__ or "").strip()
        task_name = fn.__name__

        agent = Agent(
            task_name,
            model=model,
            tools=tools or [],
            instructions=instructions,
            strategy=strategy,
            max_iterations=max_iterations,
            max_cost_usd=max_cost_usd,
            timeout_seconds=timeout_seconds,
        )

        @functools.wraps(fn)
        async def wrapper(*args: Any, **kwargs: Any) -> str:
            # The first positional arg (or the first kwarg) is the prompt
            if args:
                prompt = str(args[0])
            elif kwargs:
                prompt = str(next(iter(kwargs.values())))
            else:
                raise TypeError(f"{task_name}() requires at least one argument (the prompt)")
            result = await agent.run(prompt)
            return result.output

        wrapper._jamjet_agent = agent  # type: ignore[attr-defined]
        return wrapper  # type: ignore[return-value]

    if func is not None:
        return decorator(func)
    return decorator
