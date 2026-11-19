"""Tests for `jamjet audit EXECUTION_ID` and the trail it builds.

The fixtures are real engine output (get_execution, get_events and the
approvals listing) captured from two durable runs of the venue agent on the
scripted model: one where pay_deposit needed approval and `tester` approved it
with `jamjet approve --as tester` on a dev-mode engine, and one where
cancel_booking was blocked by `blocked_tools: ["cancel_*"]`.
"""

from __future__ import annotations

import copy
import json
from dataclasses import asdict
from pathlib import Path
from unittest.mock import AsyncMock, patch

import httpx
from typer.testing import CliRunner

from jamjet.cli.audit_trail import build_trail, render_trail
from jamjet.cli.main import app

FIXTURES = Path(__file__).parent / "fixtures" / "audit"
APPROVED_ID = "exec_881197f666254703be3d1c992ef78525"
BLOCKED_ID = "exec_ea628a50de754bfda1698af79c819604"

runner = CliRunner()


def _load(name: str) -> dict:
    return json.loads((FIXTURES / name).read_text(encoding="utf-8"))


def _trail(fixture: dict):
    return build_trail(fixture["execution"], fixture["events"], fixture["approvals"]["decided"])


def _event(fixture: dict, kind: str) -> dict:
    return next(e for e in fixture["events"] if e["kind"]["type"] == kind)


def _fake_client(fixture: dict) -> AsyncMock:
    fake = AsyncMock()
    fake.get_execution = AsyncMock(return_value=fixture["execution"])
    fake.get_events = AsyncMock(return_value={"events": fixture["events"]})
    fake.list_approvals = AsyncMock(return_value=fixture["approvals"])
    fake.__aenter__ = AsyncMock(return_value=fake)
    fake.__aexit__ = AsyncMock(return_value=None)
    return fake


# ── build_trail on the approved run ──────────────────────────────────────────


def test_approved_run_trail_in_order() -> None:
    trail = _trail(_load("approved_run.json"))
    assert [r.step for r in trail.rows] == [
        "model asked for search_venues",
        "ran search_venues",
        "model asked for check_dates",
        "ran check_dates",
        "model asked for pay_deposit",
        "pay_deposit needs approval",
        "approved by tester",
        "ran pay_deposit",
        "final answer",
    ]
    times = [r.time for r in trail.rows]
    assert times == sorted(times)


def test_proposal_shows_the_arguments_as_compact_json() -> None:
    trail = _trail(_load("approved_run.json"))
    pay = next(r for r in trail.rows if r.step == "model asked for pay_deposit")
    assert '{"amount":500,"venue":"Casa do Rio"}' in pay.detail
    assert "call_2_0" in pay.detail


def test_dev_mode_approval_is_marked_not_verified() -> None:
    fixture = _load("approved_run.json")
    trail = _trail(fixture)
    row = next(r for r in trail.rows if r.step == "approved by tester")
    assert "dev mode, name not verified" in row.detail
    assert "Deposit is within the event budget" in row.detail
    assert row.time == _event(fixture, "approval_received")["created_at"]


def test_token_approval_names_the_principal() -> None:
    fixture = copy.deepcopy(_load("approved_run.json"))
    _event(fixture, "approval_received")["kind"]["principal"] = "token:tok_ops_1"
    row = next(r for r in _trail(fixture).rows if r.step == "approved by tester")
    assert "token tok_ops_1" in row.detail
    assert "dev mode" not in row.detail


def test_rejected_decision_reads_rejected_by() -> None:
    fixture = copy.deepcopy(_load("approved_run.json"))
    _event(fixture, "approval_received")["kind"]["decision"] = "rejected"
    steps = [r.step for r in _trail(fixture).rows]
    assert "rejected by tester" in steps
    assert "approved by tester" not in steps


def test_ran_row_is_tied_to_its_call_id() -> None:
    trail = _trail(_load("approved_run.json"))
    ran = next(r for r in trail.rows if r.step == "ran pay_deposit")
    assert ran.detail.startswith("call_2_0")
    assert "Receipt #1" in ran.detail


def test_tool_runs_count_each_result_once() -> None:
    trail = _trail(_load("approved_run.json"))
    assert trail.tool_runs == {"search_venues": 1, "check_dates": 1, "pay_deposit": 1}


def test_final_answer_is_cut_at_80_characters() -> None:
    fixture = _load("approved_run.json")
    answer = fixture["execution"]["current_state"]["last_model_output"]
    assert len(answer) > 80
    row = _trail(fixture).rows[-1]
    assert row.step == "final answer"
    assert row.detail == answer[:80] + "..."


def test_tool_results_seeded_from_an_earlier_session_do_not_count() -> None:
    fixture = copy.deepcopy(_load("approved_run.json"))
    first_turn = _event(fixture, "node_completed")  # __model_0__
    assert first_turn["kind"]["node_id"] == "__model_0__"
    dispatch = next(
        e for e in fixture["events"] if e["kind"]["type"] == "node_completed" and e["kind"]["node_id"] == "__tools_0__"
    )
    seed = dispatch["kind"]["output"]["messages"]  # system, user, assistant call_0_0, tool call_0_0
    fixture["execution"]["initial_input"]["messages"] = seed
    _event(fixture, "workflow_started")["kind"]["initial_input"]["messages"] = seed
    first_turn["kind"]["output"]["tool_calls"] = []
    trail = _trail(fixture)
    assert trail.tool_runs == {"check_dates": 1, "pay_deposit": 1}
    assert "ran search_venues" not in [r.step for r in trail.rows]
    assert "ran check_dates" in [r.step for r in trail.rows]


# ── render_trail ─────────────────────────────────────────────────────────────


def test_render_header_and_footer() -> None:
    fixture = _load("approved_run.json")
    text = render_trail(_trail(fixture), fixture["execution"])
    head = text.splitlines()[:4]
    assert any(APPROVED_ID in line for line in head)
    assert any("venue-booker" in line and "completed" in line for line in head)
    assert "Source: the engine's event log and approval records. Times are UTC." in head
    assert "pay_deposit ran 1 time, approved by tester at 02:14:01" in text
    assert "search_venues ran 1 time\n" in text + "\n"
    assert "search_venues ran 1 time, approved" not in text


def test_render_escapes_terminal_controls() -> None:
    fixture = copy.deepcopy(_load("approved_run.json"))
    _event(fixture, "approval_received")["kind"]["comment"] = "ok\x1b[2J\u202e"
    text = render_trail(_trail(fixture), fixture["execution"])
    assert "\x1b" not in text
    assert "\u202e" not in text
    assert "ok\\x1b[2J\\u202e" in text


# ── build_trail on the blocked run ───────────────────────────────────────────


def test_blocked_run_shows_the_block_and_nothing_ran() -> None:
    fixture = _load("blocked_run.json")
    trail = _trail(fixture)
    steps = [r.step for r in trail.rows]
    assert steps.index("model asked for cancel_booking") < steps.index("cancel_booking blocked")
    blocked = next(r for r in trail.rows if r.step == "cancel_booking blocked")
    assert blocked.detail == _event(fixture, "policy_violation")["kind"]["rule"]
    assert "cancel_booking" not in trail.tool_runs
    assert not any(s.startswith("ran ") for s in steps)
    assert steps[-1] == "run failed: " + _event(fixture, "workflow_failed")["kind"]["error"]


def test_blocked_run_footer_says_no_tool_ran() -> None:
    fixture = _load("blocked_run.json")
    text = render_trail(_trail(fixture), fixture["execution"])
    assert BLOCKED_ID in text
    assert "No tool ran." in text


# ── the command ──────────────────────────────────────────────────────────────


def test_audit_command_prints_the_trail() -> None:
    fixture = _load("approved_run.json")
    fake = _fake_client(fixture)
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["audit", APPROVED_ID])
    assert result.exit_code == 0, result.output
    assert "approved by tester" in result.output
    assert "pay_deposit ran 1 time, approved by tester at 02:14:01" in result.output
    fake.get_execution.assert_awaited_once_with(APPROVED_ID)
    fake.get_events.assert_awaited_once_with(APPROVED_ID)
    fake.list_approvals.assert_awaited_once_with(APPROVED_ID)


def test_audit_json_matches_build_trail() -> None:
    fixture = _load("approved_run.json")
    with patch("jamjet.cli.main._client", return_value=_fake_client(fixture)):
        result = runner.invoke(app, ["audit", APPROVED_ID, "--json"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    trail = _trail(fixture)
    assert data["rows"] == [asdict(r) for r in trail.rows]
    assert data["tool_runs"] == trail.tool_runs
    assert data["execution"] == fixture["execution"]


def test_audit_unknown_execution_exits_1() -> None:
    fake = _fake_client(_load("approved_run.json"))
    url = "http://localhost:7700/executions/exec_x"
    request = httpx.Request("GET", url)
    fake.get_execution = AsyncMock(
        side_effect=httpx.HTTPStatusError("404", request=request, response=httpx.Response(404, request=request))
    )
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["audit", "exec_x"])
    assert result.exit_code == 1
    assert "No execution exec_x on http://localhost:7700." in result.output


def test_audit_uses_the_runtime_option() -> None:
    fake = _fake_client(_load("blocked_run.json"))
    with patch("jamjet.cli.main._client", return_value=fake) as made:
        result = runner.invoke(app, ["audit", BLOCKED_ID, "--runtime", "http://127.0.0.1:7711"])
    assert result.exit_code == 0, result.output
    made.assert_called_once_with("http://127.0.0.1:7711")
    assert "cancel_booking blocked" in result.output
