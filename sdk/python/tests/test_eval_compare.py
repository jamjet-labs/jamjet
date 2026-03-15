"""Tests for eval compare — ComparisonResult, GridResults.compare(), and CLI command."""

from __future__ import annotations

import json
from unittest.mock import patch

import pytest

from jamjet.eval.grid import (
    ComparisonResult,
    ConditionResult,
    GridResults,
    _cohens_d,
    _confidence_interval,
    _resolve_auto_test,
    _run_comparison,
)
from jamjet.eval.runner import EvalResult
from jamjet.eval.scorers import ScorerResult

# ── Helpers ──────────────────────────────────────────────────────────────────


def _make_eval_result(score: float, row_id: str = "r1") -> EvalResult:
    return EvalResult(
        row_id=row_id,
        input={"q": "test"},
        expected=None,
        output={"answer": "ok"},
        scorers=[ScorerResult(scorer="mock", passed=True, score=score, message="ok")],
        duration_ms=100.0,
        cost_usd=None,
    )


def _make_grid_results(
    scores_a: list[float],
    scores_b: list[float],
    cond_a: dict[str, str] | None = None,
    cond_b: dict[str, str] | None = None,
) -> GridResults:
    if cond_a is None:
        cond_a = {"strategy": "react"}
    if cond_b is None:
        cond_b = {"strategy": "debate"}

    results_a = [_make_eval_result(s, row_id=f"a{i}") for i, s in enumerate(scores_a)]
    results_b = [_make_eval_result(s, row_id=f"b{i}") for i, s in enumerate(scores_b)]

    return GridResults(
        [
            ConditionResult(condition=cond_a, seed=42, eval_results=results_a),
            ConditionResult(condition=cond_b, seed=42, eval_results=results_b),
        ]
    )


# ── ComparisonResult construction ────────────────────────────────────────────


def test_comparison_result_construction():
    cr = ComparisonResult(
        test_name="welch_t_test",
        statistic=2.5,
        p_value=0.02,
        effect_size=0.8,
        ci_lower=0.1,
        ci_upper=0.9,
        significant=True,
        sample_sizes=(10, 10),
        mean_a=3.5,
        mean_b=3.0,
        mean_diff=0.5,
        condition_a="strategy=react",
        condition_b="strategy=debate",
    )
    assert cr.test_name == "welch_t_test"
    assert cr.statistic == 2.5
    assert cr.p_value == 0.02
    assert cr.effect_size == 0.8
    assert cr.ci_lower == 0.1
    assert cr.ci_upper == 0.9
    assert cr.significant is True
    assert cr.sample_sizes == (10, 10)
    assert cr.mean_a == 3.5
    assert cr.mean_b == 3.0
    assert cr.mean_diff == 0.5
    assert cr.condition_a == "strategy=react"
    assert cr.condition_b == "strategy=debate"


def test_comparison_result_defaults():
    cr = ComparisonResult(
        test_name="test",
        statistic=None,
        p_value=None,
        effect_size=None,
        ci_lower=None,
        ci_upper=None,
        significant=False,
        sample_sizes=(5, 5),
    )
    assert cr.mean_a == 0.0
    assert cr.mean_b == 0.0
    assert cr.mean_diff == 0.0
    assert cr.condition_a == ""
    assert cr.condition_b == ""


# ── compare() with scipy ─────────────────────────────────────────────────────


def test_compare_welch():
    gr = _make_grid_results(
        [4.0, 4.5, 3.8, 4.2, 4.1],
        [3.0, 3.2, 2.8, 3.1, 3.0],
    )
    result = gr.compare({"strategy": "react"}, {"strategy": "debate"}, test="welch")
    assert isinstance(result, ComparisonResult)
    assert result.test_name == "welch_t_test"
    assert result.statistic is not None
    assert result.p_value is not None
    assert result.p_value < 0.05
    assert result.significant is True
    assert result.sample_sizes == (5, 5)
    assert result.mean_a > result.mean_b
    assert result.mean_diff > 0


def test_compare_wilcoxon():
    gr = _make_grid_results(
        [4.0, 4.5, 3.8, 4.2, 4.1],
        [3.0, 3.2, 2.8, 3.1, 3.0],
    )
    result = gr.compare({"strategy": "react"}, {"strategy": "debate"}, test="wilcoxon")
    assert result.test_name == "wilcoxon_signed_rank"
    assert result.statistic is not None
    assert result.p_value is not None


def test_compare_mann_whitney():
    gr = _make_grid_results(
        [4.0, 4.5, 3.8, 4.2, 4.1],
        [3.0, 3.2, 2.8, 3.1, 3.0],
    )
    result = gr.compare({"strategy": "react"}, {"strategy": "debate"}, test="mann_whitney")
    assert result.test_name == "mann_whitney_u"
    assert result.statistic is not None
    assert result.p_value is not None


def test_compare_auto_picks_wilcoxon_for_small_paired():
    gr = _make_grid_results(
        [4.0, 4.5, 3.8, 4.2, 4.1],
        [3.0, 3.2, 2.8, 3.1, 3.0],
    )
    result = gr.compare({"strategy": "react"}, {"strategy": "debate"}, test="auto")
    # n=5 paired -> auto selects wilcoxon
    assert result.test_name == "wilcoxon_signed_rank"


def test_compare_auto_picks_mann_whitney_for_unequal():
    gr = _make_grid_results(
        [4.0, 4.5, 3.8, 4.2],
        [3.0, 3.2, 2.8, 3.1, 3.0],
    )
    result = gr.compare({"strategy": "react"}, {"strategy": "debate"}, test="auto")
    assert result.test_name == "mann_whitney_u"


def test_compare_condition_not_found():
    gr = _make_grid_results([4.0], [3.0])
    with pytest.raises(ValueError, match="condition not found"):
        gr.compare({"strategy": "nonexistent"}, {"strategy": "debate"})


def test_compare_insufficient_data():
    gr = GridResults(
        [
            ConditionResult(
                condition={"strategy": "react"},
                seed=42,
                eval_results=[
                    EvalResult(
                        row_id="r1",
                        input={},
                        expected=None,
                        output=None,
                        scorers=[],
                        duration_ms=100.0,
                        cost_usd=None,
                    )
                ],
            ),
            ConditionResult(
                condition={"strategy": "debate"},
                seed=42,
                eval_results=[_make_eval_result(3.0)],
            ),
        ]
    )
    with pytest.raises(ValueError, match="insufficient"):
        gr.compare({"strategy": "react"}, {"strategy": "debate"})


def test_compare_not_significant():
    # Scores that are very close should not be significant.
    gr = _make_grid_results(
        [3.0, 3.1, 2.9, 3.0, 3.05],
        [3.0, 2.95, 3.1, 3.0, 2.98],
    )
    result = gr.compare({"strategy": "react"}, {"strategy": "debate"}, test="welch")
    assert result.significant is False


def test_compare_custom_alpha():
    gr = _make_grid_results(
        [4.0, 4.5, 3.8, 4.2, 4.1],
        [3.0, 3.2, 2.8, 3.1, 3.0],
    )
    # Very strict alpha — should still be significant with this data.
    result_strict = gr.compare({"strategy": "react"}, {"strategy": "debate"}, test="welch", alpha=0.001)
    # Very lenient alpha.
    result_lenient = gr.compare({"strategy": "react"}, {"strategy": "debate"}, test="welch", alpha=0.5)
    assert result_lenient.significant is True
    # Both should have the same p-value.
    assert result_strict.p_value == result_lenient.p_value


# ── compare() without scipy (fallback) ───────────────────────────────────────


def test_compare_without_scipy():
    gr = _make_grid_results(
        [4.0, 4.5, 3.8, 4.2, 4.1],
        [3.0, 3.2, 2.8, 3.1, 3.0],
    )
    with patch.dict("sys.modules", {"scipy": None, "scipy.stats": None}):
        import jamjet.eval.grid as grid_mod

        # Force re-import to trigger ImportError path.
        # We mock at the _run_comparison level since the module is already loaded.
        orig_scipy_compare = grid_mod._scipy_compare

        def _fake_scipy_compare(*args, **kwargs):
            raise ImportError("No module named 'scipy'")

        grid_mod._scipy_compare = _fake_scipy_compare
        try:
            with pytest.warns(UserWarning, match="scipy is not installed"):
                result = gr.compare({"strategy": "react"}, {"strategy": "debate"}, test="welch")
            assert result.test_name == "basic_mean_diff"
            assert result.statistic is None
            assert result.p_value is None
            assert result.effect_size is None
            assert result.ci_lower is None
            assert result.ci_upper is None
            assert result.significant is False
            assert result.mean_diff == pytest.approx(result.mean_a - result.mean_b)
        finally:
            grid_mod._scipy_compare = orig_scipy_compare


# ── Auto test selection ──────────────────────────────────────────────────────


def test_auto_resolves_to_wilcoxon_small_paired():
    result = _resolve_auto_test("auto", list(range(10)), list(range(10)))
    assert result == "wilcoxon"


def test_auto_resolves_to_mann_whitney_unequal():
    result = _resolve_auto_test("auto", list(range(10)), list(range(15)))
    assert result == "mann_whitney"


def test_auto_resolves_to_welch_large_paired():
    result = _resolve_auto_test("auto", list(range(50)), list(range(50)))
    assert result == "welch"


def test_auto_passthrough_non_auto():
    assert _resolve_auto_test("welch", [1], [2]) == "welch"
    assert _resolve_auto_test("wilcoxon", [1], [2]) == "wilcoxon"
    assert _resolve_auto_test("mann_whitney", [1], [2]) == "mann_whitney"


# ── Welch's t-test ───────────────────────────────────────────────────────────


def test_welch_t_test_returns_correct_type():
    result = _run_comparison(
        [4.0, 4.5, 3.8, 4.2, 4.1],
        [3.0, 3.2, 2.8, 3.1, 3.0],
        test="welch",
    )
    assert result.test_name == "welch_t_test"
    assert isinstance(result.statistic, float)
    assert isinstance(result.p_value, float)


def test_welch_identical_samples():
    vals = [3.0, 3.0, 3.0, 3.0, 3.0]
    result = _run_comparison(vals, vals, test="welch")
    # Identical samples should have p=1 or NaN (scipy returns nan for zero variance).
    assert result.mean_diff == pytest.approx(0.0)


# ── Confidence interval computation ─────────────────────────────────────────


def test_confidence_interval_contains_mean_diff():
    a = [4.0, 4.5, 3.8, 4.2, 4.1]
    b = [3.0, 3.2, 2.8, 3.1, 3.0]
    ci_lo, ci_hi = _confidence_interval(a, b, alpha=0.05)
    mean_diff = sum(a) / len(a) - sum(b) / len(b)
    assert ci_lo <= mean_diff <= ci_hi


def test_confidence_interval_narrower_at_lower_confidence():
    a = [4.0, 4.5, 3.8, 4.2, 4.1, 4.3, 3.9, 4.0]
    b = [3.0, 3.2, 2.8, 3.1, 3.0, 3.3, 2.9, 3.1]
    ci_95_lo, ci_95_hi = _confidence_interval(a, b, alpha=0.05)
    ci_99_lo, ci_99_hi = _confidence_interval(a, b, alpha=0.01)
    width_95 = ci_95_hi - ci_95_lo
    width_99 = ci_99_hi - ci_99_lo
    assert width_99 > width_95


def test_confidence_interval_zero_variance():
    a = [3.0, 3.0, 3.0]
    b = [3.0, 3.0, 3.0]
    ci_lo, ci_hi = _confidence_interval(a, b, alpha=0.05)
    assert ci_lo == pytest.approx(0.0)
    assert ci_hi == pytest.approx(0.0)


# ── Effect size computation ──────────────────────────────────────────────────


def test_cohens_d_large_effect():
    a = [4.0, 4.5, 3.8, 4.2, 4.1]
    b = [2.0, 2.5, 1.8, 2.2, 2.1]
    d = _cohens_d(a, b)
    assert abs(d) > 0.8  # Large effect


def test_cohens_d_zero_effect():
    a = [3.0, 3.0, 3.0, 3.0]
    b = [3.0, 3.0, 3.0, 3.0]
    d = _cohens_d(a, b)
    assert d == pytest.approx(0.0)


def test_cohens_d_sign():
    a = [4.0, 4.5, 3.8]
    b = [3.0, 3.2, 2.8]
    d = _cohens_d(a, b)
    assert d > 0  # a > b, so d is positive
    d_reversed = _cohens_d(b, a)
    assert d_reversed < 0


def test_cohens_d_single_sample():
    # Edge case: single-element samples.
    d = _cohens_d([5.0], [3.0])
    assert isinstance(d, float)


# ── from_json / round-trip ───────────────────────────────────────────────────


def test_grid_results_from_json_roundtrip(tmp_path):
    gr = _make_grid_results([4.0, 4.5], [3.0, 3.2])
    path = str(tmp_path / "results.json")
    gr.to_json(path)

    loaded = GridResults.from_json(path)
    assert len(loaded.results) == 2
    assert loaded.results[0].condition == {"strategy": "react"}
    assert loaded.results[1].condition == {"strategy": "debate"}
    assert len(loaded.results[0].eval_results) == 2
    assert loaded.results[0].eval_results[0].overall_score == pytest.approx(4.0)


def test_grid_results_from_json_invalid_format(tmp_path):
    path = str(tmp_path / "bad.json")
    with open(path, "w") as f:
        json.dump({"not": "a list"}, f)
    with pytest.raises(ValueError, match="expected a JSON array"):
        GridResults.from_json(path)


# ── CLI command ──────────────────────────────────────────────────────────────


def test_cli_compare_table_output(tmp_path):
    from typer.testing import CliRunner

    from jamjet.cli.main import app

    gr = _make_grid_results(
        [4.0, 4.5, 3.8, 4.2, 4.1],
        [3.0, 3.2, 2.8, 3.1, 3.0],
    )
    path = str(tmp_path / "results.json")
    gr.to_json(path)

    runner = CliRunner()
    result = runner.invoke(app, ["eval", "compare", path, "--conditions", "react,debate"])
    assert result.exit_code == 0
    assert "Statistical Comparison" in result.output or "Comparison" in result.output


def test_cli_compare_json_output(tmp_path):
    from typer.testing import CliRunner

    from jamjet.cli.main import app

    gr = _make_grid_results(
        [4.0, 4.5, 3.8, 4.2, 4.1],
        [3.0, 3.2, 2.8, 3.1, 3.0],
    )
    path = str(tmp_path / "results.json")
    gr.to_json(path)

    runner = CliRunner()
    result = runner.invoke(app, ["eval", "compare", path, "--conditions", "react,debate", "--format", "json"])
    assert result.exit_code == 0
    data = json.loads(result.output)
    assert "test_name" in data
    assert "p_value" in data
    assert "effect_size" in data
    assert "sample_sizes" in data


def test_cli_compare_file_not_found():
    from typer.testing import CliRunner

    from jamjet.cli.main import app

    runner = CliRunner()
    result = runner.invoke(app, ["eval", "compare", "/nonexistent/file.json", "--conditions", "a,b"])
    assert result.exit_code == 1
    assert "not found" in result.output


def test_cli_compare_bad_conditions():
    from typer.testing import CliRunner

    from jamjet.cli.main import app

    runner = CliRunner()
    result = runner.invoke(app, ["eval", "compare", "file.json", "--conditions", "only_one"])
    assert result.exit_code == 1
    assert "exactly 2" in result.output


def test_cli_compare_condition_not_found(tmp_path):
    from typer.testing import CliRunner

    from jamjet.cli.main import app

    gr = _make_grid_results([4.0, 4.5], [3.0, 3.2])
    path = str(tmp_path / "results.json")
    gr.to_json(path)

    runner = CliRunner()
    result = runner.invoke(app, ["eval", "compare", path, "--conditions", "react,nonexistent"])
    assert result.exit_code == 1
    assert "not found" in result.output


def test_cli_compare_welch_test_flag(tmp_path):
    from typer.testing import CliRunner

    from jamjet.cli.main import app

    gr = _make_grid_results(
        [4.0, 4.5, 3.8, 4.2, 4.1],
        [3.0, 3.2, 2.8, 3.1, 3.0],
    )
    path = str(tmp_path / "results.json")
    gr.to_json(path)

    runner = CliRunner()
    result = runner.invoke(
        app, ["eval", "compare", path, "--conditions", "react,debate", "--test", "welch", "--format", "json"]
    )
    assert result.exit_code == 0
    data = json.loads(result.output)
    assert data["test_name"] == "welch_t_test"
