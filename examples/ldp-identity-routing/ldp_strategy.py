"""
LDP-aware Coordinator Strategy.

Subclasses DefaultCoordinatorStrategy to use LDP identity card metadata
(reasoning_profile, cost_profile, quality_score, domains) for routing decisions.

Scoring mirrors the LDP paper's routing algorithm (arXiv:2603.08852, RQ1):
- Easy tasks → prefer fast, cheap agents
- Hard tasks → prefer analytical, high-quality agents
- Domain-specific tasks → prefer agents with matching domain expertise
"""
from __future__ import annotations

import re
from typing import Any

from jamjet.coordinator import (
    AgentCandidate,
    Decision,
    DefaultCoordinatorStrategy,
    DimensionScores,
    ScoringResult,
)

# Keywords signaling complex, multi-step analysis
HARD_SIGNALS = [
    "analyze", "impact", "implications", "evaluate", "compare",
    "sustainability", "multi-step", "trade-off", "assess", "critique",
    "systemic", "structural", "long-term", "comprehensive",
]

# Known domains for domain-matching
KNOWN_DOMAINS = ["finance", "healthcare", "legal", "tax", "compliance"]


def classify_difficulty(task: str) -> str:
    """Classify task difficulty using lightweight heuristics (no LLM call).

    Returns "easy", "medium", or "hard".
    """
    task_lower = task.lower()
    word_count = len(task.split())
    hard_matches = sum(1 for s in HARD_SIGNALS if s in task_lower)

    if hard_matches >= 2 or word_count > 20:
        return "hard"
    if hard_matches == 1 or word_count > 12:
        return "medium"
    return "easy"


def detect_domains(task: str) -> list[str]:
    """Detect domain keywords in the task."""
    task_lower = task.lower()
    return [d for d in KNOWN_DOMAINS if d in task_lower or _domain_synonyms(d, task_lower)]


def _domain_synonyms(domain: str, text: str) -> bool:
    """Check for domain synonym matches."""
    synonyms = {
        "finance": ["financial", "investment", "portfolio", "market", "debt", "equity"],
        "healthcare": ["medical", "clinical", "patient", "diagnosis"],
        "legal": ["law", "regulation", "statute", "compliance"],
        "tax": ["ira", "roth", "deduction", "bracket", "irmaa"],
        "compliance": ["regulatory", "audit", "governance"],
    }
    return any(s in text for s in synonyms.get(domain, []))


class LdpCoordinatorStrategy(DefaultCoordinatorStrategy):
    """Routes tasks using LDP identity card metadata.

    Reads ldp.* labels from AgentCandidate.agent_card["labels"] to compute
    a composite score based on:
    - reasoning_profile fit (analytical vs fast vs domain-expert)
    - cost efficiency (prefer cheap agents for easy tasks)
    - quality weighting (scale by task difficulty)
    - domain match (bonus for matching domain expertise)
    """

    async def score(
        self,
        task: str,
        candidates: list[AgentCandidate],
        weights: dict[str, float],
        context: dict[str, Any],
    ) -> tuple[list[ScoringResult], float]:
        difficulty = classify_difficulty(task)
        detected_domains = detect_domains(task)

        results = []
        for c in candidates:
            labels = c.agent_card.get("labels", {})
            reasoning = labels.get("ldp.reasoning_profile", "")
            cost = labels.get("ldp.cost_profile", "")
            quality = float(labels.get("ldp.quality_score", "0.5"))
            agent_domains = [
                d.strip() for d in labels.get("ldp.domains", "").split(",") if d.strip()
            ]
            latency_ms = _parse_latency(labels.get("ldp.latency_profile", ""))

            # --- Reasoning profile fit ---
            reasoning_fit = 0.5  # neutral default
            if difficulty == "hard" and reasoning == "analytical":
                reasoning_fit = 1.0
            elif difficulty == "easy" and reasoning == "fast":
                reasoning_fit = 1.0
            elif difficulty == "medium" and reasoning == "domain-expert":
                reasoning_fit = 0.8
            elif difficulty == "hard" and reasoning == "domain-expert":
                reasoning_fit = 0.7

            # --- Cost efficiency ---
            cost_fit = 0.5
            if difficulty == "easy":
                cost_fit = {"low": 1.0, "medium": 0.5, "high": 0.1}.get(cost, 0.5)
            elif difficulty == "hard":
                cost_fit = {"low": 0.3, "medium": 0.6, "high": 0.8}.get(cost, 0.5)

            # --- Quality weighting (scaled by difficulty) ---
            if difficulty == "hard":
                quality_fit = quality  # high quality matters
            elif difficulty == "easy":
                quality_fit = 0.5 + (1.0 - quality) * 0.3  # prefer cheaper
            else:
                quality_fit = 0.5 + quality * 0.3

            # --- Domain match bonus ---
            domain_bonus = 0.0
            if detected_domains and agent_domains:
                overlap = set(detected_domains) & set(agent_domains)
                if overlap:
                    domain_bonus = 0.25 * len(overlap)

            # --- Latency fit ---
            latency_fit = 0.5
            if latency_ms > 0:
                if difficulty == "easy":
                    latency_fit = max(0.0, 1.0 - latency_ms / 10000)
                else:
                    latency_fit = max(0.2, 1.0 - latency_ms / 20000)

            scores = DimensionScores(
                capability_fit=reasoning_fit,
                cost_fit=cost_fit,
                latency_fit=latency_fit,
                trust_compatibility=1.0 if c.trust_domain == "research" else 0.3,
                historical_performance=quality_fit,
            )

            ldp_weights = {
                "capability_fit": 2.0,
                "cost_fit": 1.5 if difficulty == "easy" else 0.5,
                "latency_fit": 1.0 if difficulty == "easy" else 0.3,
                "trust_compatibility": 1.0,
                "historical_performance": 2.5 if difficulty == "hard" else 1.0,
            }

            composite = min(scores.composite(ldp_weights) + domain_bonus, 1.0)
            results.append(ScoringResult(
                agent_uri=c.uri, scores=scores, composite=composite,
            ))

        results.sort(key=lambda r: r.composite, reverse=True)
        spread = (results[0].composite - results[-1].composite) if len(results) >= 2 else 1.0
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
            return Decision(selected_uri=None, method="no_candidates")
        difficulty = classify_difficulty(task)
        selected = top_candidates[0]
        return Decision(
            selected_uri=selected.agent_uri,
            method="structured",
            reasoning=(
                f"LDP routing: difficulty={difficulty}, "
                f"best fit={selected.agent_uri} (score={selected.composite:.3f})"
            ),
            confidence=selected.composite,
            rejected=[
                {"uri": c.agent_uri, "reason": f"score={c.composite:.3f}"}
                for c in top_candidates[1:]
            ],
        )


def _parse_latency(profile: str) -> float:
    """Extract p50 latency in ms from profile string like 'p50:3000ms'."""
    match = re.search(r"p50:(\d+)ms", profile)
    return float(match.group(1)) if match else 0.0
