"""Tests for ``Team.deploy`` (Track 7a-3) — register each sub-agent's workflow.

A team is N agent IRs (Track 6, Path A): deploy reuses :meth:`Agent.deploy` per
sub-agent and returns one :class:`DeployResult` per member. The Python
orchestration (sequencing / routing / merge) still runs client-side; deploy
pre-registers the building blocks. We mock ``JamjetClient`` and collect every
constructed instance so we can assert each sub-agent was registered against the
resolved runtime.
"""

from __future__ import annotations

from typing import Any

import pytest

from jamjet.agents.agent import Agent
from jamjet.deploy import LOCAL_RUNTIME_URL, DeployResult
from jamjet.team import Loop, Parallel, Sequential
from jamjet.tools.decorators import tool


@tool
async def echo(text: str) -> str:
    """Echo the text."""
    return text


def _agent(name: str) -> Agent:
    return Agent(name, model="anthropic/claude-sonnet-4-6", tools=[echo], instructions="x")


class _FakeDeployClient:
    def __init__(self, base_url: str = "http://localhost:7700", api_token: str | None = None) -> None:
        self.base_url = base_url
        self.api_token = api_token
        self.created: list[dict[str, Any]] = []
        self.cron: list[dict[str, Any]] = []

    async def __aenter__(self) -> _FakeDeployClient:
        return self

    async def __aexit__(self, *args: Any) -> None:
        return None

    async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
        self.created.append(ir)
        return {"workflow_id": ir["workflow_id"], "version": ir.get("version")}

    async def create_cron_job(self, **kwargs: Any) -> dict[str, Any]:
        self.cron.append(kwargs)
        return {"name": kwargs.get("name")}


def _patch_client(monkeypatch: pytest.MonkeyPatch) -> list[_FakeDeployClient]:
    """Patch JamjetClient; collect EVERY constructed instance (one per sub-agent)."""
    instances: list[_FakeDeployClient] = []

    def factory(base_url: str = "http://localhost:7700", api_token: str | None = None, **_: Any) -> _FakeDeployClient:
        client = _FakeDeployClient(base_url=base_url, api_token=api_token)
        instances.append(client)
        return client

    monkeypatch.setattr("jamjet.client.JamjetClient", factory)
    return instances


def _clear_env(monkeypatch: pytest.MonkeyPatch) -> None:
    for var in ("JAMJET_RUNTIME_URL", "JAMJET_RUNTIME_TOKEN", "JAMJET_TOKEN"):
        monkeypatch.delenv(var, raising=False)


async def test_sequential_deploys_each_member(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    instances = _patch_client(monkeypatch)
    team = Sequential([_agent("alpha"), _agent("beta")])

    results = await team.deploy()

    assert isinstance(results, list)
    assert len(results) == 2
    assert all(isinstance(r, DeployResult) for r in results)
    assert [r.workflow_id for r in results] == ["alpha", "beta"]
    # One client per sub-agent, each registered its own IR against LOCAL.
    assert len(instances) == 2
    assert {inst.created[0]["workflow_id"] for inst in instances} == {"alpha", "beta"}
    assert all(r.url == LOCAL_RUNTIME_URL for r in results)


async def test_parallel_deploys_each_member(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    _patch_client(monkeypatch)
    team = Parallel([_agent("a"), _agent("b"), _agent("c")])

    results = await team.deploy()

    assert [r.workflow_id for r in results] == ["a", "b", "c"]


async def test_loop_deploys_single_member(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    _patch_client(monkeypatch)
    team = Loop(_agent("refiner"), max_iters=3)

    results = await team.deploy()

    assert len(results) == 1
    assert results[0].workflow_id == "refiner"


async def test_team_deploy_threads_runtime_to_each_subagent(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    monkeypatch.setenv("JAMJET_RUNTIME_URL", "https://engine.internal:8080")
    monkeypatch.setenv("JAMJET_RUNTIME_TOKEN", "tok-self")
    instances = _patch_client(monkeypatch)
    team = Sequential([_agent("alpha"), _agent("beta")])

    results = await team.deploy(runtime="self-host")

    assert all(r.runtime == "self-host" for r in results)
    assert all(r.url == "https://engine.internal:8080" for r in results)
    assert all(inst.base_url == "https://engine.internal:8080" for inst in instances)
    assert all(inst.api_token == "tok-self" for inst in instances)


async def test_team_deploy_with_schedule_schedules_each_subagent(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    instances = _patch_client(monkeypatch)
    team = Parallel([_agent("a"), _agent("b")])

    results = await team.deploy(schedule="0 9 * * *")

    assert all(r.scheduled for r in results)
    # Each sub-agent's workflow got its own cron job.
    assert all(len(inst.cron) == 1 for inst in instances)
    assert {inst.cron[0]["workflow_id"] for inst in instances} == {"a", "b"}
