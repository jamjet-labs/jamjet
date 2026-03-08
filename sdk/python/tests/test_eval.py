"""Tests for the eval harness — scorers, dataset loader, and runner."""

from __future__ import annotations

import json

import pytest

from jamjet.eval.dataset import EvalDataset, EvalRow
from jamjet.eval.runner import EvalResult, EvalRunner
from jamjet.eval.scorers import (
    AssertionScorer,
    CostScorer,
    LatencyScorer,
    LlmJudgeScorer,
    ScorerResult,
)

# ── EvalDataset ───────────────────────────────────────────────────────────────


def test_dataset_from_jsonl(tmp_path):
    path = tmp_path / "data.jsonl"
    path.write_text(
        '{"input": {"query": "hello"}, "expected": "world"}\n'
        '{"id": "r2", "input": {"query": "foo"}, "metadata": {"tag": "smoke"}}\n'
        "# comment line\n"
        "\n"
    )
    ds = EvalDataset.from_file(path)
    assert len(ds) == 2
    assert ds[0].id == "row_0"
    assert ds[0].input == {"query": "hello"}
    assert ds[0].expected == "world"
    assert ds[1].id == "r2"
    assert ds[1].metadata == {"tag": "smoke"}


def test_dataset_from_json(tmp_path):
    path = tmp_path / "data.json"
    path.write_text(
        json.dumps(
            [
                {"input": {"q": "a"}, "expected": 1},
                {"id": "explicit", "input": {"q": "b"}},
            ]
        )
    )
    ds = EvalDataset.from_file(path)
    assert len(ds) == 2
    assert ds[0].expected == 1
    assert ds[1].id == "explicit"


def test_dataset_file_not_found():
    with pytest.raises(FileNotFoundError):
        EvalDataset.from_file("/nonexistent/missing.jsonl")


def test_dataset_missing_input_field(tmp_path):
    path = tmp_path / "bad.jsonl"
    path.write_text('{"expected": "x"}\n')
    with pytest.raises(ValueError, match="missing required 'input'"):
        EvalDataset.from_file(path)


def test_dataset_json_not_array(tmp_path):
    path = tmp_path / "bad.json"
    path.write_text('{"input": {"q": "a"}}')
    with pytest.raises(ValueError, match="top-level array"):
        EvalDataset.from_file(path)


def test_dataset_iter():
    rows = [EvalRow(id=str(i), input={"x": i}) for i in range(3)]
    ds = EvalDataset(rows)
    assert list(ds) == rows


def test_dataset_len():
    rows = [EvalRow(id="a", input={}), EvalRow(id="b", input={})]
    ds = EvalDataset(rows)
    assert len(ds) == 2


# ── AssertionScorer ───────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_assertion_all_pass():
    scorer = AssertionScorer(checks=["'answer' in output", "len(output['answer']) > 0"])
    result = await scorer.score({"answer": "hello"})
    assert result.passed is True
    assert result.score == 1.0
    assert "passed" in result.message


@pytest.mark.asyncio
async def test_assertion_one_fails():
    scorer = AssertionScorer(checks=["'answer' in output", "output['answer'] == 'exact'"])
    result = await scorer.score({"answer": "something else"})
    assert result.passed is False
    assert result.score == 0.0
    assert "failed" in result.message


@pytest.mark.asyncio
async def test_assertion_check_error():
    scorer = AssertionScorer(checks=["output.nonexistent_method()"])
    result = await scorer.score("string output")
    assert result.passed is False
    assert "error:" in result.message


@pytest.mark.asyncio
async def test_assertion_false_check():
    scorer = AssertionScorer(checks=["False"])
    result = await scorer.score(None)
    assert result.passed is False
    assert "False" in result.message


@pytest.mark.asyncio
async def test_assertion_uses_expected():
    scorer = AssertionScorer(checks=["output == expected"])
    result = await scorer.score("hello", expected="hello")
    assert result.passed is True


@pytest.mark.asyncio
async def test_assertion_empty_checks():
    scorer = AssertionScorer(checks=[])
    result = await scorer.score("anything")
    assert result.passed is True
    assert result.score == 1.0


# ── LatencyScorer ─────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_latency_pass():
    scorer = LatencyScorer(threshold_ms=1000.0)
    result = await scorer.score(None, duration_ms=500.0)
    assert result.passed is True
    assert result.score == 500.0


@pytest.mark.asyncio
async def test_latency_fail():
    scorer = LatencyScorer(threshold_ms=100.0)
    result = await scorer.score(None, duration_ms=200.0)
    assert result.passed is False
    assert result.score == 200.0


@pytest.mark.asyncio
async def test_latency_exact_threshold():
    scorer = LatencyScorer(threshold_ms=500.0)
    result = await scorer.score(None, duration_ms=500.0)
    assert result.passed is True  # <=, not <


@pytest.mark.asyncio
async def test_latency_no_duration():
    scorer = LatencyScorer(threshold_ms=1000.0)
    result = await scorer.score(None)
    assert result.passed is True  # no data → don't penalise
    assert result.score is None


# ── CostScorer ────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_cost_pass():
    scorer = CostScorer(threshold_usd=0.01)
    result = await scorer.score(None, cost_usd=0.005)
    assert result.passed is True
    assert result.score == pytest.approx(0.005)


@pytest.mark.asyncio
async def test_cost_fail():
    scorer = CostScorer(threshold_usd=0.001)
    result = await scorer.score(None, cost_usd=0.005)
    assert result.passed is False


@pytest.mark.asyncio
async def test_cost_exact_threshold():
    scorer = CostScorer(threshold_usd=0.01)
    result = await scorer.score(None, cost_usd=0.01)
    assert result.passed is True


@pytest.mark.asyncio
async def test_cost_no_data():
    scorer = CostScorer(threshold_usd=0.01)
    result = await scorer.score(None)
    assert result.passed is True
    assert result.score is None


# ── LlmJudgeScorer ────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_llm_judge_pass():
    async def fake_model(prompt: str) -> str:
        return '{"score": 5, "reason": "excellent"}'

    scorer = LlmJudgeScorer(rubric="Rate quality 1-5", min_score=3, model_fn=fake_model)
    result = await scorer.score("good output")
    assert result.passed is True
    assert result.score == 5.0
    assert "5/5" in result.message


@pytest.mark.asyncio
async def test_llm_judge_fail_low_score():
    async def fake_model(prompt: str) -> str:
        return '{"score": 2, "reason": "poor"}'

    scorer = LlmJudgeScorer(rubric="Rate quality 1-5", min_score=3, model_fn=fake_model)
    result = await scorer.score("bad output")
    assert result.passed is False
    assert result.score == 2.0


@pytest.mark.asyncio
async def test_llm_judge_exact_min_score():
    async def fake_model(prompt: str) -> str:
        return '{"score": 3, "reason": "acceptable"}'

    scorer = LlmJudgeScorer(rubric="Rate quality 1-5", min_score=3, model_fn=fake_model)
    result = await scorer.score("output")
    assert result.passed is True


@pytest.mark.asyncio
async def test_llm_judge_no_json_in_response():
    async def fake_model(prompt: str) -> str:
        return "not json at all"

    scorer = LlmJudgeScorer(rubric="Rate quality 1-5", model_fn=fake_model)
    result = await scorer.score("output")
    assert result.passed is False
    assert "judge failed" in result.message


@pytest.mark.asyncio
async def test_llm_judge_model_fn_exception():
    async def bad_model(prompt: str) -> str:
        raise RuntimeError("API unavailable")

    scorer = LlmJudgeScorer(rubric="Rate quality 1-5", model_fn=bad_model)
    result = await scorer.score("output")
    assert result.passed is False
    assert "judge failed" in result.message


@pytest.mark.asyncio
async def test_llm_judge_json_embedded_in_text():
    """Judge response may include prose before/after the JSON block."""

    async def fake_model(prompt: str) -> str:
        return 'Here is my evaluation: {"score": 4, "reason": "good"} — hope that helps!'

    scorer = LlmJudgeScorer(rubric="Rate quality 1-5", min_score=3, model_fn=fake_model)
    result = await scorer.score("output")
    assert result.passed is True
    assert result.score == 4.0


# ── EvalResult ────────────────────────────────────────────────────────────────


def _make_result(scorers=None, error=None, output=None):
    return EvalResult(
        row_id="r1",
        input={"q": "hello"},
        expected=None,
        output=output or {"answer": "world"},
        scorers=scorers or [],
        duration_ms=100.0,
        cost_usd=None,
        error=error,
    )


def test_eval_result_passed_no_scorers():
    r = _make_result()
    assert r.passed is True


def test_eval_result_passed_all_pass():
    scorers = [ScorerResult("a", True, 1.0, "ok"), ScorerResult("b", True, 0.8, "ok")]
    r = _make_result(scorers=scorers)
    assert r.passed is True
    assert r.overall_score == pytest.approx(0.9)


def test_eval_result_failed_one_scorer():
    scorers = [ScorerResult("a", True, 1.0, "ok"), ScorerResult("b", False, 0.0, "fail")]
    r = _make_result(scorers=scorers)
    assert r.passed is False


def test_eval_result_error_means_failed():
    r = _make_result(error="timeout after 120s")
    assert r.passed is False


def test_eval_result_overall_score_none_when_no_numeric():
    scorers = [ScorerResult("a", True, None, "no score")]
    r = _make_result(scorers=scorers)
    assert r.overall_score is None


def test_eval_result_overall_score_skips_none():
    scorers = [ScorerResult("a", True, 1.0, "ok"), ScorerResult("b", True, None, "no score")]
    r = _make_result(scorers=scorers)
    assert r.overall_score == pytest.approx(1.0)


# ── EvalRunner (mocked) ───────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_runner_happy_path(monkeypatch):
    """Single dataset row against a mocked JamjetClient — execution completes."""
    from unittest.mock import AsyncMock, MagicMock

    mock_client = MagicMock()
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=None)
    mock_client.start_execution = AsyncMock(return_value={"execution_id": "exec-1"})
    mock_client.get_execution = AsyncMock(return_value={"status": "completed", "output": {"answer": "42"}})
    mock_client.get_events = AsyncMock(return_value={"events": []})

    import jamjet.client as client_mod

    monkeypatch.setattr(client_mod, "JamjetClient", lambda **kw: mock_client)

    ds = EvalDataset([EvalRow(id="r1", input={"query": "hello"})])
    scorer = AssertionScorer(checks=["'answer' in output"])
    runner = EvalRunner("wf-id", [scorer], poll_interval_s=0.0)
    results = await runner.run(ds)

    assert len(results) == 1
    assert results[0].passed is True
    assert results[0].output == {"answer": "42"}
    assert results[0].error is None


@pytest.mark.asyncio
async def test_runner_execution_failed(monkeypatch):
    from unittest.mock import AsyncMock, MagicMock

    mock_client = MagicMock()
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=None)
    mock_client.start_execution = AsyncMock(return_value={"execution_id": "exec-2"})
    mock_client.get_execution = AsyncMock(return_value={"status": "failed", "error": "model timeout"})
    mock_client.get_events = AsyncMock(return_value={"events": []})

    import jamjet.client as client_mod

    monkeypatch.setattr(client_mod, "JamjetClient", lambda **kw: mock_client)

    ds = EvalDataset([EvalRow(id="r1", input={"query": "hi"})])
    runner = EvalRunner("wf-id", [], poll_interval_s=0.0)
    results = await runner.run(ds)

    assert len(results) == 1
    assert results[0].passed is False
    assert "failed" in results[0].error


@pytest.mark.asyncio
async def test_runner_cost_extraction(monkeypatch):
    """Cost is summed from node_completed events."""
    from unittest.mock import AsyncMock, MagicMock

    mock_client = MagicMock()
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=None)
    mock_client.start_execution = AsyncMock(return_value={"execution_id": "exec-3"})
    mock_client.get_execution = AsyncMock(return_value={"status": "completed", "output": {"answer": "ok"}})
    mock_client.get_events = AsyncMock(
        return_value={
            "events": [
                {"kind": {"type": "node_completed", "cost_usd": 0.001}},
                {"kind": {"type": "node_completed", "cost_usd": 0.002}},
                {"kind": {"type": "node_started"}},  # no cost field → ignored
            ]
        }
    )

    import jamjet.client as client_mod

    monkeypatch.setattr(client_mod, "JamjetClient", lambda **kw: mock_client)

    ds = EvalDataset([EvalRow(id="r1", input={"query": "cost test"})])
    runner = EvalRunner("wf-id", [], poll_interval_s=0.0)
    results = await runner.run(ds)

    assert results[0].cost_usd == pytest.approx(0.003)


@pytest.mark.asyncio
async def test_runner_concurrency(monkeypatch):
    """Multiple rows complete independently."""
    from unittest.mock import AsyncMock, MagicMock

    mock_client = MagicMock()
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=None)
    mock_client.start_execution = AsyncMock(return_value={"execution_id": "exec-x"})
    mock_client.get_execution = AsyncMock(return_value={"status": "completed", "output": {"x": 1}})
    mock_client.get_events = AsyncMock(return_value={"events": []})

    import jamjet.client as client_mod

    monkeypatch.setattr(client_mod, "JamjetClient", lambda **kw: mock_client)

    rows = [EvalRow(id=str(i), input={"q": i}) for i in range(5)]
    ds = EvalDataset(rows)
    runner = EvalRunner("wf-id", [], concurrency=3, poll_interval_s=0.0)
    results = await runner.run(ds)

    assert len(results) == 5
    assert all(r.passed for r in results)


@pytest.mark.asyncio
async def test_runner_scorer_error_does_not_crash(monkeypatch):
    """A scorer that raises an exception produces a failed ScorerResult, not a crash."""
    from unittest.mock import AsyncMock, MagicMock

    mock_client = MagicMock()
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=None)
    mock_client.start_execution = AsyncMock(return_value={"execution_id": "exec-4"})
    mock_client.get_execution = AsyncMock(return_value={"status": "completed", "output": {"answer": "hi"}})
    mock_client.get_events = AsyncMock(return_value={"events": []})

    import jamjet.client as client_mod

    monkeypatch.setattr(client_mod, "JamjetClient", lambda **kw: mock_client)

    class BrokenScorer(AssertionScorer):
        async def score(self, output, **kwargs):
            raise ValueError("scorer exploded")

    ds = EvalDataset([EvalRow(id="r1", input={"q": "x"})])
    runner = EvalRunner("wf-id", [BrokenScorer(checks=[])], poll_interval_s=0.0)
    results = await runner.run(ds)

    assert len(results) == 1
    assert results[0].passed is False
    assert "scorer error" in results[0].scorers[0].message
