"""Verifies the Python SDK demo audit output conforms to the v1 audit schema."""

import json
from pathlib import Path

from typer.testing import CliRunner

from jamjet.cli.main import app

runner = CliRunner()

# Subset of the v1 schema required keys (see jamjet-policy/conformance/audit-event-shape.json).
V1_REQUIRED = {"ts", "run_id", "adapter", "host", "tool", "decision", "executed", "schema_version"}


def _run_demo_json(cmd: str, tmp_path: Path, monkeypatch) -> dict:
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["demo", cmd, "--json"])
    assert result.exit_code == 0, result.output
    return json.loads(result.stdout)


def test_unsafe_tool_call_emits_v1_required_fields(tmp_path, monkeypatch) -> None:
    payload = _run_demo_json("unsafe-tool-call", tmp_path, monkeypatch)
    missing = V1_REQUIRED - set(payload.keys())
    assert not missing, f"missing v1 required fields: {missing}"
    assert payload["adapter"] == "python-sdk"
    assert payload["host"] == "python"
    assert payload["schema_version"] == 1
    assert payload["policy_version"] == "1"
    assert payload["decision"] == "BLOCKED"
    assert payload["rule_kind"] == "block"


def test_approval_emits_v1_schema(tmp_path, monkeypatch) -> None:
    payload = _run_demo_json("approval", tmp_path, monkeypatch)
    assert {"ts", "adapter", "host", "schema_version"} <= set(payload.keys())
    assert payload["adapter"] == "python-sdk"
    assert payload["decision"] == "WAITING_FOR_APPROVAL"
    assert payload["rule_kind"] == "require_approval"


def test_budget_cap_emits_v1_schema(tmp_path, monkeypatch) -> None:
    payload = _run_demo_json("budget-cap", tmp_path, monkeypatch)
    assert {"ts", "adapter", "host", "schema_version"} <= set(payload.keys())
    assert payload["decision"] == "BUDGET_EXCEEDED"


def test_mcp_tool_policy_emits_v1_schema(tmp_path, monkeypatch) -> None:
    payload = _run_demo_json("mcp-tool-policy", tmp_path, monkeypatch)
    assert {"ts", "adapter", "host", "schema_version"} <= set(payload.keys())
    assert payload["adapter"] == "python-sdk"
    assert payload["decision"] == "BLOCKED"


def test_backward_compat_keeps_timestamp_alias(tmp_path, monkeypatch) -> None:
    """The pre-v1 `timestamp` field is preserved as an alias for `ts` so existing
    consumers (e.g. ad-hoc scripts written against jamjet 0.8.1) keep working."""
    payload = _run_demo_json("unsafe-tool-call", tmp_path, monkeypatch)
    assert payload.get("timestamp") == payload["ts"]
