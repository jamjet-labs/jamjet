"""Registry filtering and heuristic scoring with an optional LLM tiebreaker."""

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
    """Route using advertised agent metadata and optional model-based selection."""

    def __init__(self, registry: Any = None) -> None:
        """Initialize the strategy with an optional agent registry.

        Args:
            registry: Object exposing an async list_agents() method that returns
                agent mappings. Discovery returns no candidates when omitted.
        """
        self._registry = registry

    async def discover(
        self,
        task: str,
        required_skills: list[str],
        preferred_skills: list[str],
        trust_domain: str | None,
        context: dict[str, Any],
    ) -> tuple[list[AgentCandidate], list[dict[str, str]]]:
        """Filter registry agents by required skills and trust domain.

        Args:
            task: Task description; unused by this implementation.
            required_skills: Skills every returned agent must advertise.
            preferred_skills: Optional skills; unused by this implementation.
            trust_domain: Domain agents must match when this value is nonempty.
            context: Additional inputs; unused by this implementation.

        Returns:
            Eligible candidates and excluded agents with URI and reason mappings.
            Both lists are empty when no registry is configured.
        """
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
        """Rank candidates using skill counts and advertised metadata.

        Args:
            task: Task description passed to the capability scoring hook. The
                default hook does not inspect its text.
            candidates: Agents to score.
            weights: Dimension weights passed to DimensionScores.composite().
                An empty mapping selects equal weights.
            context: May contain preferred_reasoning_modes, a list of mode names
                used to add a capability bonus for matching candidates.

        Returns:
            Rankings sorted by descending composite score and the difference
            between the first two scores, or 1.0 with fewer than two candidates.
        """
        results = []
        for c in candidates:
            reasoning_bonus = self._score_reasoning_modes(c, context)
            scores = DimensionScores(
                capability_fit=min(self._score_capability(task, c) + reasoning_bonus, 1.0),
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
        """Select the top candidate, using a model when the top scores are close.

        Args:
            task: Task description supplied to the tiebreaker prompt.
            top_candidates: Candidates in descending composite-score order.
            threshold: Maximum difference between the top two scores that
                enables a tiebreaker when a model is configured.
            tiebreaker_model: Model identifier, or an empty string to disable it.
            context: May contain agent_card_summaries, a mapping from agent URI
                to a description included in the tiebreaker prompt.

        Returns:
            A decision with no selection for an empty list, the top-ranked agent
            for structured selection, or the tiebreaker's result.
        """
        if not top_candidates:
            return Decision(selected_uri=None, method="no_candidates", confidence=0.0)

        selected = top_candidates[0]

        # Check if scores are close enough to warrant a tiebreaker
        spread = (top_candidates[0].composite - top_candidates[1].composite) if len(top_candidates) >= 2 else 1.0

        if spread <= threshold and tiebreaker_model and len(top_candidates) >= 2:
            return await self._llm_tiebreaker(task, top_candidates, tiebreaker_model, context)

        return Decision(
            selected_uri=selected.agent_uri,
            method="structured",
            confidence=selected.composite,
            rejected=[{"uri": c.agent_uri, "reason": "lower score"} for c in top_candidates[1:]],
        )

    async def _llm_tiebreaker(
        self,
        task: str,
        candidates: list[ScoringResult],
        model: str,
        context: dict[str, Any],
        max_candidates: int = 3,
    ) -> Decision:
        """Ask a model to select among the leading candidates.

        Args:
            task: Task description included in the prompt.
            candidates: Nonempty rankings in descending composite-score order.
            model: Model identifier passed to call_llm().
            context: Optional agent_card_summaries keyed by agent URI.
            max_candidates: Maximum number of leading candidates shown.

        Returns:
            A model-backed decision with token counts. An invalid selected URI
            falls back to the first candidate. A failed call or response parsing
            returns a tiebreaker_failed decision selecting that same candidate.
        """
        tied = candidates[:max_candidates]

        # Build prompt with Agent Card summaries
        card_summaries = context.get("agent_card_summaries", {})
        candidate_summaries = []
        for i, c in enumerate(tied, 1):
            s = c.scores
            card_info = card_summaries.get(c.agent_uri, "")
            card_line = f"   Description: {card_info}\n" if card_info else ""
            candidate_summaries.append(
                f"{i}. URI: {c.agent_uri}\n"
                f"{card_line}"
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

            selected_candidate = next((c for c in tied if c.agent_uri == selected_uri), tied[0])
            return Decision(
                selected_uri=selected_uri,
                method="llm_tiebreaker",
                reasoning=reasoning,
                confidence=selected_candidate.composite,
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
                rejected=[{"uri": c.agent_uri, "reason": "lower score"} for c in candidates[1:]],
            )

    def _score_capability(self, task: str, candidate: AgentCandidate) -> float:
        """Return 0.3 without skills, otherwise skill count / 5 capped at 1.0."""
        if not candidate.skills:
            return 0.3
        return min(len(candidate.skills) / 5.0, 1.0)

    def _score_cost(self, candidate: AgentCandidate) -> float:
        """Map low, medium, and high cost to scores; unknown cost scores 0.5."""
        mapping = {"low": 1.0, "medium": 0.7, "high": 0.4}
        return mapping.get(candidate.cost_class or "", 0.5)

    def _score_latency(self, candidate: AgentCandidate) -> float:
        """Map low, medium, and high latency to scores; unknown latency scores 0.5."""
        mapping = {"low": 1.0, "medium": 0.7, "high": 0.4}
        return mapping.get(candidate.latency_class or "", 0.5)

    def _score_reasoning_modes(self, candidate: AgentCandidate, context: dict[str, Any]) -> float:
        """Add up to 0.2 for overlap with context's preferred_reasoning_modes."""
        preferred = context.get("preferred_reasoning_modes", [])
        if not preferred or not candidate.reasoning_modes:
            return 0.0
        overlap = set(preferred) & set(candidate.reasoning_modes)
        return 0.2 * len(overlap) / len(preferred)
