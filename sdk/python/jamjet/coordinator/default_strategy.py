from __future__ import annotations

import json
import logging
from typing import Any

from jamjet.llm import call_llm

from .strategy import (
    AgentCandidate,
    CoordinatorStrategy,
    Decision,
    DimensionScores,
    ScoringResult,
)

logger = logging.getLogger(__name__)


class DefaultCoordinatorStrategy(CoordinatorStrategy):
    """Built-in coordinator strategy: structured scoring with optional LLM tiebreaker."""

    def __init__(self, registry: Any = None) -> None:
        self._registry = registry

    async def discover(
        self,
        task: str,
        required_skills: list[str],
        preferred_skills: list[str],
        trust_domain: str | None,
        context: dict[str, Any],
    ) -> tuple[list[AgentCandidate], list[dict[str, str]]]:
        if self._registry is None:
            return [], []

        all_agents = await self._registry.list_agents()
        candidates = []
        filtered = []

        for agent in all_agents:
            agent_skills = set(agent.get("skills", []))
            required = set(required_skills)

            if not required.issubset(agent_skills):
                filtered.append(
                    {
                        "uri": agent["uri"],
                        "reason": f"missing skills: {required - agent_skills}",
                    }
                )
                continue

            if trust_domain and agent.get("trust_domain") != trust_domain:
                filtered.append(
                    {
                        "uri": agent["uri"],
                        "reason": f"trust domain mismatch: {agent.get('trust_domain')} != {trust_domain}",
                    }
                )
                continue

            candidates.append(
                AgentCandidate(
                    uri=agent["uri"],
                    agent_card=agent.get("agent_card", {}),
                    skills=list(agent_skills),
                    latency_class=agent.get("latency_class"),
                    cost_class=agent.get("cost_class"),
                    trust_domain=agent.get("trust_domain"),
                )
            )

        return candidates, filtered

    async def score(
        self,
        task: str,
        candidates: list[AgentCandidate],
        weights: dict[str, float],
        context: dict[str, Any],
    ) -> tuple[list[ScoringResult], float]:
        results = []
        for c in candidates:
            scores = DimensionScores(
                capability_fit=self._score_capability(task, c),
                cost_fit=self._score_cost(c),
                latency_fit=self._score_latency(c),
                trust_compatibility=1.0 if c.trust_domain else 0.5,
                historical_performance=0.5,
            )
            composite = scores.composite(weights if weights else None)
            results.append(
                ScoringResult(
                    agent_uri=c.uri,
                    scores=scores,
                    composite=composite,
                )
            )

        results.sort(key=lambda r: r.composite, reverse=True)
        spread = (results[0].composite - results[1].composite) if len(results) >= 2 else 1.0
        return results, spread

    async def decide(
        self,
        task: str,
        top_candidates: list[ScoringResult],
        threshold: float,
        tiebreaker_model: str,
        context: dict[str, Any],
    ) -> Decision:
        if not top_candidates:
            return Decision(selected_uri=None, method="no_candidates", confidence=0.0)

        selected = top_candidates[0]

        # Check if scores are close enough to warrant a tiebreaker
        spread = (
            (top_candidates[0].composite - top_candidates[1].composite)
            if len(top_candidates) >= 2
            else 1.0
        )

        if spread <= threshold and tiebreaker_model and len(top_candidates) >= 2:
            return await self._llm_tiebreaker(
                task, top_candidates, tiebreaker_model, context
            )

        return Decision(
            selected_uri=selected.agent_uri,
            method="structured",
            confidence=selected.composite,
            rejected=[
                {"uri": c.agent_uri, "reason": "lower score"}
                for c in top_candidates[1:]
            ],
        )

    async def _llm_tiebreaker(
        self,
        task: str,
        candidates: list[ScoringResult],
        model: str,
        context: dict[str, Any],
        max_candidates: int = 3,
    ) -> Decision:
        """Call an LLM to break a tie between closely-scored candidates."""
        tied = candidates[:max_candidates]

        # Build prompt
        candidate_summaries = []
        for i, c in enumerate(tied, 1):
            s = c.scores
            candidate_summaries.append(
                f"{i}. URI: {c.agent_uri}\n"
                f"   Composite: {c.composite:.3f}\n"
                f"   Scores: capability={s.capability_fit:.2f}, "
                f"cost={s.cost_fit:.2f}, latency={s.latency_fit:.2f}, "
                f"trust={s.trust_compatibility:.2f}"
            )

        prompt = (
            f"You are selecting the best AI agent for a task.\n\n"
            f"Task: {task}\n\n"
            f"Candidates (scores are very close):\n"
            f"{''.join(candidate_summaries)}\n\n"
            f"Return ONLY valid JSON:\n"
            f'{{"selected_uri": "<uri of best agent>", '
            f'"reasoning": "<one sentence why>"}}'
        )

        try:
            resp = await call_llm(model=model, prompt=prompt, max_tokens=256)
            parsed = json.loads(resp.text.strip())
            selected_uri = parsed.get("selected_uri", "")
            reasoning = parsed.get("reasoning", "")

            # Validate that selected_uri is actually one of the candidates
            valid_uris = {c.agent_uri for c in tied}
            if selected_uri not in valid_uris:
                selected_uri = tied[0].agent_uri
                reasoning = f"LLM returned invalid URI, falling back. Original: {reasoning}"

            return Decision(
                selected_uri=selected_uri,
                method="llm_tiebreaker",
                reasoning=reasoning,
                confidence=tied[0].composite,
                rejected=[
                    {"uri": c.agent_uri, "reason": "not selected by tiebreaker"}
                    for c in tied
                    if c.agent_uri != selected_uri
                ],
                tiebreaker_tokens={
                    "input": resp.input_tokens,
                    "output": resp.output_tokens,
                },
                tiebreaker_cost=None,
            )
        except Exception as e:
            logger.warning("LLM tiebreaker failed: %s", e)
            # Fall back to structured selection
            return Decision(
                selected_uri=tied[0].agent_uri,
                method="tiebreaker_failed",
                reasoning=f"LLM tiebreaker failed: {e}",
                confidence=tied[0].composite,
                rejected=[
                    {"uri": c.agent_uri, "reason": "lower score"}
                    for c in candidates[1:]
                ],
            )

    def _score_capability(self, task: str, candidate: AgentCandidate) -> float:
        if not candidate.skills:
            return 0.3
        return min(len(candidate.skills) / 5.0, 1.0)

    def _score_cost(self, candidate: AgentCandidate) -> float:
        mapping = {"low": 1.0, "medium": 0.7, "high": 0.4}
        return mapping.get(candidate.cost_class or "", 0.5)

    def _score_latency(self, candidate: AgentCandidate) -> float:
        mapping = {"low": 1.0, "medium": 0.7, "high": 0.4}
        return mapping.get(candidate.latency_class or "", 0.5)
