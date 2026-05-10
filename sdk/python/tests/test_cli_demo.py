import json
from pathlib import Path

from typer.testing import CliRunner

from jamjet.cli._demo_audit import DemoAuditEvent, write_audit_event
from jamjet.cli.main import app

runner = CliRunner()


def test_demo_help_lists_four_subcommands():
    result = runner.invoke(app, ["demo", "--help"])
    assert result.exit_code == 0
    assert "unsafe-tool-call" in result.stdout
    assert "approval" in result.stdout
    assert "budget-cap" in result.stdout
    assert "mcp-tool-policy" in result.stdout


def test_audit_event_writes_json_under_run_dir(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    event = DemoAuditEvent(
        run_id="demo-run-001",
        demo="unsafe-tool-call",
        decision="BLOCKED",
        tool="database.delete_all_customers",
        rule="*delete*",
        executed=False,
    )
    path = write_audit_event(event)

    assert path == tmp_path / ".jamjet-demo" / "runs" / "demo-run-001.json"
    assert path.exists()

    written = json.loads(path.read_text())
    assert written["run_id"] == "demo-run-001"
    assert written["decision"] == "BLOCKED"
    assert written["executed"] is False


from jamjet.cli._demo_agent import DeterministicDemoAgent


def test_demo_agent_is_explicit_about_being_mocked():
    agent = DeterministicDemoAgent(scenario="unsafe-tool-call")
    plan = agent.plan_tool_calls()
    assert plan, "agent must propose at least one tool call"
    # Honesty: agent identifies as mocked, not as a real model.
    assert "mock" in agent.name().lower() or "deterministic" in agent.name().lower()
