"""Tests for ``Team.deploy`` (Track 7a-3) — register each sub-agent's workflow.

A team is N agent IRs (Track 6, Path A): deploy reuses :meth:`Agent.deploy` per
sub-agent and returns a ``{agent.name: DeployResult | BaseException}`` mapping
(insertion order == declared order), mirroring the run path's per-agent
child-crash isolation: a sub-agent whose deploy raises has its exception captured
as that key's value and the rest still deploy. The Python orchestration
(sequencing / routing / merge) still runs client-side; deploy pre-registers the
building blocks. We mock ``JamjetClient`` and collect every constructed instance
so we can assert each sub-agent was registered against the resolved runtime.
"""

from __future__ import annotations

from typing import Any

import pytest

from jamjet.agents.agent import Agent
from jamjet.compiler.agent_ir import compile_agent_to_ir
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

    # The mapping is keyed by agent name in declared order.
    assert isinstance(results, dict)
    assert list(results.keys()) == ["alpha", "beta"]
    assert all(isinstance(r, DeployResult) for r in results.values())
    assert results["alpha"].workflow_id == "alpha"
    assert results["beta"].workflow_id == "beta"
    # One client per sub-agent, each registered its own IR against LOCAL.
    assert len(instances) == 2
    assert {inst.created[0]["workflow_id"] for inst in instances} == {"alpha", "beta"}
    assert all(r.url == LOCAL_RUNTIME_URL for r in results.values())


async def test_parallel_deploys_each_member(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    _patch_client(monkeypatch)
    team = Parallel([_agent("a"), _agent("b"), _agent("c")])

    results = await team.deploy()

    assert list(results.keys()) == ["a", "b", "c"]
    assert [r.workflow_id for r in results.values()] == ["a", "b", "c"]


async def test_loop_deploys_single_member(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    _patch_client(monkeypatch)
    team = Loop(_agent("refiner"), max_iters=3)

    results = await team.deploy()

    assert list(results.keys()) == ["refiner"]
    assert results["refiner"].workflow_id == "refiner"


async def test_team_deploy_threads_runtime_to_each_subagent(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    monkeypatch.setenv("JAMJET_RUNTIME_URL", "https://engine.internal:8080")
    monkeypatch.setenv("JAMJET_RUNTIME_TOKEN", "tok-self")
    instances = _patch_client(monkeypatch)
    team = Sequential([_agent("alpha"), _agent("beta")])

    results = await team.deploy(runtime="self-host")

    assert all(r.runtime == "self-host" for r in results.values())
    assert all(r.url == "https://engine.internal:8080" for r in results.values())
    assert all(inst.base_url == "https://engine.internal:8080" for inst in instances)
    assert all(inst.api_token == "tok-self" for inst in instances)


async def test_team_deploy_with_schedule_schedules_each_subagent(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    instances = _patch_client(monkeypatch)
    team = Parallel([_agent("a"), _agent("b")])

    results = await team.deploy(schedule="0 9 * * *")

    assert all(r.scheduled for r in results.values())
    # Each sub-agent's workflow got its own cron job.
    assert all(len(inst.cron) == 1 for inst in instances)
    assert {inst.cron[0]["workflow_id"] for inst in instances} == {"a", "b"}


# ── child-crash isolation: one failed sub-agent never aborts the team ─────────


async def test_team_deploy_isolates_a_failing_subagent(monkeypatch: pytest.MonkeyPatch) -> None:
    """A middle sub-agent whose deploy raises must NOT abort the team.

    The returned mapping carries the two successful :class:`DeployResult`s AND the
    captured exception for the failed member, keyed by agent name — mirroring the
    run path's per-agent isolation. The team itself does not raise.
    """
    _clear_env(monkeypatch)

    class _BoomError(RuntimeError):
        pass

    class _FailingMiddleClient:
        def __init__(self, base_url: str = "http://localhost:7700", api_token: str | None = None) -> None:
            self.base_url = base_url
            self.api_token = api_token

        async def __aenter__(self) -> _FailingMiddleClient:
            return self

        async def __aexit__(self, *args: Any) -> None:
            return None

        async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
            if ir["workflow_id"] == "beta":
                raise _BoomError("middle sub-agent deploy failed")
            return {"workflow_id": ir["workflow_id"], "version": ir.get("version")}

        async def create_cron_job(self, **kwargs: Any) -> dict[str, Any]:
            return {"name": kwargs.get("name")}

    def factory(
        base_url: str = "http://localhost:7700", api_token: str | None = None, **_: Any
    ) -> _FailingMiddleClient:
        return _FailingMiddleClient(base_url=base_url, api_token=api_token)

    monkeypatch.setattr("jamjet.client.JamjetClient", factory)

    team = Sequential([_agent("alpha"), _agent("beta"), _agent("gamma")])

    # Must NOT raise even though the middle sub-agent's deploy fails.
    results = await team.deploy()

    assert isinstance(results, dict)
    assert list(results.keys()) == ["alpha", "beta", "gamma"]
    # The two healthy sub-agents still registered.
    assert isinstance(results["alpha"], DeployResult)
    assert isinstance(results["gamma"], DeployResult)
    assert results["alpha"].workflow_id == "alpha"
    assert results["gamma"].workflow_id == "gamma"
    # The failed member's exception is captured as its value, not raised.
    assert isinstance(results["beta"], _BoomError)


async def test_team_deploy_returns_mapping_keyed_by_agent_name(monkeypatch: pytest.MonkeyPatch) -> None:
    """Happy path: the mapping is keyed by each agent name, in declared order."""
    _clear_env(monkeypatch)
    _patch_client(monkeypatch)
    team = Sequential([_agent("alpha"), _agent("beta"), _agent("gamma")])

    results = await team.deploy()

    assert isinstance(results, dict)
    assert list(results.keys()) == ["alpha", "beta", "gamma"]
    assert all(isinstance(v, DeployResult) for v in results.values())
    assert {name: r.workflow_id for name, r in results.items()} == {
        "alpha": "alpha",
        "beta": "beta",
        "gamma": "gamma",
    }


async def test_team_deploy_threads_max_turns_to_each_subagent(monkeypatch: pytest.MonkeyPatch) -> None:
    """``max_turns`` flows through to each sub-agent's compiled IR."""
    _clear_env(monkeypatch)
    instances = _patch_client(monkeypatch)
    team = Sequential([_agent("alpha"), _agent("beta")])

    await team.deploy(max_turns=12)

    for inst in instances:
        ir = inst.created[0]
        name = ir["workflow_id"]
        # Registered at the requested max_turns, not the default 8.
        assert ir == compile_agent_to_ir(_agent(name), "", max_turns=12)
        assert ir != compile_agent_to_ir(_agent(name), "", max_turns=8)
