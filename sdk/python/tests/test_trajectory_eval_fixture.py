"""
Smoke test: the trajectory-eval example evalset loads without error via the
real dataset loader (eval/dataset.py) and has the expected shape.

This is the T5-6 fixture-validity gate: if the example file is malformed the
loader raises, failing the suite and preventing broken example files from
landing on main.
"""

from __future__ import annotations

from pathlib import Path

from jamjet.eval.dataset import EvalDataset

REPO_ROOT = Path(__file__).resolve().parents[3]
EVALSET = REPO_ROOT / "examples" / "trajectory-eval" / "evalset.jsonl"


def test_trajectory_eval_fixture_exists():
    """The evalset file ships alongside the example."""
    assert EVALSET.exists(), f"evalset.jsonl missing at {EVALSET}"


def test_trajectory_eval_fixture_loads():
    """EvalDataset.from_file parses the evalset without error."""
    ds = EvalDataset.from_file(EVALSET)
    assert len(ds) == 3, f"expected 3 rows, got {len(ds)}"


def test_trajectory_eval_fixture_row_ids():
    """Each row has the expected id."""
    ds = EvalDataset.from_file(EVALSET)
    ids = [row.id for row in ds]
    assert "search-then-calc" in ids
    assert "search-only" in ids
    assert "no-trajectory" in ids


def test_trajectory_eval_fixture_expected_trajectory_shape():
    """Rows with expected_trajectory carry the right spec keys."""
    ds = EvalDataset.from_file(EVALSET)
    rows_by_id = {row.id: row for row in ds}

    # search-then-calc: tool_sequence + max_turns
    r = rows_by_id["search-then-calc"]
    assert r.expected_trajectory is not None
    assert "tool_sequence" in r.expected_trajectory
    assert r.expected_trajectory["tool_sequence"] == ["web_search", "calculator"]
    assert r.expected_trajectory["max_turns"] == 4

    # search-only: used_tool + did_not_use
    r = rows_by_id["search-only"]
    assert r.expected_trajectory is not None
    assert r.expected_trajectory["used_tool"] == "web_search"
    assert r.expected_trajectory["did_not_use"] == "calculator"

    # no-trajectory: backward-compatible -- expected_trajectory is None
    r = rows_by_id["no-trajectory"]
    assert r.expected_trajectory is None
