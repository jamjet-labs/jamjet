import pytest
from jamjet.coordinator import (
    AgentCandidate,
    DimensionScores,
    ScoringResult,
)
from jamjet.coordinator.default_strategy import DefaultCoordinatorStrategy


@pytest.fixture
def strategy():
    return DefaultCoordinatorStrategy(registry=None)


@pytest.fixture
def candidates():
    return [
        AgentCandidate(
            uri="jamjet://org/agent-a",
            agent_card={"name": "Agent A"},
            skills=["data-analysis", "statistics"],
            latency_class="low",
            cost_class="low",
            trust_domain="internal",
        ),
        AgentCandidate(
            uri="jamjet://org/agent-b",
            agent_card={"name": "Agent B"},
            skills=["data-analysis"],
            latency_class="medium",
            cost_class="medium",
            trust_domain="internal",
        ),
    ]


class TestDimensionScores:
    def test_composite_equal_weights(self):
        scores = DimensionScores(
            capability_fit=1.0,
            cost_fit=0.5,
            latency_fit=0.5,
            trust_compatibility=1.0,
            historical_performance=0.5,
        )
        assert scores.composite() == pytest.approx(0.7)

    def test_composite_custom_weights(self):
        scores = DimensionScores(capability_fit=1.0, cost_fit=0.0)
        result = scores.composite(
            {
                "capability_fit": 2.0,
                "cost_fit": 0.0,
                "latency_fit": 0.0,
                "trust_compatibility": 0.0,
                "historical_performance": 0.0,
            }
        )
        assert result == pytest.approx(1.0)

    def test_composite_zero_weights(self):
        scores = DimensionScores()
        result = scores.composite(
            {
                "capability_fit": 0.0,
                "cost_fit": 0.0,
                "latency_fit": 0.0,
                "trust_compatibility": 0.0,
                "historical_performance": 0.0,
            }
        )
        assert result == 0.0


class TestDefaultStrategy:
    @pytest.mark.asyncio
    async def test_score_returns_ranked_results(self, strategy, candidates):
        rankings, spread = await strategy.score(
            task="Analyze data",
            candidates=candidates,
            weights={},
            context={},
        )
        assert len(rankings) == 2
        assert rankings[0].composite >= rankings[1].composite
        assert spread >= 0.0

    @pytest.mark.asyncio
    async def test_score_missing_latency_class_gets_neutral(self, strategy):
        candidates = [
            AgentCandidate(
                uri="jamjet://org/agent-no-class",
                agent_card={},
                skills=["data-analysis"],
            ),
        ]
        rankings, _ = await strategy.score("task", candidates, {}, {})
        assert rankings[0].scores.latency_fit == pytest.approx(0.5)
        assert rankings[0].scores.cost_fit == pytest.approx(0.5)

    @pytest.mark.asyncio
    async def test_decide_selects_top_candidate(self, strategy):
        top = [
            ScoringResult(agent_uri="agent-a", scores=DimensionScores(), composite=0.9),
            ScoringResult(agent_uri="agent-b", scores=DimensionScores(), composite=0.7),
        ]
        decision = await strategy.decide("task", top, 0.1, "model", {})
        assert decision.selected_uri == "agent-a"
        assert decision.method == "structured"
        assert len(decision.rejected) == 1

    @pytest.mark.asyncio
    async def test_discover_returns_empty_without_registry(self, strategy):
        candidates, filtered = await strategy.discover("task", ["skill"], [], None, {})
        assert candidates == []
        assert filtered == []
