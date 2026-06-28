"""
Tests for TrajectoryScorer and Trajectory -- T5-3.

Coverage:
- Trajectory.from_agent_result + from_events produce the same tool sequence
- TrajectoryScorer with tool_sequence -> PASS on match, FAIL on missing/wrong order
- used_tool / did_not_use / max_turns duals (positive and negative)
- Per-assertion breakdown (which assertions passed/failed)
- DETERMINISM: same trajectory + same spec -> identical result
- LLM judge is OFF by default (no model call unless judge=True)
"""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import pytest

from jamjet.eval.trajectory import (
    Trajectory,
    TrajectoryAssertionResult,
    TrajectoryResult,
    TrajectoryScorer,
    TrajectoryStep,
)


# ── Helpers ───────────────────────────────────────────────────────────────────


def _fake_result(tool_calls: list[dict]) -> MagicMock:
    """Minimal fake AgentResult with the given tool_calls list."""
    r = MagicMock()
    r.tool_calls = tool_calls
    return r


def _tool_event(tool: str, node_id: str = "n1") -> dict:
    """Minimal ToolCalled event dict."""
    return {"kind": {"type": "tool_called", "node_id": node_id, "tool": tool}}


def _other_event(kind_type: str) -> dict:
    """Non-tool event (NodeStarted, NodeCompleted, etc.)."""
    return {"kind": {"type": kind_type, "node_id": "n1"}}


# ── Trajectory construction ───────────────────────────────────────────────────


def test_trajectory_from_agent_result_tools():
    tc = [
        {"tool": "search", "input": {"q": "foo"}, "output": "results", "duration_us": 100},
        {"tool": "calculate", "input": {"x": 1}, "output": "42", "duration_us": 50},
    ]
    traj = Trajectory.from_agent_result(_fake_result(tc))
    assert traj.tool_sequence == ["search", "calculate"]
    assert traj.step_count == 2
    assert traj.tool_set == {"search", "calculate"}


def test_trajectory_from_agent_result_args_preserved():
    tc = [{"tool": "search", "input": {"q": "foo"}, "output": "r", "duration_us": 0}]
    traj = Trajectory.from_agent_result(_fake_result(tc))
    assert traj.steps[0].args == {"q": "foo"}
    assert traj.steps[0].output == "r"
    assert traj.steps[0].node_id is None  # not available in-process


def test_trajectory_from_agent_result_empty():
    traj = Trajectory.from_agent_result(_fake_result([]))
    assert traj.tool_sequence == []
    assert traj.step_count == 0
    assert traj.tool_set == set()


def test_trajectory_from_events_raw_list():
    events = [
        _other_event("node_started"),
        _tool_event("search"),
        _tool_event("calculate"),
        _other_event("node_completed"),
    ]
    traj = Trajectory.from_events(events)
    assert traj.tool_sequence == ["search", "calculate"]
    assert traj.step_count == 2


def test_trajectory_from_events_wrapped_dict():
    """from_events accepts the {'events': [...]} dict from get_events()."""
    events_data = {
        "events": [
            _tool_event("search"),
            _tool_event("calculate"),
        ]
    }
    traj = Trajectory.from_events(events_data)
    assert traj.tool_sequence == ["search", "calculate"]


def test_trajectory_from_events_skips_non_tool():
    """Non-tool events (node_started, node_completed, etc.) are ignored."""
    events = [
        _other_event("workflow_started"),
        _other_event("node_started"),
        _tool_event("search"),
        _other_event("node_completed"),
    ]
    traj = Trajectory.from_events(events)
    assert traj.tool_sequence == ["search"]
    assert traj.step_count == 1


def test_trajectory_sources_agree():
    """from_agent_result and from_events produce the same tool sequence."""
    tc = [
        {"tool": "search", "input": {}, "output": "", "duration_us": 0},
        {"tool": "calculate", "input": {}, "output": "", "duration_us": 0},
    ]
    events = [_tool_event("search"), _tool_event("calculate")]

    t1 = Trajectory.from_agent_result(_fake_result(tc))
    t2 = Trajectory.from_events(events)
    assert t1.tool_sequence == t2.tool_sequence
    assert t1.tool_set == t2.tool_set
    assert t1.step_count == t2.step_count


def test_trajectory_node_id_from_events():
    """node_id is populated from events but not from in-process tool_calls."""
    events = [{"kind": {"type": "tool_called", "node_id": "agent-node-1", "tool": "search"}}]
    traj = Trajectory.from_events(events)
    assert traj.steps[0].node_id == "agent-node-1"


def test_trajectory_render():
    traj = Trajectory(
        [
            TrajectoryStep(tool="search", node_id="n1"),
            TrajectoryStep(tool="calculate"),
        ]
    )
    text = traj.render()
    assert "search" in text
    assert "calculate" in text
    assert "2 steps" in text


# ── tool_sequence assertion ───────────────────────────────────────────────────


async def test_tool_sequence_exact_pass():
    """Matching tool sequence -> PASS."""
    traj = Trajectory.from_events([_tool_event("search"), _tool_event("calculate")])
    scorer = TrajectoryScorer(expected={"tool_sequence": ["search", "calculate"]})
    result = await scorer.score(None, trajectory=traj)

    assert isinstance(result, TrajectoryResult)
    assert result.passed is True
    assert result.score == pytest.approx(1.0)
    assert len(result.assertions) == 1
    assert result.assertions[0].name == "tool_sequence"
    assert result.assertions[0].passed is True


async def test_tool_sequence_subsequence_pass():
    """Extra tool between required steps is allowed (subsequence)."""
    traj = Trajectory.from_events(
        [
            _tool_event("search"),
            _tool_event("fetch"),  # extra -- allowed
            _tool_event("calculate"),
        ]
    )
    scorer = TrajectoryScorer(expected={"tool_sequence": ["search", "calculate"]})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True
    assert result.assertions[0].passed is True


async def test_tool_sequence_fail_missing_tool():
    """Missing tool in sequence -> FAIL with breakdown showing which failed."""
    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(expected={"tool_sequence": ["search", "calculate"]})
    result = await scorer.score(None, trajectory=traj)

    assert result.passed is False
    assert result.score == pytest.approx(0.0)
    assert result.assertions[0].name == "tool_sequence"
    assert result.assertions[0].passed is False
    assert "calculate" in result.assertions[0].message


async def test_tool_sequence_fail_wrong_order():
    """Both tools present but wrong order -> FAIL."""
    traj = Trajectory.from_events([_tool_event("calculate"), _tool_event("search")])
    scorer = TrajectoryScorer(expected={"tool_sequence": ["search", "calculate"]})
    result = await scorer.score(None, trajectory=traj)

    assert result.passed is False
    assert result.assertions[0].passed is False


async def test_tool_sequence_empty_expected_passes():
    """Empty expected sequence -> always passes."""
    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(expected={"tool_sequence": []})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True


# ── expected_tools assertion ──────────────────────────────────────────────────


async def test_expected_tools_subset_pass():
    """All listed tools appear (subset check -- extras OK)."""
    traj = Trajectory.from_events(
        [
            _tool_event("search"),
            _tool_event("calculate"),
            _tool_event("extra"),
        ]
    )
    scorer = TrajectoryScorer(expected={"expected_tools": ["search", "calculate"]})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True


async def test_expected_tools_subset_fail():
    """Missing required tool -> FAIL."""
    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(expected={"expected_tools": ["search", "calculate"]})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is False
    assert "calculate" in result.assertions[0].message


async def test_expected_tools_exact_pass():
    traj = Trajectory.from_events([_tool_event("search"), _tool_event("calculate")])
    scorer = TrajectoryScorer(
        expected={
            "expected_tools": ["search", "calculate"],
            "expected_tools_exact": True,
        }
    )
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True


async def test_expected_tools_exact_fail_extra():
    """Extra tool with exact=True -> FAIL."""
    traj = Trajectory.from_events(
        [
            _tool_event("search"),
            _tool_event("calculate"),
            _tool_event("extra"),
        ]
    )
    scorer = TrajectoryScorer(
        expected={
            "expected_tools": ["search", "calculate"],
            "expected_tools_exact": True,
        }
    )
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is False
    assert "extra" in result.assertions[0].message


# ── used_tool assertion ───────────────────────────────────────────────────────


async def test_used_tool_string_pass():
    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(expected={"used_tool": "search"})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True
    assert result.assertions[0].name == "used_tool"
    assert result.assertions[0].passed is True


async def test_used_tool_string_fail():
    """used_tool: required tool absent -> FAIL (negative dual)."""
    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(expected={"used_tool": "calculate"})
    result = await scorer.score(None, trajectory=traj)

    assert result.passed is False
    assert result.assertions[0].name == "used_tool"
    assert result.assertions[0].passed is False
    assert "calculate" in result.assertions[0].message


async def test_used_tool_list_pass():
    traj = Trajectory.from_events([_tool_event("search"), _tool_event("calculate")])
    scorer = TrajectoryScorer(expected={"used_tool": ["search", "calculate"]})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True


async def test_used_tool_list_partial_fail():
    """One of several required tools missing -> FAIL."""
    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(expected={"used_tool": ["search", "calculate"]})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is False


# ── did_not_use assertion ─────────────────────────────────────────────────────


async def test_did_not_use_string_pass():
    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(expected={"did_not_use": "dangerous_tool"})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True
    assert result.assertions[0].name == "did_not_use"
    assert result.assertions[0].passed is True


async def test_did_not_use_string_fail():
    """Forbidden tool was used -> FAIL (negative dual)."""
    traj = Trajectory.from_events([_tool_event("dangerous_tool")])
    scorer = TrajectoryScorer(expected={"did_not_use": "dangerous_tool"})
    result = await scorer.score(None, trajectory=traj)

    assert result.passed is False
    assert result.assertions[0].name == "did_not_use"
    assert result.assertions[0].passed is False
    assert "dangerous_tool" in result.assertions[0].message


async def test_did_not_use_list_fail():
    """Any forbidden tool present -> FAIL."""
    traj = Trajectory.from_events([_tool_event("search"), _tool_event("bad_tool")])
    scorer = TrajectoryScorer(expected={"did_not_use": ["bad_tool", "evil_tool"]})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is False
    assert "bad_tool" in result.assertions[0].message


async def test_did_not_use_list_pass_when_none_used():
    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(expected={"did_not_use": ["bad_tool", "evil_tool"]})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True


# ── max_turns assertion ───────────────────────────────────────────────────────


async def test_max_turns_pass():
    traj = Trajectory.from_events([_tool_event("a"), _tool_event("b")])
    scorer = TrajectoryScorer(expected={"max_turns": 3})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True
    assert result.assertions[0].name == "max_turns"


async def test_max_turns_exact_boundary_pass():
    traj = Trajectory.from_events([_tool_event("a"), _tool_event("b"), _tool_event("c")])
    scorer = TrajectoryScorer(expected={"max_turns": 3})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True  # <= is the check


async def test_max_turns_fail():
    """Exceeding max_turns -> FAIL (negative dual)."""
    traj = Trajectory.from_events(
        [
            _tool_event("a"),
            _tool_event("b"),
            _tool_event("c"),
            _tool_event("d"),
        ]
    )
    scorer = TrajectoryScorer(expected={"max_turns": 3})
    result = await scorer.score(None, trajectory=traj)

    assert result.passed is False
    assert result.assertions[0].name == "max_turns"
    assert result.assertions[0].passed is False
    assert "4" in result.assertions[0].message


# ── step_count assertion ──────────────────────────────────────────────────────


async def test_step_count_exact_pass():
    traj = Trajectory.from_events([_tool_event("a"), _tool_event("b")])
    scorer = TrajectoryScorer(expected={"step_count": 2})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True


async def test_step_count_exact_fail():
    traj = Trajectory.from_events([_tool_event("a")])
    scorer = TrajectoryScorer(expected={"step_count": 2})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is False
    assert result.assertions[0].name == "step_count"


# ── Per-assertion breakdown ───────────────────────────────────────────────────


async def test_multiple_assertions_breakdown_mixed():
    """Multiple assertions: breakdown shows which passed and which failed."""
    traj = Trajectory.from_events([_tool_event("search")])  # 'calculate' missing

    scorer = TrajectoryScorer(
        expected={
            "expected_tools": ["search", "calculate"],  # FAIL (missing calculate)
            "did_not_use": "dangerous",  # PASS
            "max_turns": 5,  # PASS
        }
    )
    result = await scorer.score(None, trajectory=traj)

    assert result.passed is False
    assert len(result.assertions) == 3

    by_name = {a.name: a for a in result.assertions}
    assert "expected_tools" in by_name
    assert "did_not_use" in by_name
    assert "max_turns" in by_name
    assert by_name["expected_tools"].passed is False
    assert by_name["did_not_use"].passed is True
    assert by_name["max_turns"].passed is True

    # Score: 2 of 3 passed
    assert result.score == pytest.approx(2.0 / 3.0)


async def test_all_assertions_pass_score_is_one():
    traj = Trajectory.from_events([_tool_event("search"), _tool_event("calculate")])
    scorer = TrajectoryScorer(
        expected={
            "used_tool": "search",
            "did_not_use": "bad",
            "max_turns": 5,
        }
    )
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True
    assert result.score == pytest.approx(1.0)


async def test_message_lists_failed_assertions():
    """Message names the failed assertions when not all pass."""
    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(
        expected={
            "used_tool": "calculate",  # FAIL
            "did_not_use": "bad",  # PASS
        }
    )
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is False
    assert "used_tool" in result.message


# ── No trajectory provided ────────────────────────────────────────────────────


async def test_no_trajectory_skips_gracefully():
    """If no trajectory kwarg is given, scorer skips all assertions."""
    scorer = TrajectoryScorer(expected={"used_tool": "search"})
    result = await scorer.score("some output")  # no trajectory= kwarg

    assert result.passed is True
    assert result.score is None
    assert len(result.assertions) == 0
    assert "no trajectory" in result.message


async def test_no_assertions_in_spec_passes():
    """Empty spec -> passes with score 1.0."""
    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(expected={})
    result = await scorer.score(None, trajectory=traj)
    assert result.passed is True
    assert result.score == pytest.approx(1.0)
    assert len(result.assertions) == 0


# ── DETERMINISM ───────────────────────────────────────────────────────────────


async def test_scoring_deterministic_same_result_twice():
    """Same trajectory + same spec -> identical result, both calls."""
    traj = Trajectory.from_events([_tool_event("search"), _tool_event("calculate")])
    scorer = TrajectoryScorer(
        expected={
            "tool_sequence": ["search", "calculate"],
            "max_turns": 5,
        }
    )

    r1 = await scorer.score(None, trajectory=traj)
    r2 = await scorer.score(None, trajectory=traj)

    assert r1.passed == r2.passed
    assert r1.score == r2.score
    assert r1.message == r2.message
    assert len(r1.assertions) == len(r2.assertions)
    for a1, a2 in zip(r1.assertions, r2.assertions):
        assert a1.name == a2.name
        assert a1.passed == a2.passed
        assert a1.message == a2.message


async def test_scoring_deterministic_pass_then_fail():
    """Determinism holds across different trajectories (not just same)."""
    traj_pass = Trajectory.from_events([_tool_event("search"), _tool_event("calc")])
    traj_fail = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(expected={"tool_sequence": ["search", "calc"]})

    r_pass = await scorer.score(None, trajectory=traj_pass)
    r_fail = await scorer.score(None, trajectory=traj_fail)
    # Scoring same passing trajectory again -- must equal first result
    r_pass2 = await scorer.score(None, trajectory=traj_pass)

    assert r_pass.passed is True
    assert r_fail.passed is False
    assert r_pass2.passed == r_pass.passed
    assert r_pass2.score == r_pass.score


# ── LLM judge: OFF by default ─────────────────────────────────────────────────


async def test_judge_off_by_default_no_model_call(monkeypatch):
    """No model call is made when judge=False (the default)."""
    calls: list[str] = []

    async def spy_model(prompt: str) -> str:
        calls.append(prompt)
        return '{"score": 5, "reason": "great"}'

    # Monkeypatch LlmJudgeScorer._call_model to detect any invocation.
    from jamjet.eval import scorers as scorers_mod

    monkeypatch.setattr(scorers_mod.LlmJudgeScorer, "_call_model", spy_model)

    traj = Trajectory.from_events([_tool_event("search")])
    # Default: no judge flag
    scorer = TrajectoryScorer(expected={"used_tool": "search"})
    result = await scorer.score(None, trajectory=traj)

    assert result.passed is True
    assert len(calls) == 0, "LLM judge must NOT be called when judge=False"


async def test_judge_enabled_calls_model():
    """When judge=True, the model_fn is called exactly once."""
    calls: list[str] = []

    async def fake_model(prompt: str) -> str:
        calls.append(prompt)
        return '{"score": 4, "reason": "good trajectory"}'

    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(
        expected={"used_tool": "search"},
        judge=True,
        judge_model_fn=fake_model,
    )
    result = await scorer.score(None, trajectory=traj)

    assert result.passed is True  # structural assertion passes
    assert len(calls) == 1, "LLM judge SHOULD be called once when judge=True"
    assert "judge" in result.message  # judge result appended to message


async def test_judge_enabled_but_structural_fails():
    """Even with judge enabled, structural failure dominates passed=False."""

    async def fake_model(prompt: str) -> str:
        return '{"score": 5, "reason": "great"}'

    traj = Trajectory.from_events([_tool_event("search")])
    scorer = TrajectoryScorer(
        expected={"used_tool": "calculate"},  # FAIL structurally
        judge=True,
        judge_model_fn=fake_model,
    )
    result = await scorer.score(None, trajectory=traj)

    # Structural assertion failed -- overall result is False
    assert result.passed is False


# ── Public surface via jamjet.eval ────────────────────────────────────────────


def test_exports_from_jamjet_eval():
    """Trajectory, TrajectoryScorer etc. are accessible from jamjet.eval."""
    from jamjet.eval import (
        Trajectory,
        TrajectoryAssertionResult,
        TrajectoryResult,
        TrajectoryScorer,
        TrajectoryStep,
    )

    assert Trajectory is not None
    assert TrajectoryScorer is not None
    assert TrajectoryResult is not None
    assert TrajectoryStep is not None
    assert TrajectoryAssertionResult is not None


def test_trajectory_result_is_scorer_result():
    """TrajectoryResult is a ScorerResult subclass."""
    from jamjet.eval.scorers import ScorerResult

    result = TrajectoryResult(
        scorer="trajectory",
        passed=True,
        score=1.0,
        message="ok",
        assertions=[],
    )
    assert isinstance(result, ScorerResult)
    assert result.scorer == "trajectory"
    assert result.passed is True
