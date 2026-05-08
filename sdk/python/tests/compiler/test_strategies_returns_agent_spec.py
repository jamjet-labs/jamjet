from jamjet.compiler.strategies import StrategyLimits, compile_strategy_to_spec
from jamjet.spec import AgentSpec


def test_compile_strategy_to_spec_returns_agent_spec():
    spec = compile_strategy_to_spec(
        strategy_name="react",
        strategy_config={},
        tools=[],
        model="gpt-4o",
        limits=StrategyLimits(max_iterations=10, max_cost_usd=1.0, timeout_seconds=300),
        goal="hi",
        agent_id="test",
    )
    assert isinstance(spec, AgentSpec)
    assert spec.strategy.name == "react"
    assert spec.llm.model == "gpt-4o"
    assert spec.name == "test"
    assert spec.instructions == "hi"


def test_compile_strategy_to_spec_propagates_config():
    spec = compile_strategy_to_spec(
        strategy_name="critic",
        strategy_config={"rounds": 5},
        tools=[],
        model="gpt-4o",
        limits=StrategyLimits(max_iterations=3, max_cost_usd=0.1, timeout_seconds=60),
        goal="x",
        agent_id="y",
    )
    assert spec.strategy.config == {"rounds": 5}
    assert spec.limits["max_iterations"] == 3
