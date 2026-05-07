import pytest
from pydantic import ValidationError

from jamjet.spec import AgentSpec, AgentStrategy, LLMConfig, MemoryConfig, ToolSpec


def _llm():
    return LLMConfig(provider="openai", model="gpt-4o")


def test_default_strategy():
    s = AgentStrategy(name="react")
    assert s.config == {}


def test_minimal_agent():
    a = AgentSpec(name="x", llm=_llm())
    assert a.kind == "agent"
    assert a.ir_version == "1.0"
    assert a.tools == []
    assert a.memory is None
    assert a.strategy.name == "plan-and-execute"


def test_agent_with_tools_and_memory():
    a = AgentSpec(
        name="planner",
        llm=_llm(),
        tools=[ToolSpec(name="t", description="d", input_schema={}, handler_ref="m:f")],
        memory=MemoryConfig(),
        strategy=AgentStrategy(name="critic", config={"rounds": 3}),
        instructions="You plan things.",
    )
    assert len(a.tools) == 1
    assert a.strategy.config == {"rounds": 3}


def test_invalid_strategy_rejected():
    with pytest.raises(ValidationError):
        AgentStrategy(name="not-a-strategy")  # type: ignore[arg-type]


def test_round_trip_json():
    a = AgentSpec(name="x", llm=_llm(), memory=MemoryConfig())
    assert AgentSpec.model_validate_json(a.model_dump_json()) == a
