from __future__ import annotations

from typing import Any

from .strategy import (
    AgentCandidate,
    CoordinatorStrategy,
    Decision,
    DimensionScores,
    ScoringResult,
)


class DefaultCoordinatorStrategy(CoordinatorStrategy):
    """Built-in coordinator strategy: structured scoring with optional LLM tiebreaker."""

    def __init__(self, registry=None):
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
                filtered.append({
                    "uri": agent["uri"],
                    "reason": f"missing skills: {required - agent_skills}",
                })
                continue

            if trust_domain and agent.get("trust_domain") != trust_domain:
                filtered.append({
                    "uri": agent["uri"],
                    "reason": f"trust domain mismatch: {agent.get('trust_domain')} != {trust_domain}",
                })
                continue

            candidates.append(AgentCandidate(
                uri=agent["uri"],
                agent_card=agent.get("agent_card", {}),
                skills=list(agent_skills),
                latency_class=agent.get("latency_class"),
                cost_class=agent.get("cost_class"),
                trust_domain=agent.get("trust_domain"),
            ))

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
            results.append(ScoringResult(
                agent_uri=c.uri,
                scores=scores,
                composite=composite,
            ))

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
        selected = top_candidates[0] if top_candidates else None
        return Decision(
            selected_uri=selected.agent_uri if selected else None,
            method="structured",
            confidence=selected.composite if selected else 0.0,
            rejected=[
                {"uri": c.agent_uri, "reason": "lower score"}
                for c in top_candidates[1:]
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
