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
    agent.calls = []  # type: ignore[attr-defined]  # (mode, prompt, session) per run
    agent.durable_runtime_urls = []  # type: ignore[attr-defined]  # runtime_url per durable run

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
        agent.durable_runtime_urls.append(runtime_url)  # type: ignore[attr-defined]
        return AgentResult(output=_compute(prompt), tool_calls=[], ir={})

    agent.run = _run  # type: ignore[method-assign,assignment]
    agent.run_durable = _run_durable  # type: ignore[method-assign,assignment]
    return agent


# ── Durable client fake (drives REAL Agent.run_durable, no engine) ─────────────


def completed_terminal(answer: str) -> dict[str, Any]:
    """A ``completed`` terminal execution whose final answer is *answer*."""
    return {
        "status": "completed",
        "current_state": {
            "messages": [{"role": "user", "content": "in"}],
            "last_model_output": answer,
            "last_model_finish_reason": "stop",
        },
    }


def failed_terminal(status: str = "failed") -> dict[str, Any]:
    """A non-``completed`` terminal (``failed`` / ``limit_exceeded`` / ``cancelled``)."""
    return {"status": status, "current_state": {}}


class MultiAgentFakeClient:
    """An async-context fake of :class:`~jamjet.client.JamjetClient` serving MANY
    sub-agents (one Team run starts several executions through one patched client).

    Terminals are keyed by ``workflow_id`` (== the agent name), so different
    sub-agents can be made to succeed or fail independently — exactly what the
    durable child-crash-isolation tests need. The SAME instance is shared across
    concurrent parallel sub-agent runs (single-threaded asyncio, distinct
    workflow ids -> no collision).
    """

    def __init__(self, terminals: dict[str, dict[str, Any]]) -> None:
        self._terminals = terminals
        self._by_exec: dict[str, dict[str, Any]] = {}
        self.created: list[str] = []
        self.started: list[tuple[str, dict[str, Any]]] = []

    async def __aenter__(self) -> MultiAgentFakeClient:
        return self

    async def __aexit__(self, *args: Any) -> None:
        return None

    async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
        self.created.append(ir["workflow_id"])
        return {"workflow_id": ir["workflow_id"]}

    async def start_execution(
        self, workflow_id: str, input: dict[str, Any], workflow_version: str | None = None
    ) -> dict[str, Any]:
        exec_id = f"exec-{workflow_id}"
        term = dict(self._terminals[workflow_id])
        term.setdefault("execution_id", exec_id)
        self._by_exec[exec_id] = term
        self.started.append((workflow_id, input))
        return {"execution_id": exec_id}

    async def get_execution(self, execution_id: str) -> dict[str, Any]:
        return self._by_exec[execution_id]

    async def get_events(self, execution_id: str) -> dict[str, Any]:
        return {"events": []}
