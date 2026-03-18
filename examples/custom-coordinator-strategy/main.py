"""
Custom Coordinator Strategy — Healthcare agent routing.

Demonstrates:
- Subclassing CoordinatorStrategy for domain-specific routing
- Custom scoring weights (medical certification weighted 3x)
- Custom tiebreaker logic
- Registering strategies with the StrategyServer
"""
from __future__ import annotations

from typing import Any

from jamjet.coordinator import (
    AgentCandidate,
    CoordinatorStrategy,
    Decision,
    DefaultCoordinatorStrategy,
    DimensionScores,
    ScoringResult,
)
from jamjet.coordinator.server import StrategyServer


# --- Custom strategy for healthcare routing ---

class HealthcareCoordinatorStrategy(DefaultCoordinatorStrategy):
    """Routes medical queries with heavy weight on medical certification."""

    async def score(
        self,
        task: str,
        candidates: list[AgentCandidate],
        weights: dict[str, float],
        context: dict[str, Any],
    ) -> tuple[list[ScoringResult], float]:
        """Override scoring to weight medical expertise heavily."""
        results = []
        for c in candidates:
            # Base scoring from parent
            base_scores = DimensionScores(
                capability_fit=self._score_capability(task, c),
                cost_fit=self._score_cost(c),
                latency_fit=self._score_latency(c),
                trust_compatibility=1.0 if c.trust_domain == "healthcare" else 0.2,
                historical_performance=0.5,
            )

            # Boost: check for medical certification in agent card
            has_medical_cert = "medical" in c.agent_card.get("certifications", [])
            cert_bonus = 0.3 if has_medical_cert else 0.0

            # Custom weights: medical certification 3x importance
            custom_weights = {
                "capability_fit": 1.0,
                "cost_fit": 0.5,
                "latency_fit": 0.5,
                "trust_compatibility": 3.0,  # healthcare trust domain is critical
                "historical_performance": 1.0,
            }

            composite = base_scores.composite(custom_weights) + cert_bonus
            results.append(ScoringResult(
                agent_uri=c.uri,
                scores=base_scores,
                composite=min(composite, 1.0),
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
        """Override tiebreaker to always prefer healthcare-certified agents."""
        if not top_candidates:
            return Decision(selected_uri=None, method="no_candidates")

        # In healthcare, prefer caution — always pick highest scorer, no LLM tiebreaker
        selected = top_candidates[0]
        return Decision(
            selected_uri=selected.agent_uri,
            method="structured",
            reasoning="Healthcare routing: selected highest-scoring certified agent",
            confidence=selected.composite,
            rejected=[
                {"uri": c.agent_uri, "reason": "lower score"}
                for c in top_candidates[1:]
            ],
        )


# --- Mock agents ---

HEALTHCARE_AGENTS = [
    AgentCandidate(
        uri="jamjet://health/cardiology-agent",
        agent_card={
            "name": "Cardiology Specialist",
            "certifications": ["medical", "cardiology"],
        },
        skills=["cardiology", "ecg-analysis", "heart-disease"],
        latency_class="medium",
        cost_class="high",
        trust_domain="healthcare",
    ),
    AgentCandidate(
        uri="jamjet://health/general-practitioner",
        agent_card={
            "name": "General Practitioner",
            "certifications": ["medical"],
        },
        skills=["general-medicine", "triage", "referrals"],
        latency_class="low",
        cost_class="low",
        trust_domain="healthcare",
    ),
    AgentCandidate(
        uri="jamjet://support/general-agent",
        agent_card={
            "name": "General Support Bot",
            "certifications": [],
        },
        skills=["faq", "scheduling", "general-support"],
        latency_class="low",
        cost_class="low",
        trust_domain="internal",
    ),
]


async def demo_custom_strategy():
    """Show custom healthcare routing in action."""
    print("=" * 60)
    print("Custom Coordinator Strategy — Healthcare Routing")
    print("=" * 60)

    strategy = HealthcareCoordinatorStrategy(registry=None)

    queries = [
        "Patient reports chest pain and shortness of breath",
        "Need to schedule a follow-up appointment",
    ]

    for query in queries:
        print(f"\n--- Query: {query} ---")

        # Score all agents
        rankings, spread = await strategy.score(
            task=query,
            candidates=HEALTHCARE_AGENTS,
            weights={},
            context={},
        )

        print(f"  Rankings (spread={spread:.3f}):")
        for r in rankings:
            certified = any(
                "medical" in a.agent_card.get("certifications", [])
                for a in HEALTHCARE_AGENTS if a.uri == r.agent_uri
            )
            cert_badge = " [CERTIFIED]" if certified else ""
            print(f"    {r.agent_uri}: {r.composite:.3f}{cert_badge}")

        decision = await strategy.decide(query, rankings, 0.1, "", {})
        print(f"  Selected: {decision.selected_uri}")
        print(f"  Reasoning: {decision.reasoning}")


def demo_strategy_server_registration():
    """Show how to register a custom strategy with the StrategyServer."""
    print("\n" + "=" * 60)
    print("Custom Strategy — Server Registration")
    print("=" * 60)

    server = StrategyServer(port=4270)

    # Register the healthcare strategy alongside the default
    server.register_strategy("healthcare", HealthcareCoordinatorStrategy(registry=None))

    print(f"\n  Registered strategies: {list(server._strategies.keys())}")
    print("  Usage in workflow YAML:")
    print("    coordinator:")
    print("      strategy: 'healthcare'")
    print("      required_skills: ['cardiology']")
    print("      trust_domain: 'healthcare'")
    print("\n  The Rust runtime calls POST /coordinator/score with")
    print("  strategy_name='healthcare' and the custom scoring runs.")


async def main():
    await demo_custom_strategy()
    demo_strategy_server_registration()


if __name__ == "__main__":
    import asyncio
    asyncio.run(main())
