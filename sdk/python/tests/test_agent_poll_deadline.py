"""``Agent._poll_to_terminal`` keeps to its deadline while the engine is down."""

from __future__ import annotations

import types

import httpx
import pytest

from jamjet.agents import agent as agent_mod
from jamjet.agents.agent import Agent


class _DownClient:
    def __init__(self, clock: list[float]) -> None:
        self.clock = clock
        self.calls: list[float] = []

    async def get_execution(self, exec_id: str) -> dict:
        self.calls.append(self.clock[0])
        raise httpx.ConnectError("engine down")


async def test_backoff_never_sleeps_past_the_deadline(monkeypatch: pytest.MonkeyPatch) -> None:
    clock = [1000.0]
    slept: list[float] = []

    async def fake_sleep(s: float) -> None:
        slept.append(s)
        clock[0] += s

    # Stand-ins local to the agent module, so the event loop keeps real time.
    monkeypatch.setattr(agent_mod, "time", types.SimpleNamespace(monotonic=lambda: clock[0]))
    monkeypatch.setattr(agent_mod, "asyncio", types.SimpleNamespace(sleep=fake_sleep))

    agent = Agent("a", model="anthropic/claude-sonnet-4-6", instructions="x", tools=[])
    agent.limits.timeout_seconds = 1
    client = _DownClient(clock)
    with pytest.raises(TimeoutError) as info:
        await agent._poll_to_terminal(client, "exec_1", "http://x")

    assert sum(slept) <= 1.0 + 1e-9
    assert all(t <= 1000.0 + 1.0 for t in client.calls), "no request after the deadline"
    assert isinstance(info.value.__cause__, httpx.ConnectError)


async def test_timeout_with_a_running_status_names_the_status(monkeypatch: pytest.MonkeyPatch) -> None:
    clock = [0.0]

    async def fake_sleep(s: float) -> None:
        clock[0] += s

    class _Running:
        async def get_execution(self, exec_id: str) -> dict:
            return {"status": "running"}

    monkeypatch.setattr(agent_mod, "time", types.SimpleNamespace(monotonic=lambda: clock[0]))
    monkeypatch.setattr(agent_mod, "asyncio", types.SimpleNamespace(sleep=fake_sleep))
    agent = Agent("a", model="anthropic/claude-sonnet-4-6", instructions="x", tools=[])
    agent.limits.timeout_seconds = 1
    with pytest.raises(TimeoutError, match="last status: 'running'"):
        await agent._poll_to_terminal(_Running(), "exec_1", "http://x")
