"""Tests for the --output json flag on `jamjet run`."""

from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from jamjet.cli.main import OutputFormat, app

runner = CliRunner()


@pytest.fixture()
def fake_client(monkeypatch: pytest.MonkeyPatch):
    """Shared mock client that avoids needing a running runtime."""

    class FakeClient:
        def __init__(self, *a, **kw):
            pass

        async def __aenter__(self):
            return self

        async def __aexit__(self, *a):
            pass

        async def start_execution(self, **kw):
            return {"execution_id": "exec_test123"}

        async def get_execution(self, eid):
            return {"status": "completed", "output": {"result": "ok"}}

        async def get_events(self, eid):
            return {"events": []}

    monkeypatch.setattr("jamjet.cli.main._client", lambda runtime: FakeClient())
    return FakeClient


class TestOutputFormatEnum:
    """OutputFormat enum validates allowed values."""

    def test_valid_values(self) -> None:
        assert OutputFormat("text") is OutputFormat.text
        assert OutputFormat("json") is OutputFormat.json

    def test_invalid_value_rejected(self) -> None:
        with pytest.raises(ValueError):
            OutputFormat("xml")

    def test_enum_members(self) -> None:
        assert set(OutputFormat.__members__) == {"text", "json"}

    def test_invalid_output_format_via_cli(self) -> None:
        result = runner.invoke(app, ["run", "test-wf", "--output", "xml"])
        assert result.exit_code != 0
        output = result.stdout + (result.stderr or "")
        assert "xml" in output or "Invalid value" in output or result.exit_code == 2


class TestJsonOutput:
    """JSON output mode produces valid, compact JSON with expected keys."""

    def test_json_output_is_valid_json(self, fake_client) -> None:
        """Smoke test: --output json should produce parseable JSON (mocked)."""
        result = runner.invoke(app, ["run", "test-wf", "--output", "json", "--runtime", "http://fake:7700"])

        # The output should be valid JSON
        stdout = result.stdout.strip()
        stderr = result.stderr if hasattr(result, "stderr") else "N/A"
        assert stdout, f"Expected JSON output, got empty stdout. stderr: {stderr}"
        parsed = json.loads(stdout)
        assert "execution_id" in parsed
        assert "final_state" in parsed
        assert "steps_executed" in parsed
        assert "total_duration_us" in parsed
        assert "events" in parsed

    def test_json_output_is_compact(self, fake_client) -> None:
        """JSON output should be compact (no indentation)."""
        result = runner.invoke(app, ["run", "test-wf", "--output", "json", "--runtime", "http://fake:7700"])
        stdout = result.stdout.strip()
        # Compact JSON should be a single line
        assert "\n" not in stdout, "JSON output should be compact (single line)"


class TestJsonOutputSuppressesRich:
    """JSON mode should not emit Rich/ANSI formatting."""

    def test_no_ansi_in_json_mode(self, fake_client) -> None:
        result = runner.invoke(app, ["run", "test-wf", "-o", "json", "--runtime", "http://fake:7700"])
        stdout = result.stdout.strip()
        # No ANSI escape codes
        assert "\033[" not in stdout, "JSON output should not contain ANSI escape codes"
        assert "\x1b[" not in stdout, "JSON output should not contain ANSI escape codes"
