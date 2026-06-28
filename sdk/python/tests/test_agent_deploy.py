"""Tests for ``Agent.deploy`` (Track 7a-2) — ship the compiled IR to a runtime.

No real engine: a fake stands in for ``JamjetClient`` and records the base URL +
token it was constructed with and every IR / cron job registered. We assert that
deploy compiles the SAME agent-loop IR as ``run_durable``, registers it over
``create_workflow`` against the RESOLVED url/token, optionally installs a cron
schedule, never strips the agent's governance from the IR, records (but does not
call into) Cloud governance for the cloud leg, and defaults to LOCAL so a bare
``deploy()`` never targets a remote URL.
"""

from __future__ import annotations

from typing import Any

import pytest

from jamjet.agents.agent import Agent
from jamjet.compiler.agent_ir import compile_agent_to_ir
from jamjet.deploy import LOCAL_RUNTIME_URL, DeployResult
from jamjet.tools.decorators import tool


@tool
async def get_weather(city: str) -> str:
    """Return the weather for a city."""
    return f"sunny in {city}"


def _agent(**kw: Any) -> Agent:
    return Agent(
        "weatherbot",
        model="anthropic/claude-sonnet-4-6",
        tools=[get_weather],
        instructions="You are a weather assistant.",
        **kw,
    )


class _FakeDeployClient:
    """Async-context fake of JamjetClient that records its construction + calls."""

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
        return {"name": kwargs.get("name"), "next_run_at": "2026-06-28T09:00:00Z"}


def _patch_client(monkeypatch: pytest.MonkeyPatch) -> dict[str, Any]:
    """Patch JamjetClient with the fake; capture the constructed instance."""
    captured: dict[str, Any] = {}

    def factory(base_url: str = "http://localhost:7700", api_token: str | None = None, **_: Any) -> _FakeDeployClient:
        client = _FakeDeployClient(base_url=base_url, api_token=api_token)
        captured["client"] = client
        return client

    monkeypatch.setattr("jamjet.client.JamjetClient", factory)
    return captured


def _clear_env(monkeypatch: pytest.MonkeyPatch) -> None:
    for var in (
        "JAMJET_RUNTIME_URL",
        "JAMJET_RUNTIME_TOKEN",
        "JAMJET_CLOUD_RUNTIME_URL",
        "JAMJET_CLOUD_TOKEN",
        "JAMJET_API_KEY",
        "JAMJET_TOKEN",
    ):
        monkeypatch.delenv(var, raising=False)


# ── local default (no prod-targeting footgun) ─────────────────────────────────


async def test_deploy_defaults_to_local(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    captured = _patch_client(monkeypatch)

    result = await _agent().deploy()

    assert isinstance(result, DeployResult)
    assert result.runtime == "local"
    assert result.url == LOCAL_RUNTIME_URL
    assert result.scheduled is False
    assert result.cloud_governance is False
    # Registered against the LOCAL engine — never a remote URL.
    assert captured["client"].base_url == LOCAL_RUNTIME_URL
    assert captured["client"].api_token is None


async def test_deploy_registers_the_compiled_ir(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    captured = _patch_client(monkeypatch)
    agent = _agent()

    result = await agent.deploy()

    client = captured["client"]
    assert len(client.created) == 1
    deployed_ir = client.created[0]
    # The SAME IR run_durable builds (prompt is not embedded in the IR).
    assert deployed_ir == compile_agent_to_ir(agent, "")
    assert deployed_ir["workflow_id"] == "weatherbot"
    assert deployed_ir["labels"]["jamjet.agent.loop"] == "true"
    assert result.workflow_id == "weatherbot"


async def test_deploy_threads_max_turns_into_ir(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    captured = _patch_client(monkeypatch)
    agent = _agent()

    await agent.deploy(max_turns=12)

    deployed_ir = captured["client"].created[0]
    # Registered at the requested max_turns — same IR run_durable(max_turns=12) ships.
    assert deployed_ir == compile_agent_to_ir(agent, "", max_turns=12)
    # And NOT the default-8 IR: more turns unroll more nodes -> a different version.
    assert deployed_ir != compile_agent_to_ir(agent, "", max_turns=8)


# ── governance is never stripped on deploy ────────────────────────────────────


async def test_deploy_does_not_strip_governance(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    captured = _patch_client(monkeypatch)
    # A governed agent: an explicit cost budget compiles to cost_budget_usd.
    agent = _agent(max_cost_usd=0.25)

    await agent.deploy()

    deployed_ir = captured["client"].created[0]
    assert deployed_ir["cost_budget_usd"] == 0.25
    # PII is on by default -> data_policy metadata survives the deploy.
    assert "data_policy" in deployed_ir


# ── self-host ─────────────────────────────────────────────────────────────────


async def test_deploy_self_host_uses_env_url_and_token(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    monkeypatch.setenv("JAMJET_RUNTIME_URL", "https://engine.internal:8080")
    monkeypatch.setenv("JAMJET_RUNTIME_TOKEN", "tok-self")
    captured = _patch_client(monkeypatch)

    result = await _agent().deploy(runtime="self-host")

    assert result.runtime == "self-host"
    assert result.url == "https://engine.internal:8080"
    assert captured["client"].base_url == "https://engine.internal:8080"
    assert captured["client"].api_token == "tok-self"
    assert result.cloud_governance is False


async def test_deploy_self_host_unset_raises(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    _patch_client(monkeypatch)
    with pytest.raises(ValueError, match="JAMJET_RUNTIME_URL"):
        await _agent().deploy(runtime="self-host")


# ── cloud (hosted engine + governance, never load-bearing) ────────────────────


async def test_deploy_cloud_targets_hosted_engine_and_records_governance(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _clear_env(monkeypatch)
    monkeypatch.setenv("JAMJET_CLOUD_RUNTIME_URL", "https://my-engine.fly.dev")
    monkeypatch.setenv("JAMJET_CLOUD_TOKEN", "tok-cloud")
    captured = _patch_client(monkeypatch)

    result = await _agent().deploy(runtime="cloud")

    # The deploy client hits the hosted ENGINE, never api.jamjet.dev.
    assert captured["client"].base_url == "https://my-engine.fly.dev"
    assert captured["client"].api_token == "tok-cloud"
    assert result.runtime == "cloud"
    assert result.url == "https://my-engine.fly.dev"
    # cloud_governance is recorded; deploy does NOT call into Cloud to succeed.
    assert result.cloud_governance is True
    assert "api.jamjet.dev" not in captured["client"].base_url


# ── scheduling ────────────────────────────────────────────────────────────────


async def test_deploy_with_schedule_creates_cron_job(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    captured = _patch_client(monkeypatch)

    result = await _agent().deploy(schedule="0 9 * * *")

    client = captured["client"]
    assert len(client.cron) == 1
    job = client.cron[0]
    assert job["cron_expression"] == "0 9 * * *"
    assert job["workflow_id"] == "weatherbot"
    assert result.scheduled is True
    # The scheduled seed is build_initial_state(agent, "") — an empty user turn
    # (the recurring intent lives in the agent's instructions, not the seed). The
    # last message is the empty prompt the engine fires the workflow with.
    seed = job["input"]
    assert seed["messages"][-1] == {"role": "user", "content": ""}


async def test_deploy_without_schedule_creates_no_cron(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    captured = _patch_client(monkeypatch)

    result = await _agent().deploy()

    assert captured["client"].cron == []
    assert result.scheduled is False


# ── explicit url + conflict guard ─────────────────────────────────────────────


async def test_deploy_runtime_url_passthrough(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    captured = _patch_client(monkeypatch)

    result = await _agent().deploy(runtime_url="https://my-box.example.com")

    assert captured["client"].base_url == "https://my-box.example.com"
    assert result.url == "https://my-box.example.com"
    assert result.cloud_governance is False


async def test_deploy_runtime_and_runtime_url_conflict_raises(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    _patch_client(monkeypatch)
    with pytest.raises(ValueError, match="runtime"):
        await _agent().deploy(runtime="local", runtime_url="https://box.example.com")


# ── registered-but-unscheduled is honest and distinguishable ──────────────────


async def test_deploy_schedule_failure_after_register_is_distinguishable(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """create_workflow succeeds, create_cron_job fails: the workflow IS registered
    but unscheduled. The raised error must say so AND chain the cron error, so the
    registered-but-unscheduled state is distinguishable from a never-registered one
    (and Team.deploy's captured exception carries that distinction)."""
    _clear_env(monkeypatch)

    class _CronFailsClient(_FakeDeployClient):
        async def create_cron_job(self, **kwargs: Any) -> dict[str, Any]:
            raise RuntimeError("cron backend down")

    captured: dict[str, Any] = {}

    def factory(base_url: str = "http://localhost:7700", api_token: str | None = None, **_: Any) -> _CronFailsClient:
        client = _CronFailsClient(base_url=base_url, api_token=api_token)
        captured["client"] = client
        return client

    monkeypatch.setattr("jamjet.client.JamjetClient", factory)

    with pytest.raises(RuntimeError) as exc:
        await _agent().deploy(schedule="0 9 * * *")

    msg = str(exc.value)
    assert "weatherbot" in msg  # names the registered workflow id
    assert "registered" in msg  # states it WAS registered
    assert "scheduling failed" in msg  # only the schedule step failed
    # Chained from the underlying cron error (registered-but-unscheduled, not lost).
    assert isinstance(exc.value.__cause__, RuntimeError)
    assert "cron backend down" in str(exc.value.__cause__)
    # The workflow really was registered before the schedule step blew up.
    assert captured["client"].created and captured["client"].created[0]["workflow_id"] == "weatherbot"


async def test_deploy_create_workflow_failure_is_not_registered(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """create_workflow itself fails: the workflow is NOT registered, so the error
    must NOT claim it was registered (the opposite of the schedule-failure case)."""
    _clear_env(monkeypatch)

    class _RegisterFailsClient(_FakeDeployClient):
        async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
            raise RuntimeError("engine rejected the IR")

    monkeypatch.setattr(
        "jamjet.client.JamjetClient",
        lambda base_url="http://localhost:7700", api_token=None, **_: _RegisterFailsClient(
            base_url=base_url, api_token=api_token
        ),
    )

    with pytest.raises(RuntimeError) as exc:
        await _agent().deploy(schedule="0 9 * * *")

    msg = str(exc.value)
    assert "engine rejected the IR" in msg
    assert "registered" not in msg.lower()  # a create_workflow failure never claims registration
