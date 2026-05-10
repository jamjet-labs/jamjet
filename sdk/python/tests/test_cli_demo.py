import json
import re
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


def _redact_tmp_audit_path(stdout: str, tmp_path: Path) -> str:
    """Replace per-test tmp paths in audit lines with a stable token.

    On macOS, ``tmp_path`` resolves to ``/var/...`` while ``Path.cwd()`` after
    ``monkeypatch.chdir`` resolves to ``/private/var/...``. Strip both, plus
    the pytest-N counter that increments each test run.
    """
    redacted = stdout.replace(str(tmp_path), "<TMP>")
    redacted = redacted.replace("/private" + str(tmp_path), "<TMP>")
    # Also strip any pytest-N folders that survived the simple replace.
    redacted = re.sub(r"pytest-of-[^/]+/pytest-\d+/[^/]+", "pytest-tmp", redacted)
    return redacted


def test_unsafe_tool_call_human_output(tmp_path, monkeypatch, snapshot):
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["demo", "unsafe-tool-call"])
    assert result.exit_code == 0
    # Honesty line MUST appear.
    assert "The model is mocked. The enforcement path is real." in result.stdout
    # Snapshot the full output for regression protection.
    assert _redact_tmp_audit_path(result.stdout, tmp_path) == snapshot


def test_unsafe_tool_call_json_output(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["demo", "unsafe-tool-call", "--json"])
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload["decision"] == "BLOCKED"
    assert payload["executed"] is False
    assert payload["tool"] == "database.delete_all_customers"


def test_approval_pauses_when_no_approve_flag(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["demo", "approval"])
    assert result.exit_code == 0
    assert "WAITING_FOR_APPROVAL" in result.stdout
    assert "jamjet demo approval --approve" in result.stdout
    runs = list((tmp_path / ".jamjet-demo" / "runs").glob("approval-*.json"))
    assert len(runs) == 1
    state = json.loads(runs[0].read_text())
    assert state["decision"] == "WAITING_FOR_APPROVAL"
    assert state["executed"] is False


def test_approval_resumes_after_approve_flag(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    runner.invoke(app, ["demo", "approval"])
    runs = list((tmp_path / ".jamjet-demo" / "runs").glob("approval-*.json"))
    run_id = runs[0].stem

    result = runner.invoke(app, ["demo", "approval", "--approve", run_id])
    assert result.exit_code == 0
    assert "Approved" in result.stdout
    assert "Tool executed" in result.stdout

    state = json.loads(runs[0].read_text())
    assert state["decision"] == "APPROVED"
    assert state["executed"] is True
