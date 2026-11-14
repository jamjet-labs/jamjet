"""The code review example routes by expertise and diff size without a model."""

import runpy
from pathlib import Path

import pytest

EXAMPLE = Path(__file__).resolve().parents[3] / "examples" / "code-review-routing" / "main.py"
example = runpy.run_path(str(EXAMPLE))


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("language", "diff_lines", "reviewer"),
    [
        ("python", 50, "python-reviewer"),
        ("python", 1000, "python-deep-reviewer"),
        ("rust", 300, "rust-reviewer"),
        ("typescript", 100, "frontend-reviewer"),
    ],
)
async def test_routes_to_matching_reviewer(language, diff_lines, reviewer):
    strategy = example["CodeReviewStrategy"]()
    context = {"language": language, "diff_lines": diff_lines}
    rankings, spread = await strategy.score("Review", example["AGENTS"], {}, context)
    decision = await strategy.decide("Review", rankings, 0.1, "", context)

    assert decision.selected_uri == f"jamjet://review/{reviewer}"
    assert decision.method == "structured"
    assert [result.composite for result in rankings] == sorted((result.composite for result in rankings), reverse=True)
    assert spread == pytest.approx(rankings[0].composite - rankings[1].composite)


@pytest.mark.asyncio
async def test_empty_candidates_leave_no_selection():
    strategy = example["CodeReviewStrategy"]()
    rankings, spread = await strategy.score("Review", [], {}, {"language": "python", "diff_lines": 0})
    decision = await strategy.decide("Review", rankings, 0.1, "", {})

    assert rankings == []
    assert spread == 1.0
    assert decision.selected_uri is None
