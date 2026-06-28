"""
Tests for trajectory eval wiring + replay-regression diff -- T5-4.

Coverage:
- EvalRow.expected_trajectory: default None; loader parses it (JSONL / JSON / YAML)
- EvalRunner (durable, mocked client): a case whose events match expected_trajectory
  scores the trajectory PASS and the case passes
- EvalRunner: a case whose agent uses the WRONG tools scores the trajectory FAIL and
  the case FAILS even though the output scorer matched (output AND trajectory)
- EvalRow with no expected_trajectory -> output-only, unchanged (no trajectory scorer)
- AgentEvalRunner (in-process, mocked Agent): trajectory built from tool_calls
- diff_trajectories: an added tool is flagged as a regression; reorder + identical
- jamjet eval trajectory-diff CLI: detects the regression and exits 1
"""

from __future__ import annotations

import json
from unittest.mock import AsyncMock, MagicMock

import pytest

from jamjet.eval.dataset import EvalDataset, EvalRow
from jamjet.eval.runner import AgentEvalRunner, EvalRunner
from jamjet.eval.scorers import AssertionScorer
from jamjet.eval.trajectory import Trajectory, diff_trajectories

# ── Helpers ───────────────────────────────────────────────────────────────────


def _tool_event(tool: str, node_id: str = "n1") -> dict:
    return {"kind": {"type": "tool_called", "node_id": node_id, "tool": tool}}


def _model_turn(tool: str, *, node_id: str = "model", seq: int = 0) -> dict:
    """A model-node ``NodeCompleted`` event in the SHAPE a real durable run emits.

    This is what ``client.get_events`` actually returns for a durable agent run
    (per ``runtime/workers/src/executors/model_node.rs``): the model's requested
    tool calls live on ``kind.output.tool_calls`` as ``{id, name, arguments}``,
    NOT in a (never-emitted) ``tool_called`` event. The runner reconstructs the
    trajectory from these.
    """
    tc = [{"id": f"call_{seq}", "name": tool, "arguments": {}}]
    return {
        "id": f"evt-{seq}",
        "sequence": seq,
        "kind": {
            "type": "node_completed",
            "node_id": node_id,
            "output": {
                "content": "",
                "model": "anthropic/claude-sonnet-4-6",
                "finish_reason": "tool_calls",
                "tool_calls": tc,
            },
            "state_patch": {"last_model_tool_calls": tc, "last_model_finish_reason": "tool_calls"},
            "duration_ms": 5,
        },
    }


def _mock_client(*, output: dict, events: list[dict]) -> MagicMock:
    client = MagicMock()
    client.__aenter__ = AsyncMock(return_value=client)
    client.__aexit__ = AsyncMock(return_value=None)
    client.start_execution = AsyncMock(return_value={"execution_id": "exec-1"})
    client.get_execution = AsyncMock(return_value={"status": "completed", "output": output})
    client.get_events = AsyncMock(return_value={"events": events})
    return client


# ── EvalRow.expected_trajectory + loader ──────────────────────────────────────


def test_evalrow_expected_trajectory_default_none():
    row = EvalRow(id="r1", input={"q": "x"})
    assert row.expected_trajectory is None


def test_loader_parses_expected_trajectory_jsonl(tmp_path):
    path = tmp_path / "data.jsonl"
    path.write_text(
        '{"id": "q1", "input": {"q": "a"}, "expected_trajectory": {"tool_sequence": ["search", "calc"]}}\n'
        '{"id": "q2", "input": {"q": "b"}}\n'
    )
    ds = EvalDataset.from_file(path)
    assert ds[0].expected_trajectory == {"tool_sequence": ["search", "calc"]}
    assert ds[1].expected_trajectory is None  # backward compatible


def test_loader_parses_expected_trajectory_json(tmp_path):
    path = tmp_path / "data.json"
    path.write_text(
        json.dumps(
            [
                {"input": {"q": "a"}, "expected_trajectory": {"used_tool": "search"}},
            ]
        )
    )
    ds = EvalDataset.from_file(path)
    assert ds[0].expected_trajectory == {"used_tool": "search"}


def test_loader_parses_expected_trajectory_yaml(tmp_path):
    path = tmp_path / "data.yaml"
    path.write_text("- input:\n    q: a\n  expected_trajectory:\n    tool_sequence: [search, calc]\n")
    ds = EvalDataset.from_file(path)
    assert ds[0].expected_trajectory == {"tool_sequence": ["search", "calc"]}


# ── EvalRunner: trajectory scored IN ADDITION to output ───────────────────────


@pytest.mark.asyncio
async def test_runner_trajectory_pass(monkeypatch):
    """A case whose events match expected_trajectory -> trajectory PASS, case passes.

    Events are the REAL durable shape (model ``node_completed`` with
    ``output.tool_calls``), so this also guards C1: the runner reconstructs the
    trajectory from what the engine actually emits, not from ``tool_called``.
    """
    client = _mock_client(
        output={"answer": "42"},
        events=[_model_turn("search", node_id="m1", seq=1), _model_turn("calc", node_id="m2", seq=2)],
    )
    import jamjet.client as client_mod

    monkeypatch.setattr(client_mod, "JamjetClient", lambda **kw: client)

    row = EvalRow(
        id="r1",
        input={"q": "hi"},
        expected_trajectory={"tool_sequence": ["search", "calc"]},
    )
    runner = EvalRunner("wf", [AssertionScorer(checks=["'answer' in output"])], poll_interval_s=0.0)
    [result] = await runner.run(EvalDataset([row]))

    assert result.passed is True
    # output AND trajectory scorers both present
    names = {s.scorer for s in result.scorers}
    assert "assertion" in names
    assert "trajectory" in names
    traj_scorer = next(s for s in result.scorers if s.scorer == "trajectory")
    assert traj_scorer.passed is True
    assert result.trajectory == ["search", "calc"]


@pytest.mark.asyncio
async def test_runner_trajectory_fail_even_if_output_matches(monkeypatch):
    """Wrong tools -> trajectory FAIL and the case FAILS even though output matched."""
    # Output matches the assertion, but the agent used the wrong tools.
    client = _mock_client(
        output={"answer": "42"},
        events=[_model_turn("search", node_id="m1", seq=1), _model_turn("web", node_id="m2", seq=2)],
    )
    import jamjet.client as client_mod

    monkeypatch.setattr(client_mod, "JamjetClient", lambda **kw: client)

    row = EvalRow(
        id="r1",
        input={"q": "hi"},
        expected_trajectory={"tool_sequence": ["search", "calc"]},  # expects calc, got web
    )
    runner = EvalRunner("wf", [AssertionScorer(checks=["'answer' in output"])], poll_interval_s=0.0)
    [result] = await runner.run(EvalDataset([row]))

    # Output scorer passes ...
    output_scorer = next(s for s in result.scorers if s.scorer == "assertion")
    assert output_scorer.passed is True
    # ... trajectory scorer fails ...
    traj_scorer = next(s for s in result.scorers if s.scorer == "trajectory")
    assert traj_scorer.passed is False
    # ... so the overall case fails (output AND trajectory).
    assert result.passed is False


@pytest.mark.asyncio
async def test_runner_no_expected_trajectory_is_output_only(monkeypatch):
    """expected_trajectory=None -> output-only, unchanged (no trajectory scorer)."""
    client = _mock_client(output={"answer": "42"}, events=[_model_turn("search", node_id="m1", seq=1)])
    import jamjet.client as client_mod

    monkeypatch.setattr(client_mod, "JamjetClient", lambda **kw: client)

    row = EvalRow(id="r1", input={"q": "hi"})  # no expected_trajectory
    runner = EvalRunner("wf", [AssertionScorer(checks=["'answer' in output"])], poll_interval_s=0.0)
    [result] = await runner.run(EvalDataset([row]))

    assert result.passed is True
    names = {s.scorer for s in result.scorers}
    assert names == {"assertion"}  # NO trajectory scorer appended
    # trajectory tool_sequence still captured for inspection / regression diffing
    assert result.trajectory == ["search"]


# ── AgentEvalRunner: trajectory from in-process tool_calls ─────────────────────


@pytest.mark.asyncio
async def test_agent_runner_trajectory_from_tool_calls(monkeypatch):
    """AgentEvalRunner builds the trajectory from AgentResult.tool_calls."""

    class _FakeAgentResult:
        output = {"answer": "ok"}
        tool_calls = [
            {"tool": "search", "input": {}, "output": "", "duration_us": 0},
            {"tool": "calc", "input": {}, "output": "", "duration_us": 0},
        ]

    class _FakeAgent:
        def __init__(self, *a, **k):
            pass

        async def run(self, task):
            return _FakeAgentResult()

    import jamjet.agents.agent as agent_mod

    monkeypatch.setattr(agent_mod, "Agent", _FakeAgent)

    row = EvalRow(
        id="r1",
        input={"task": "do it"},
        expected_trajectory={"tool_sequence": ["search", "calc"]},
    )
    runner = AgentEvalRunner([AssertionScorer(checks=["'answer' in output"])])
    [result] = await runner.run(EvalDataset([row]))

    assert result.passed is True
    traj_scorer = next(s for s in result.scorers if s.scorer == "trajectory")
    assert traj_scorer.passed is True
    assert result.trajectory == ["search", "calc"]


@pytest.mark.asyncio
async def test_agent_runner_trajectory_fail(monkeypatch):
    """AgentEvalRunner trajectory failure fails the case even when output matches."""

    class _FakeAgentResult:
        output = {"answer": "ok"}
        tool_calls = [{"tool": "search", "input": {}, "output": "", "duration_us": 0}]  # missing calc

    class _FakeAgent:
        def __init__(self, *a, **k):
            pass

        async def run(self, task):
            return _FakeAgentResult()

    import jamjet.agents.agent as agent_mod

    monkeypatch.setattr(agent_mod, "Agent", _FakeAgent)

    row = EvalRow(
        id="r1",
        input={"task": "do it"},
        expected_trajectory={"tool_sequence": ["search", "calc"]},
    )
    runner = AgentEvalRunner([AssertionScorer(checks=["'answer' in output"])])
    [result] = await runner.run(EvalDataset([row]))

    assert result.passed is False


# ── Replay-regression trajectory diff ─────────────────────────────────────────


def test_diff_flags_added_tool_regression():
    """before [search, calc] vs after [search, web, calc] -> 'web' added (regression)."""
    before = Trajectory.from_events([_tool_event("search"), _tool_event("calc")])
    after = Trajectory.from_events([_tool_event("search"), _tool_event("web"), _tool_event("calc")])

    diff = diff_trajectories(before, after)

    assert diff.changed is True
    assert diff.added == ["web"]
    assert diff.removed == []
    assert diff.reordered is False
    assert diff.before == ["search", "calc"]
    assert diff.after == ["search", "web", "calc"]


def test_diff_flags_removed_tool():
    before = Trajectory.from_events([_tool_event("search"), _tool_event("web"), _tool_event("calc")])
    after = Trajectory.from_events([_tool_event("search"), _tool_event("calc")])
    diff = diff_trajectories(before, after)
    assert diff.changed is True
    assert diff.removed == ["web"]
    assert diff.added == []


def test_diff_detects_reorder():
    before = Trajectory.from_events([_tool_event("a"), _tool_event("b")])
    after = Trajectory.from_events([_tool_event("b"), _tool_event("a")])
    diff = diff_trajectories(before, after)
    assert diff.changed is True
    assert diff.reordered is True
    assert diff.added == []
    assert diff.removed == []


def test_diff_identical_no_change():
    before = Trajectory.from_events([_tool_event("search"), _tool_event("calc")])
    after = Trajectory.from_events([_tool_event("search"), _tool_event("calc")])
    diff = diff_trajectories(before, after)
    assert diff.changed is False
    assert diff.added == []
    assert diff.removed == []
    assert diff.reordered is False


def test_diff_render_mentions_added():
    before = Trajectory.from_tool_sequence(["search", "calc"])
    after = Trajectory.from_tool_sequence(["search", "web", "calc"])
    text = diff_trajectories(before, after).render()
    assert "web" in text
    assert "CHANGED" in text


# ── CLI: jamjet eval trajectory-diff ──────────────────────────────────────────


def test_cli_trajectory_diff_detects_regression(tmp_path):
    from typer.testing import CliRunner

    from jamjet.cli.main import app

    before = tmp_path / "before.json"
    after = tmp_path / "after.json"
    before.write_text(json.dumps({"events": [_tool_event("search"), _tool_event("calc")]}))
    after.write_text(json.dumps({"events": [_tool_event("search"), _tool_event("web"), _tool_event("calc")]}))

    runner = CliRunner()
    result = runner.invoke(app, ["eval", "trajectory-diff", str(before), str(after)])

    assert result.exit_code == 1  # fail-on-change is the default (regression gate)
    assert "web" in result.stdout
    assert "REGRESSION" in result.stdout


def test_cli_trajectory_diff_no_change_exits_zero(tmp_path):
    from typer.testing import CliRunner

    from jamjet.cli.main import app

    before = tmp_path / "before.json"
    after = tmp_path / "after.json"
    same = json.dumps({"events": [_tool_event("search"), _tool_event("calc")]})
    before.write_text(same)
    after.write_text(same)

    runner = CliRunner()
    result = runner.invoke(app, ["eval", "trajectory-diff", str(before), str(after)])

    assert result.exit_code == 0
    assert "unchanged" in result.stdout.lower()


def test_cli_trajectory_diff_json_format(tmp_path):
    from typer.testing import CliRunner

    from jamjet.cli.main import app

    before = tmp_path / "before.json"
    after = tmp_path / "after.json"
    before.write_text(json.dumps([_tool_event("search")]))  # raw event list shape
    after.write_text(json.dumps([_tool_event("search"), _tool_event("web")]))

    runner = CliRunner()
    result = runner.invoke(
        app, ["eval", "trajectory-diff", str(before), str(after), "--no-fail-on-change", "--format", "json"]
    )

    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload["added"] == ["web"]
    assert payload["changed"] is True


# ── CodeRabbit #3: reject eval-results rows that carry no real trajectory ──────


def test_load_trajectory_rejects_results_row_without_trajectory(tmp_path):
    """An eval-results row with row_id but NO trajectory field fails fast with a
    clear error -- it must NOT default to [] and forge a bogus 'no tools' baseline."""
    from jamjet.cli.main import _load_trajectory_from_file

    p = tmp_path / "results.json"
    # Old/malformed `jamjet eval run --output` artifact: rows lack `trajectory`.
    p.write_text(json.dumps([{"row_id": "q1", "passed": True}, {"row_id": "q2", "passed": False}]))

    with pytest.raises(ValueError, match="trajectory"):
        _load_trajectory_from_file(str(p), row_id="q1")


def test_load_trajectory_accepts_explicit_empty_trajectory(tmp_path):
    """An explicit empty trajectory (the agent legitimately used no tools) is
    accepted -- only a MISSING trajectory field is an error."""
    from jamjet.cli.main import _load_trajectory_from_file

    p = tmp_path / "results.json"
    p.write_text(json.dumps([{"row_id": "q1", "trajectory": []}]))

    traj = _load_trajectory_from_file(str(p), row_id="q1")
    assert traj.tool_sequence == []


def test_cli_trajectory_diff_rejects_results_without_trajectory(tmp_path):
    """End-to-end: diffing a results file whose rows lack `trajectory` exits 1 with
    an error, NOT a bogus 'OK / unchanged' empty-trajectory diff."""
    from typer.testing import CliRunner

    from jamjet.cli.main import app

    before = tmp_path / "before.json"
    after = tmp_path / "after.json"
    rows = json.dumps([{"row_id": "q1", "passed": True}])
    before.write_text(rows)
    after.write_text(rows)

    runner = CliRunner()
    result = runner.invoke(app, ["eval", "trajectory-diff", str(before), str(after), "--row-id", "q1"])

    assert result.exit_code == 1
    assert "Error" in result.stdout
    # Must not have produced a bogus baseline diff.
    assert "OK:" not in result.stdout
    assert "unchanged" not in result.stdout.lower()
