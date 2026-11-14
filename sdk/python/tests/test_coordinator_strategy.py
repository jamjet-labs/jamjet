from unittest.mock import AsyncMock, patch

import pytest

from jamjet.coordinator import (
    AgentCandidate,
    DimensionScores,
    ScoringResult,
)
from jamjet.coordinator.default_strategy import DefaultCoordinatorStrategy
from jamjet.llm.client import LlmResponse


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

    def test_composite_partial_weights_stays_bounded(self):
        """Partial weights must not produce a composite above 1.0."""
        scores = DimensionScores(
            capability_fit=1.0,
            cost_fit=1.0,
            latency_fit=1.0,
            trust_compatibility=1.0,
            historical_performance=1.0,
        )
        result = scores.composite({"cost_fit": 2.0})
        assert 0.0 <= result <= 1.0

    def test_composite_partial_weights_normalized(self):
        """Missing dimensions default to weight 1.0 in both sums."""
        scores = DimensionScores(
            capability_fit=1.0,
            cost_fit=0.0,
            latency_fit=1.0,
            trust_compatibility=1.0,
            historical_performance=1.0,
        )
        # 4 dims at weight 1.0, cost_fit at 2.0 → total weight = 6
        # numerator = (1*1 + 0*2 + 1*1 + 1*1 + 1*1) = 4 → 4/6
        result = scores.composite({"cost_fit": 2.0})
        assert result == pytest.approx(4.0 / 6.0)

    def test_composite_ignores_unknown_keys(self):
        scores = DimensionScores()
        result_with_junk = scores.composite({"not_a_dimension": 999.0})
        result_default = scores.composite()
        assert result_with_junk == pytest.approx(result_default)

    def test_composite_rejects_negative_weight(self):
        scores = DimensionScores()
        with pytest.raises(ValueError, match="Negative weight"):
            scores.composite({"cost_fit": -1.0})


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
    async def test_reasoning_modes_boost_capability(self, strategy):
        candidates = [
            AgentCandidate(
                uri="agent-a",
                agent_card={},
                skills=["analysis"],
                reasoning_modes=["react", "plan-and-execute"],
            ),
            AgentCandidate(
                uri="agent-b",
                agent_card={},
                skills=["analysis"],
                reasoning_modes=[],
            ),
        ]
        context = {"preferred_reasoning_modes": ["react"]}
        rankings, _ = await strategy.score("task", candidates, {}, context)
        # Agent A should score higher due to reasoning mode match
        assert rankings[0].agent_uri == "agent-a"
        assert rankings[0].composite > rankings[1].composite

    @pytest.mark.asyncio
    async def test_reasoning_modes_neutral_without_preference(self, strategy):
        candidates = [
            AgentCandidate(
                uri="agent-a",
                agent_card={},
                skills=["analysis"],
                reasoning_modes=["react"],
            ),
            AgentCandidate(
                uri="agent-b",
                agent_card={},
                skills=["analysis"],
                reasoning_modes=[],
            ),
        ]
        # No preferred_reasoning_modes in context — should be neutral
        rankings, _ = await strategy.score("task", candidates, {}, {})
        assert rankings[0].composite == pytest.approx(rankings[1].composite)

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

    @pytest.mark.asyncio
    async def test_discovered_reasoning_modes_boost_capability(self):
        registry = AsyncMock()
        registry.list_agents.return_value = [
            {"uri": "agent-default", "skills": ["analysis"]},
            {"uri": "agent-react", "skills": ["analysis"], "reasoning_modes": ["react"]},
        ]
        strategy = DefaultCoordinatorStrategy(registry=registry)
        context = {"preferred_reasoning_modes": ["react"]}

        candidates, filtered = await strategy.discover("task", ["analysis"], [], None, context)
        rankings, _ = await strategy.score("task", candidates, {}, context)

        assert filtered == []
        assert candidates[0].reasoning_modes == []
        assert candidates[1].reasoning_modes == ["react"]
        assert rankings[0].agent_uri == "agent-react"
        assert rankings[0].scores.capability_fit == pytest.approx(0.4)
        assert rankings[1].scores.capability_fit == pytest.approx(0.2)

    @pytest.mark.asyncio
    async def test_decide_skips_tiebreaker_when_spread_above_threshold(self, strategy):
        top = [
            ScoringResult(agent_uri="agent-a", scores=DimensionScores(), composite=0.9),
            ScoringResult(agent_uri="agent-b", scores=DimensionScores(), composite=0.7),
        ]
        decision = await strategy.decide("task", top, 0.1, "some-model", {})
        assert decision.method == "structured"
        assert decision.selected_uri == "agent-a"

    @pytest.mark.asyncio
    @patch("jamjet.coordinator.default_strategy.call_llm", new_callable=AsyncMock)
    async def test_decide_calls_tiebreaker_when_tied(self, mock_llm, strategy):
        mock_llm.return_value = LlmResponse(
            text='{"selected_uri": "agent-b", "reasoning": "better fit"}',
            input_tokens=50,
            output_tokens=20,
        )
        top = [
            ScoringResult(agent_uri="agent-a", scores=DimensionScores(), composite=0.80),
            ScoringResult(agent_uri="agent-b", scores=DimensionScores(), composite=0.78),
        ]
        decision = await strategy.decide("task", top, 0.05, "claude-haiku", {})
        assert decision.method == "llm_tiebreaker"
        assert decision.selected_uri == "agent-b"
        assert decision.reasoning == "better fit"
        assert decision.tiebreaker_tokens == {"input": 50, "output": 20}
        mock_llm.assert_called_once()

    @pytest.mark.asyncio
    @patch("jamjet.coordinator.default_strategy.call_llm", new_callable=AsyncMock)
    async def test_decide_falls_back_on_tiebreaker_failure(self, mock_llm, strategy):
        mock_llm.side_effect = RuntimeError("No SDK available")
        top = [
            ScoringResult(agent_uri="agent-a", scores=DimensionScores(), composite=0.80),
            ScoringResult(agent_uri="agent-b", scores=DimensionScores(), composite=0.78),
        ]
        decision = await strategy.decide("task", top, 0.05, "claude-haiku", {})
        assert decision.method == "tiebreaker_failed"
        assert decision.selected_uri == "agent-a"
        assert "failed" in decision.reasoning.lower()
