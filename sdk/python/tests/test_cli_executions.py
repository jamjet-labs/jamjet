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
    for col in ("execution", "agent", "status", "started (local)"):
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


def _status_error(code: int):
    import httpx

    request = httpx.Request("GET", "http://x/executions/exec_gone/approvals")
    return httpx.HTTPStatusError("gone", request=request, response=httpx.Response(code, request=request))


def test_approvals_without_id_skips_an_execution_that_vanished() -> None:
    gone = {**NEW, "execution_id": "exec_gone"}
    fake = _client([NEW, gone], {"exec_new": {"pending": _pending(), "decided": []}})
    inner = fake.list_approvals.side_effect

    def fetch(eid):
        if eid == "exec_gone":
            raise _status_error(404)
        return inner(eid)

    fake.list_approvals = AsyncMock(side_effect=fetch)
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals"])
    assert result.exit_code == 0
    assert "jamjet approve exec_new" in result.output
    assert "skipped exec_gone" in result.output
    assert "404" in result.output


def test_executions_unreachable_engine_prints_one_line() -> None:
    import httpx

    fake = _client([])
    fake.list_executions = AsyncMock(side_effect=httpx.ConnectError("refused"))
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["executions", "--runtime", "http://nowhere:7700"])
    assert result.exit_code == 1
    assert "http://nowhere:7700" in result.output
    assert "Traceback" not in result.output


def test_approvals_without_id_unreachable_engine_prints_one_line() -> None:
    import httpx

    fake = _client([])
    fake.list_executions = AsyncMock(side_effect=httpx.ConnectError("refused"))
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "--runtime", "http://nowhere:7700"])
    assert result.exit_code == 1
    assert "http://nowhere:7700" in result.output
    assert "Traceback" not in result.output


UUID = "0b8e3c1a-2f4d-4e6a-9b7c-1d2e3f4a5b6c"
DISPLAY = "exec_0b8e3c1a2f4d4e6a9b7c1d2e3f4a5b6c"


def test_approvals_without_id_finds_a_running_run_with_a_pending_approval() -> None:
    running = {**NEW, "execution_id": "exec_run", "status": "running"}
    idle = {**NEW, "execution_id": "exec_idle", "status": "running"}
    fake = _client([running, idle], {"exec_run": {"pending": _pending(), "decided": []}})

    async def by_status(status=None, limit=50):
        return {"executions": [running, idle] if status == "running" else []}

    fake.list_executions = AsyncMock(side_effect=by_status)
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals"])
    assert result.exit_code == 0
    assert "jamjet approve exec_run" in result.output
    assert "exec_idle" not in result.output
    assert "No approvals are waiting." not in result.output


def test_executions_table_shows_the_exec_form_but_json_stays_raw() -> None:
    item = {**NEW, "execution_id": UUID}
    with patch("jamjet.cli.main._client", return_value=_client([item])):
        table = runner.invoke(app, ["executions"])
        raw = runner.invoke(app, ["executions", "--json"])
    assert DISPLAY in table.output
    assert UUID not in table.output
    assert json.loads(raw.output)[0]["execution_id"] == UUID


def test_approvals_without_id_prints_the_exec_form_in_the_command() -> None:
    item = {**NEW, "execution_id": UUID}
    fake = _client([item], {UUID: {"pending": _pending(), "decided": []}})
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals"])
    assert f"jamjet approve {DISPLAY}" in result.output
    assert UUID not in result.output
