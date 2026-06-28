"""Tests for T7a-4 — the dead ``entry.deploy`` + ``CloudRuntime`` stubs are
redirected/deprecated, never a ``NotImplementedError`` masquerading as a real API.

- ``jamjet.entry.deploy(target)`` now delegates to ``target.deploy(...)`` when the
  target is an Agent/Team (with a DeprecationWarning), and raises a clear error
  pointing at ``Agent.deploy()`` otherwise.
- ``CloudRuntime.execute`` raises an HONEST message (Cloud is the governance
  plane, use ``Agent.deploy(runtime='cloud')``), not the old "lands in Phase 5".
- ``jamjet.deploy`` resolves cleanly to the deploy MODULE (no function/module
  shadowing collision).
"""

from __future__ import annotations

from typing import Any

import pytest

from jamjet.agents.agent import Agent
from jamjet.deploy import DeployResult
from jamjet.team import Sequential
from jamjet.tools.decorators import tool


@tool
async def echo(text: str) -> str:
    """Echo the text."""
    return text


def _agent(name: str = "redir") -> Agent:
    return Agent(name, model="anthropic/claude-sonnet-4-6", tools=[echo], instructions="x")


class _FakeDeployClient:
    def __init__(self, base_url: str = "http://localhost:7700", api_token: str | None = None) -> None:
        self.base_url = base_url
        self.api_token = api_token

    async def __aenter__(self) -> _FakeDeployClient:
        return self

    async def __aexit__(self, *args: Any) -> None:
        return None

    async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
        return {"workflow_id": ir["workflow_id"]}

    async def create_cron_job(self, **kwargs: Any) -> dict[str, Any]:
        return {"name": kwargs.get("name")}


def _patch_client(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("jamjet.client.JamjetClient", lambda **kw: _FakeDeployClient(**kw))


def _clear_env(monkeypatch: pytest.MonkeyPatch) -> None:
    for var in ("JAMJET_RUNTIME_URL", "JAMJET_CLOUD_RUNTIME_URL", "JAMJET_TOKEN"):
        monkeypatch.delenv(var, raising=False)


# ── entry.deploy redirects ────────────────────────────────────────────────────


async def test_entry_deploy_redirects_agent(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    _patch_client(monkeypatch)
    from jamjet.entry import deploy as entry_deploy

    with pytest.warns(DeprecationWarning, match="Agent.deploy"):
        result = await entry_deploy(_agent())

    assert isinstance(result, DeployResult)
    assert result.runtime == "local"


async def test_entry_deploy_redirects_team(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    _patch_client(monkeypatch)
    from jamjet.entry import deploy as entry_deploy

    with pytest.warns(DeprecationWarning):
        results = await entry_deploy(Sequential([_agent("a"), _agent("b")]))

    assert isinstance(results, list)
    assert [r.workflow_id for r in results] == ["a", "b"]


async def test_entry_deploy_non_agent_raises_clear_error(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    from jamjet.entry import deploy as entry_deploy

    # A legacy spec/function target has no .deploy -> clear redirect error, and it
    # must NOT be the old "lands in Phase 5" NotImplementedError.
    with pytest.raises((TypeError, ValueError)) as exc:
        await entry_deploy(object())
    msg = str(exc.value)
    assert "Agent.deploy" in msg
    assert "Phase 5" not in msg


# ── CloudRuntime honest message ───────────────────────────────────────────────


async def test_cloud_runtime_execute_points_at_agent_deploy() -> None:
    from jamjet.runtime.stub import CloudRuntime

    with pytest.raises(NotImplementedError) as exc:
        await CloudRuntime().execute(object(), {})
    msg = str(exc.value)
    assert "Agent.deploy" in msg
    assert "Phase 5" not in msg  # no masquerading "coming soon" stub message


async def test_cloud_runtime_resume_points_at_agent_deploy() -> None:
    from jamjet.runtime.stub import CloudRuntime

    with pytest.raises(NotImplementedError) as exc:
        await CloudRuntime().resume(object(), "exec_1")
    assert "Phase 5" not in str(exc.value)


# ── no name collision: jamjet.deploy is the module ────────────────────────────


def test_jamjet_deploy_resolves_to_module() -> None:
    import jamjet

    # The deploy surface lives in the jamjet.deploy MODULE; accessing it after
    # import must not be shadowed by a top-level deploy() function.
    assert hasattr(jamjet.deploy, "resolve_runtime_target")
    assert hasattr(jamjet.deploy, "RuntimeTarget")
    # The friendly method is the public deploy entry point.
    assert callable(jamjet.Agent.deploy)
    # The deprecated top-level deploy() function is no longer exported.
    assert "deploy" not in jamjet.__all__
