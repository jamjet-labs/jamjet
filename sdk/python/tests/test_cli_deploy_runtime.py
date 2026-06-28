"""Tests for ``jamjet deploy --runtime`` parity (Track 7a-7).

The CLI deploy command resolves ``--runtime`` with the SAME resolver as
``Agent.deploy`` (local / self-host / cloud / a URL), so the CLI and the SDK
agree on targeting. We patch the JamjetClient class (so the resolver actually
runs) and assert the resolved base URL + token reach it; a bad runtime errors
clearly; the default resolves to the shared local engine URL (``LOCAL_RUNTIME_URL``,
``http://127.0.0.1:7700``) — the SAME source ``Agent.deploy`` uses.
"""

from __future__ import annotations

from typing import Any

import pytest
from typer.testing import CliRunner

from jamjet.cli.main import app
from jamjet.deploy import LOCAL_RUNTIME_URL

runner = CliRunner()

FLEET = """
version: 1
defaults:
  model: claude-sonnet-4-6
  limits: { max_iterations: 2, max_cost_usd: 0.2, timeout_seconds: 30 }
agents:
  researcher:
    strategy: react
    goal: brief
"""


class _RecordingClient:
    def __init__(self, base_url: str = "http://localhost:7700", api_token: str | None = None) -> None:
        self.base_url = base_url
        self.api_token = api_token

    async def __aenter__(self) -> _RecordingClient:
        return self

    async def __aexit__(self, *args: Any) -> None:
        return None

    async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
        return {"workflow_id": ir["workflow_id"], "version": ir.get("version")}

    async def create_cron_job(self, **kwargs: Any) -> dict[str, Any]:
        return {"name": kwargs.get("name"), "next_run_at": "x"}


def _patch_client_class(monkeypatch: pytest.MonkeyPatch) -> dict[str, Any]:
    captured: dict[str, Any] = {}

    def factory(base_url: str = "http://localhost:7700", api_token: str | None = None, **_: Any) -> _RecordingClient:
        client = _RecordingClient(base_url=base_url, api_token=api_token)
        captured["client"] = client
        return client

    # _client() constructs jamjet.cli.main.JamjetClient; patch it so the resolver runs.
    monkeypatch.setattr("jamjet.cli.main.JamjetClient", factory)
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


def test_deploy_default_runtime_preserved(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    captured = _patch_client_class(monkeypatch)
    f = tmp_path / "fleet.yaml"
    f.write_text(FLEET)

    result = runner.invoke(app, ["deploy", str(f)])

    assert result.exit_code == 0, result.output
    # The default --runtime resolves through the SAME resolver as Agent.deploy, so
    # the CLI and the SDK agree on the local engine URL (127.0.0.1:7700, not localhost).
    assert captured["client"].base_url == LOCAL_RUNTIME_URL
    assert captured["client"].api_token is None


def test_deploy_runtime_local(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    captured = _patch_client_class(monkeypatch)
    f = tmp_path / "fleet.yaml"
    f.write_text(FLEET)

    result = runner.invoke(app, ["deploy", str(f), "--runtime", "local"])

    assert result.exit_code == 0, result.output
    assert captured["client"].base_url == "http://127.0.0.1:7700"


def test_deploy_runtime_self_host(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    monkeypatch.setenv("JAMJET_RUNTIME_URL", "https://engine.internal:8080")
    monkeypatch.setenv("JAMJET_RUNTIME_TOKEN", "tok-self")
    captured = _patch_client_class(monkeypatch)
    f = tmp_path / "fleet.yaml"
    f.write_text(FLEET)

    result = runner.invoke(app, ["deploy", str(f), "--runtime", "self-host"])

    assert result.exit_code == 0, result.output
    assert captured["client"].base_url == "https://engine.internal:8080"
    assert captured["client"].api_token == "tok-self"


def test_deploy_runtime_cloud(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    monkeypatch.setenv("JAMJET_CLOUD_RUNTIME_URL", "https://my-engine.fly.dev")
    monkeypatch.setenv("JAMJET_CLOUD_TOKEN", "tok-cloud")
    captured = _patch_client_class(monkeypatch)
    f = tmp_path / "fleet.yaml"
    f.write_text(FLEET)

    result = runner.invoke(app, ["deploy", str(f), "--runtime", "cloud"])

    assert result.exit_code == 0, result.output
    assert captured["client"].base_url == "https://my-engine.fly.dev"
    assert captured["client"].api_token == "tok-cloud"


def test_deploy_runtime_url_passthrough(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    captured = _patch_client_class(monkeypatch)
    f = tmp_path / "fleet.yaml"
    f.write_text(FLEET)

    result = runner.invoke(app, ["deploy", str(f), "--runtime", "https://box.example.com"])

    assert result.exit_code == 0, result.output
    assert captured["client"].base_url == "https://box.example.com"


def test_deploy_bad_runtime_errors_clearly(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    _patch_client_class(monkeypatch)
    f = tmp_path / "fleet.yaml"
    f.write_text(FLEET)

    result = runner.invoke(app, ["deploy", str(f), "--runtime", "banana"])

    assert result.exit_code != 0
    assert "banana" in result.output


def test_deploy_self_host_unset_errors_clearly(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    _patch_client_class(monkeypatch)
    f = tmp_path / "fleet.yaml"
    f.write_text(FLEET)

    result = runner.invoke(app, ["deploy", str(f), "--runtime", "self-host"])

    assert result.exit_code != 0
    assert "JAMJET_RUNTIME_URL" in result.output
