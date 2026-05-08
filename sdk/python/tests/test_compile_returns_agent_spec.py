from jamjet import Agent
from jamjet.spec import AgentSpec
from jamjet.tools.decorators import tool


@tool
async def fake(query: str) -> str:
    """Fake tool for testing."""
    return query


def test_compile_returns_agent_spec():
    a = Agent("x", model="gpt-4o", tools=[fake], strategy="react")
    spec = a.compile()
    assert isinstance(spec, AgentSpec)
    assert spec.name == "x"
    assert spec.llm.model == "gpt-4o"
    assert spec.strategy.name == "react"
    assert len(spec.tools) == 1
    assert spec.tools[0].name == "fake"


def test_compile_no_tools_empty_list():
    a = Agent("y", model="gpt-4o", tools=[], strategy="plan-and-execute")
    spec = a.compile()
    assert spec.tools == []


def test_compile_propagates_limits():
    a = Agent(
        "z",
        model="gpt-4o",
        tools=[],
        strategy="react",
        max_iterations=5,
        max_cost_usd=0.5,
        timeout_seconds=60,
    )
    spec = a.compile()
    assert spec.limits["max_iterations"] == 5
    assert spec.limits["max_cost_usd"] == 0.5
    assert spec.limits["timeout_seconds"] == 60
