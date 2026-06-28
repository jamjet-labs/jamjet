"""Shared test helpers for the Team patterns (not collected by pytest).

``scripted_agent`` builds a REAL :class:`~jamjet.agents.agent.Agent` (so the
coordinator's ``isinstance(coord, Agent)`` router detection works and governance
is real) but replaces its ``run`` / ``run_durable`` with scripted coroutines, so
the patterns run with NO engine and NO network. Each call is recorded on
``agent.calls`` as ``(mode, prompt, session)``.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from jamjet.agents.agent import Agent, AgentResult


def scripted_agent(
    name: str,
    *,
    transform: Callable[[str], str] | None = None,
    output: str | None = None,
    fail: BaseException | None = None,
    **agent_kwargs: Any,
) -> Agent:
    """A real Agent whose ``run`` / ``run_durable`` are scripted (no engine).

    Output precedence: ``fail`` (raise) > ``output`` (constant) > ``transform``
    (``f(prompt)``) > the default ``f"{name}:{prompt}"`` (so sequential threading
    is observable). ``agent_kwargs`` (e.g. ``budget=``) flow to the real Agent ctor.
    """
    agent = Agent(name, model="anthropic/claude-sonnet-4-6", tools=[], **agent_kwargs)
    agent.calls = []  # type: ignore[attr-defined]

    def _compute(prompt: str) -> str:
        if fail is not None:
            raise fail
        if output is not None:
            return output
        if transform is not None:
            return transform(prompt)
        return f"{name}:{prompt}"

    async def _run(prompt: str, *, session: Any = None) -> AgentResult:
        agent.calls.append(("run", prompt, session))  # type: ignore[attr-defined]
        return AgentResult(output=_compute(prompt), tool_calls=[], ir={})

    async def _run_durable(
        prompt: str,
        *,
        runtime_url: str = "http://127.0.0.1:7700",
        session: Any = None,
        **kw: Any,
    ) -> AgentResult:
        agent.calls.append(("run_durable", prompt, session))  # type: ignore[attr-defined]
        return AgentResult(output=_compute(prompt), tool_calls=[], ir={})

    agent.run = _run  # type: ignore[method-assign,assignment]
    agent.run_durable = _run_durable  # type: ignore[method-assign,assignment]
    return agent
