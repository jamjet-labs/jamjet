from unittest.mock import AsyncMock, patch

from typer.testing import CliRunner

from jamjet.cli.main import app

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
    schedule: { cron: "0 9 * * *" }
"""


def test_deploy_registers_workflows_and_cron(tmp_path):
    f = tmp_path / "fleet.yaml"
    f.write_text(FLEET)

    fake = AsyncMock()
    fake.create_workflow = AsyncMock(return_value={"workflow_id": "researcher", "version": "0.1.0"})
    fake.create_cron_job = AsyncMock(return_value={"name": "researcher", "next_run_at": "2026-06-06T09:00:00Z"})
    fake.__aenter__ = AsyncMock(return_value=fake)
    fake.__aexit__ = AsyncMock(return_value=None)

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["deploy", str(f)])

    assert result.exit_code == 0, result.output
    fake.create_workflow.assert_awaited_once()
    fake.create_cron_job.assert_awaited_once()
    assert fake.create_cron_job.await_args.kwargs["name"] == "researcher"


MULTI = (
    FLEET
    + """
  reconciler:
    strategy: react
    goal: reconcile
"""
)


def test_run_multi_unit_requires_selector(tmp_path):
    f = tmp_path / "fleet.yaml"
    f.write_text(MULTI)
    fake = AsyncMock()
    fake.__aenter__ = AsyncMock(return_value=fake)
    fake.__aexit__ = AsyncMock(return_value=None)
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["run", str(f)])
    assert result.exit_code != 0
    assert "researcher" in result.output and "reconciler" in result.output


def test_run_multi_unit_with_selector_starts_one(tmp_path):
    f = tmp_path / "fleet.yaml"
    f.write_text(MULTI)
    fake = AsyncMock()
    fake.create_workflow = AsyncMock(return_value={})
    fake.start_execution = AsyncMock(return_value={"execution_id": "exec_abc"})
    fake.get_execution = AsyncMock(return_value={"status": "completed", "current_state": {}})
    fake.__aenter__ = AsyncMock(return_value=fake)
    fake.__aexit__ = AsyncMock(return_value=None)
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["run", str(f), "reconciler", "--no-follow"])
    assert result.exit_code == 0, result.output
    assert fake.create_workflow.await_count == 2  # both units registered
    assert fake.start_execution.await_args.kwargs["workflow_id"] == "reconciler"


def test_run_single_unit_no_selector_auto_runs(tmp_path):
    f = tmp_path / "solo.yaml"
    f.write_text(FLEET)  # FLEET has exactly one unit: researcher
    fake = AsyncMock()
    fake.create_workflow = AsyncMock(return_value={})
    fake.start_execution = AsyncMock(return_value={"execution_id": "exec_solo"})
    fake.get_execution = AsyncMock(return_value={"status": "completed", "current_state": {}})
    fake.__aenter__ = AsyncMock(return_value=fake)
    fake.__aexit__ = AsyncMock(return_value=None)
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["run", str(f), "--no-follow"])
    assert result.exit_code == 0, result.output
    assert fake.start_execution.await_args.kwargs["workflow_id"] == "researcher"


def test_run_unknown_unit_errors(tmp_path):
    f = tmp_path / "fleet.yaml"
    f.write_text(MULTI)
    fake = AsyncMock()
    fake.create_workflow = AsyncMock(return_value={})
    fake.__aenter__ = AsyncMock(return_value=fake)
    fake.__aexit__ = AsyncMock(return_value=None)
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["run", str(f), "ghost"])
    assert result.exit_code != 0
    assert "ghost" in result.output
    assert "researcher" in result.output and "reconciler" in result.output
