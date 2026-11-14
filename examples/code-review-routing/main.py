"""Route code reviews with mock agents; no runtime or model API key required."""

from __future__ import annotations

import asyncio
from typing import Any

from jamjet.coordinator import AgentCandidate, DefaultCoordinatorStrategy, DimensionScores, ScoringResult


class CodeReviewStrategy(DefaultCoordinatorStrategy):
    """Rank reviewers by language expertise and capacity for the diff size."""

    async def score(
        self,
        task: str,
        candidates: list[AgentCandidate],
        weights: dict[str, float],
        context: dict[str, Any],
    ) -> tuple[list[ScoringResult], float]:
        """Use context's language and diff_lines to score the mock reviewers.

        Agent cards contain expertise scores from 0 to 1 and max_diff_lines,
        a rough capacity estimate. Diff size is a simple proxy for complexity,
        not a measurement of code quality or review correctness.
        """
        language = context["language"]
        diff_lines = max(context["diff_lines"], 1)
        scoring_weights = {
            "capability_fit": 3.0,
            "cost_fit": 0.5,
            "latency_fit": 2.0,
            "trust_compatibility": 0.0,
            "historical_performance": 0.0,
        }
        scoring_weights.update({key: weights[key] for key in scoring_weights if key in weights})
        results = []
        for candidate in candidates:
            expertise = candidate.agent_card.get("expertise", {}).get(language, 0.0)
            capacity = candidate.agent_card.get("max_diff_lines", 0)
            scores = DimensionScores(
                capability_fit=expertise,
                cost_fit=self._score_cost(candidate),
                latency_fit=self._score_latency(candidate) * min(capacity / diff_lines, 1.0),
            )
            results.append(
                ScoringResult(
                    agent_uri=candidate.uri,
                    scores=scores,
                    composite=scores.composite(scoring_weights),
                )
            )

        results.sort(key=lambda result: result.composite, reverse=True)
        spread = results[0].composite - results[1].composite if len(results) >= 2 else 1.0
        return results, spread


AGENTS = [
    AgentCandidate(
        uri="jamjet://review/python-reviewer",
        agent_card={"expertise": {"python": 1.0}, "max_diff_lines": 200},
        skills=["code-review"],
        latency_class="low",
        cost_class="low",
    ),
    AgentCandidate(
        uri="jamjet://review/python-deep-reviewer",
        agent_card={"expertise": {"python": 1.0}, "max_diff_lines": 2000},
        skills=["code-review"],
        latency_class="medium",
        cost_class="high",
    ),
    AgentCandidate(
        uri="jamjet://review/rust-reviewer",
        agent_card={"expertise": {"rust": 1.0, "python": 0.3}, "max_diff_lines": 1500},
        skills=["code-review"],
        latency_class="medium",
        cost_class="medium",
    ),
    AgentCandidate(
        uri="jamjet://review/frontend-reviewer",
        agent_card={"expertise": {"typescript": 1.0, "javascript": 1.0}, "max_diff_lines": 800},
        skills=["code-review"],
        latency_class="low",
        cost_class="low",
    ),
]


async def demo() -> None:
    """Print rankings and selections for four local, deterministic examples."""
    strategy = CodeReviewStrategy()
    for language, diff_lines in [("python", 50), ("python", 1000), ("rust", 300), ("typescript", 100)]:
        task = f"Review a {diff_lines}-line {language} diff"
        context = {"language": language, "diff_lines": diff_lines}
        rankings, spread = await strategy.score(task, AGENTS, {}, context)
        # An empty model name disables the optional LLM tiebreaker.
        decision = await strategy.decide(task, rankings, 0.1, "", context)
        print(f"\n{task} (spread={spread:.3f})")
        for result in rankings:
            print(f"  {result.agent_uri}: {result.composite:.3f}")
        print(f"Selected: {decision.selected_uri}")


if __name__ == "__main__":
    asyncio.run(demo())
