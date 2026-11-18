"""Tests for `jamjet executions` and `jamjet approvals` with no execution id."""

from __future__ import annotations

import json
from unittest.mock import AsyncMock, patch

from typer.testing import CliRunner

from jamjet.cli.main import app

runner = CliRunner()

OLD = {
    "execution_id": "exec_old",
    "workflow_id": "venue_agent",
    "status": "completed",
    "started_at": "2026-11-17T09:00:00Z",
}
NEW = {
    "execution_id": "exec_new",
    "workflow_id": "venue_agent",
    "status": "paused",
    "started_at": "2026-11-18T12:00:00Z",
}


def _client(executions: list, approvals: dict | None = None):
    fake = AsyncMock()
    fake.list_executions = AsyncMock(return_value={"executions": executions})
    fake.list_approvals = AsyncMock(side_effect=lambda eid: (approvals or {}).get(eid, {"pending": [], "decided": []}))
    fake.__aenter__ = AsyncMock(return_value=fake)
    fake.__aexit__ = AsyncMock(return_value=None)
    return fake


def test_executions_lists_newest_first() -> None:
    fake = _client([OLD, NEW])
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["executions"])
    assert result.exit_code == 0
    assert result.output.index("exec_new") < result.output.index("exec_old")
    for col in ("execution", "agent", "status", "started"):
        assert col in result.output
    assert "venue_agent" in result.output


def test_executions_empty() -> None:
    with patch("jamjet.cli.main._client", return_value=_client([])):
        result = runner.invoke(app, ["executions"])
    assert result.exit_code == 0
    assert "No executions yet." in result.output


def test_executions_json_prints_items_unchanged() -> None:
    with patch("jamjet.cli.main._client", return_value=_client([OLD, NEW])):
        result = runner.invoke(app, ["executions", "--json"])
    assert result.exit_code == 0
    assert json.loads(result.output) == [OLD, NEW]


def test_executions_passes_status_and_limit() -> None:
    fake = _client([NEW])
    with patch("jamjet.cli.main._client", return_value=fake):
        runner.invoke(app, ["executions", "--status", "paused", "--limit", "5"])
    fake.list_executions.assert_awaited_once_with(status="paused", limit=5)


def _pending() -> list:
    return [
        {
            "node_id": "pay_deposit",
            "tool_name": "pay_deposit",
            "approver": "ops",
            "sequence": 3,
            "context": {
                "calls_hash": "sha256:abc123",
                "calls": [{"name": "pay_deposit", "arguments": {"amount": 500}}],
            },
        }
    ]


def test_approvals_without_id_lists_waiting_runs() -> None:
    fake = _client([OLD, NEW], {"exec_new": {"pending": _pending(), "decided": []}})
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals"])
    assert result.exit_code == 0
    assert "exec_new" in result.output
    assert "exec_old" not in result.output
    assert "pay_deposit" in result.output
    assert "jamjet approve exec_new" in result.output
    assert "sha256:abc123" in result.output
    assert fake.list_executions.await_args.kwargs["status"] == "paused"


def test_approvals_without_id_none_waiting() -> None:
    with patch("jamjet.cli.main._client", return_value=_client([NEW])):
        result = runner.invoke(app, ["approvals"])
    assert result.exit_code == 0
    assert "No approvals are waiting." in result.output


def test_approvals_without_id_pending_without_digest_points_to_the_detail_view() -> None:
    pending = [{"node_id": "pay_deposit", "context": {}}]
    fake = _client([NEW], {"exec_new": {"pending": pending, "decided": []}})
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals"])
    assert result.exit_code == 0
    assert "exec_new" in result.output
    assert "jamjet approvals exec_new" in result.output
    assert "jamjet approve exec_new" not in result.output


def test_executions_table_shows_start_time_to_the_second() -> None:
    with patch("jamjet.cli.main._client", return_value=_client([NEW])):
        result = runner.invoke(app, ["executions"])
    assert "2026-11-18" in result.output
