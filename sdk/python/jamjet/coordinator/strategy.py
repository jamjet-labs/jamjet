from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from typing import Any


@dataclass
class AgentCandidate:
    """An agent discovered during the discovery phase."""

    uri: str
    agent_card: dict[str, Any]
    skills: list[str] = field(default_factory=list)
    latency_class: str | None = None
    cost_class: str | None = None
    trust_domain: str | None = None


@dataclass
class DimensionScores:
    """Per-dimension scoring for a single candidate."""

    capability_fit: float = 0.5
    cost_fit: float = 0.5
    latency_fit: float = 0.5
    trust_compatibility: float = 0.5
    historical_performance: float = 0.5

    def composite(self, weights: dict[str, float] | None = None) -> float:
        w = weights or {
            "capability_fit": 1.0,
            "cost_fit": 1.0,
            "latency_fit": 1.0,
            "trust_compatibility": 1.0,
            "historical_performance": 1.0,
        }
        total_weight = sum(w.values())
        if total_weight == 0:
            return 0.0
        return (
            sum(
                getattr(self, dim) * w.get(dim, 1.0)
                for dim in [
                    "capability_fit",
                    "cost_fit",
                    "latency_fit",
                    "trust_compatibility",
                    "historical_performance",
                ]
            )
            / total_weight
        )


@dataclass
class ScoringResult:
    """Scoring result for a single candidate."""

    agent_uri: str
    scores: DimensionScores
    composite: float = 0.0


@dataclass
class Decision:
    """The coordinator's final routing decision."""

    selected_uri: str | None
    method: str
    reasoning: str | None = None
    confidence: float = 0.0
    rejected: list[dict[str, str]] = field(default_factory=list)
    tiebreaker_tokens: dict[str, int] | None = None
    tiebreaker_cost: float | None = None


class CoordinatorStrategy(ABC):
    """Base class for coordinator routing strategies."""

    @abstractmethod
    async def discover(
        self,
        task: str,
        required_skills: list[str],
        preferred_skills: list[str],
        trust_domain: str | None,
        context: dict[str, Any],
    ) -> tuple[list[AgentCandidate], list[dict[str, str]]]:
        """Discover candidate agents. Returns (candidates, filtered_out)."""
        ...

    @abstractmethod
    async def score(
        self,
        task: str,
        candidates: list[AgentCandidate],
        weights: dict[str, float],
        context: dict[str, Any],
    ) -> tuple[list[ScoringResult], float]:
        """Score candidates. Returns (rankings sorted desc, spread)."""
        ...

    @abstractmethod
    async def decide(
        self,
        task: str,
        top_candidates: list[ScoringResult],
        threshold: float,
        tiebreaker_model: str,
        context: dict[str, Any],
    ) -> Decision:
        """Make final selection. Called only when spread <= threshold."""
        ...
