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
