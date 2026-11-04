"""Data models and the discovery, scoring, and selection strategy interface."""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from typing import Any


@dataclass
class AgentCandidate:
    """An agent available for coordinator routing.

    Attributes:
        uri: Agent identifier used in rankings and routing decisions.
        agent_card: Agent Card metadata available to custom strategies.
        skills: Skills advertised by the agent.
        latency_class: Advertised latency category, if known.
        cost_class: Advertised cost category, if known.
        trust_domain: Agent's trust domain, if specified.
        reasoning_modes: Reasoning modes advertised by the agent.
    """

    uri: str
    agent_card: dict[str, Any]
    skills: list[str] = field(default_factory=list)
    latency_class: str | None = None
    cost_class: str | None = None
    trust_domain: str | None = None
    reasoning_modes: list[str] = field(default_factory=list)


@dataclass
class DimensionScores:
    """Per-dimension scores for one candidate.

    Attributes:
        capability_fit: Suitability of the agent's capabilities.
        cost_fit: Suitability of the agent's cost.
        latency_fit: Suitability of the agent's latency.
        trust_compatibility: Compatibility with trust requirements.
        historical_performance: Score based on previous performance.
    """

    capability_fit: float = 0.5
    cost_fit: float = 0.5
    latency_fit: float = 0.5
    trust_compatibility: float = 0.5
    historical_performance: float = 0.5

    def composite(self, weights: dict[str, float] | None = None) -> float:
        """Combine the dimension scores using the supplied weights.

        Args:
            weights: Weights keyed by dimension name. Missing dimensions use a
                multiplier of 1.0. None or an empty mapping uses equal weights.

        Returns:
            Weighted score sum divided by the sum of supplied weights, or 0.0
            when that sum is zero.
        """
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
    """Scores and ranking value for one candidate.

    Attributes:
        agent_uri: Identifier of the scored agent.
        scores: Individual dimension scores.
        composite: Combined score used to rank the candidate.
    """

    agent_uri: str
    scores: DimensionScores
    composite: float = 0.0


@dataclass
class Decision:
    """The coordinator's final routing decision.

    Attributes:
        selected_uri: Selected agent, or None if no agent was selected.
        method: Identifier describing how the selection was made.
        reasoning: Optional explanation of the selection.
        confidence: Confidence reported by the strategy.
        rejected: Rejected agents, represented by URI and reason mappings.
        tiebreaker_tokens: Optional input and output token counts for a tiebreaker.
        tiebreaker_cost: Optional reported cost of the tiebreaker call.
    """

    selected_uri: str | None
    method: str
    reasoning: str | None = None
    confidence: float = 0.0
    rejected: list[dict[str, str]] = field(default_factory=list)
    tiebreaker_tokens: dict[str, int] | None = None
    tiebreaker_cost: float | None = None


class CoordinatorStrategy(ABC):
    """Define the discovery, scoring, and selection stages of agent routing."""

    @abstractmethod
    async def discover(
        self,
        task: str,
        required_skills: list[str],
        preferred_skills: list[str],
        trust_domain: str | None,
        context: dict[str, Any],
    ) -> tuple[list[AgentCandidate], list[dict[str, str]]]:
        """Discover agents eligible to handle a task.

        Args:
            task: Description of the task to route.
            required_skills: Skills required of an eligible agent.
            preferred_skills: Skills preferred but not required.
            trust_domain: Requested trust domain, or None for no restriction.
            context: Additional strategy-specific discovery inputs.

        Returns:
            Eligible candidates and filtered agents with URI and reason mappings.
        """
        ...

    @abstractmethod
    async def score(
        self,
        task: str,
        candidates: list[AgentCandidate],
        weights: dict[str, float],
        context: dict[str, Any],
    ) -> tuple[list[ScoringResult], float]:
        """Score and rank discovered candidates.

        Args:
            task: Description of the task to route.
            candidates: Eligible agents returned by discovery.
            weights: Weights keyed by scoring dimension name.
            context: Additional strategy-specific scoring inputs.

        Returns:
            Rankings in descending composite-score order and the spread between
            the top two scores, as defined by the strategy.
        """
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
        """Select an agent from the ranked candidates.

        Args:
            task: Description of the task to route.
            top_candidates: Candidates in descending composite-score order.
            threshold: Maximum score spread that warrants a tiebreaker.
            tiebreaker_model: Model identifier to use for a tiebreaker.
            context: Additional strategy-specific selection inputs.

        Returns:
            The selected agent, selection method, and supporting metadata.
        """
        ...
