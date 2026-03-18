"""
Custom Coordinator Strategy -- Healthcare agent routing.

Demonstrates:
- Subclassing CoordinatorStrategy for domain-specific routing
- Custom scoring weights (medical certification 3x)
- Custom decision logic (no LLM tiebreaker for safety)
- Registering strategies with the StrategyServer
"""
from __future__ import annotations

import asyncio
from typing import Any

from jamjet.coordinator import (
    AgentCandidate,
    Decision,
    DefaultCoordinatorStrategy,
    DimensionScores,
    ScoringResult,
)
from jamjet.coordinator.server import StrategyServer


class HealthcareCoordinatorStrategy(DefaultCoordinatorStrategy):
    """Routes medical queries with heavy weight on certification and trust domain."""

    async def score(
        self, task: str, candidates: list[AgentCandidate],
        weights: dict[str, float], context: dict[str, Any],
    ) -> tuple[list[ScoringResult], float]:
        results = []
        for c in candidates:
            scores = DimensionScores(
                capability_fit=self._score_capability(task, c),
                cost_fit=self._score_cost(c),
                latency_fit=self._score_latency(c),
                trust_compatibility=1.0 if c.trust_domain == "healthcare" else 0.2,
                historical_performance=0.5,
            )
            has_cert = "medical" in c.agent_card.get("certifications", [])
            cert_bonus = 0.3 if has_cert else 0.0
            custom_weights = {
                "capability_fit": 1.0, "cost_fit": 0.5, "latency_fit": 0.5,
                "trust_compatibility": 3.0, "historical_performance": 1.0,
            }
            composite = min(scores.composite(custom_weights) + cert_bonus, 1.0)
            results.append(ScoringResult(agent_uri=c.uri, scores=scores, composite=composite))

        results.sort(key=lambda r: r.composite, reverse=True)
        spread = (results[0].composite - results[1].composite) if len(results) >= 2 else 1.0
        return results, spread

    async def decide(
        self, task: str, top_candidates: list[ScoringResult],
        threshold: float, tiebreaker_model: str, context: dict[str, Any],
    ) -> Decision:
        if not top_candidates:
            return Decision(selected_uri=None, method="no_candidates")
        selected = top_candidates[0]
        return Decision(
            selected_uri=selected.agent_uri, method="structured",
            reasoning="Healthcare: highest-scoring certified agent (no tiebreaker)",
            confidence=selected.composite,
            rejected=[{"uri": c.agent_uri, "reason": "lower score"} for c in top_candidates[1:]],
        )


AGENTS = [
    AgentCandidate(
        uri="jamjet://health/cardiology-agent",
        agent_card={"name": "Cardiology Specialist", "certifications": ["medical", "cardiology"]},
        skills=["cardiology", "ecg-analysis", "heart-disease"],
        latency_class="medium", cost_class="high", trust_domain="healthcare",
    ),
    AgentCandidate(
        uri="jamjet://health/general-practitioner",
        agent_card={"name": "General Practitioner", "certifications": ["medical"]},
        skills=["general-medicine", "triage", "referrals"],
        latency_class="low", cost_class="low", trust_domain="healthcare",
    ),
    AgentCandidate(
        uri="jamjet://support/chatbot",
        agent_card={"name": "Support Chatbot", "certifications": []},
        skills=["faq", "scheduling"],
        latency_class="low", cost_class="low", trust_domain="internal",
    ),
]


async def demo():
    print("=" * 60)
    print("Custom Coordinator Strategy -- Healthcare Routing")
    print("=" * 60)

    strategy = HealthcareCoordinatorStrategy(registry=None)
    queries = [
        "Patient reports chest pain and shortness of breath",
        "Need to schedule a follow-up appointment",
    ]

    for query in queries:
        print(f"\n--- {query} ---")
        rankings, spread = await strategy.score(task=query, candidates=AGENTS, weights={}, context={})
        print(f"  Rankings (spread={spread:.3f}):")
        for r in rankings:
            agent = next(a for a in AGENTS if a.uri == r.agent_uri)
            certs = agent.agent_card.get("certifications", [])
            badge = " [CERTIFIED]" if "medical" in certs else ""
            print(f"    {r.agent_uri}: {r.composite:.3f}{badge} [{agent.trust_domain}]")
        decision = await strategy.decide(query, rankings, 0.1, "", {})
        print(f"  -> {decision.selected_uri} ({decision.reasoning})")

    print("\n" + "=" * 60)
    print("Strategy Server Registration")
    print("=" * 60)
    server = StrategyServer(port=4270)
    server.register_strategy("healthcare", HealthcareCoordinatorStrategy(registry=None))
    print(f"\n  Strategies: {list(server._strategies.keys())}")
    print("  Use 'strategy: healthcare' in coordinator YAML to activate.")


if __name__ == "__main__":
    asyncio.run(demo())
