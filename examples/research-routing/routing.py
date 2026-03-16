"""
Identity-aware agent routing using JamJet.

Inspired by LDP (arXiv:2603.08852). Routes tasks to the most
appropriate agent based on quality scores and cost hints.
"""
from jamjet import Agent, Workflow, tool
from pydantic import BaseModel


@tool
async def route_by_quality(task: str, complexity: str) -> str:
    """Route task based on complexity to appropriate model tier."""
    tiers = {
        "simple": "fast-agent",
        "moderate": "balanced-agent",
        "complex": "deep-agent",
    }
    return tiers.get(complexity, "balanced-agent")


fast_agent = Agent(
    name="fast-agent",
    model="claude-haiku-4-5-20251001",
    instructions="You are a fast responder. Give brief, accurate answers.",
    strategy="react",
    max_iterations=2,
)

balanced_agent = Agent(
    name="balanced-agent",
    model="claude-haiku-4-5-20251001",
    instructions="You provide thorough, well-reasoned responses.",
    strategy="plan-and-execute",
    max_iterations=4,
)

deep_agent = Agent(
    name="deep-agent",
    model="claude-haiku-4-5-20251001",
    instructions="You perform deep analysis with multiple perspectives.",
    strategy="critic",
    max_iterations=5,
)


class RoutingState(BaseModel):
    task: str
    complexity: str = "moderate"
    route: str | None = None
    result: str | None = None


workflow = Workflow("routing")


@workflow.state
class State(RoutingState):
    pass


@workflow.step
async def classify_and_route(state: State) -> State:
    route = await route_by_quality(state.task, state.complexity)
    return state.model_copy(update={"route": route})


@workflow.step
async def execute_routed(state: State) -> State:
    agents = {
        "fast-agent": fast_agent,
        "balanced-agent": balanced_agent,
        "deep-agent": deep_agent,
    }
    agent = agents.get(state.route or "balanced-agent", balanced_agent)
    result = await agent.run(state.task)
    return state.model_copy(update={"result": result.output})


async def main() -> None:
    """Run the routing workflow on sample tasks at different complexity levels."""
    import time

    tasks = [
        ("What is the capital of France?", "simple"),
        ("Compare REST and GraphQL for a real-time dashboard", "moderate"),
        ("Design a distributed consensus algorithm for Byzantine fault tolerance", "complex"),
    ]

    print(f"\n{'='*60}")
    print("Agent Routing Example")
    print(f"{'='*60}")

    for task, complexity in tasks:
        print(f"\n--- [{complexity.upper()}] {task} ---")
        start = time.monotonic()
        result = await workflow.run(State(task=task, complexity=complexity))
        elapsed = time.monotonic() - start
        print(f"Routed to: {result.state.route}")
        print(f"Result: {result.state.result[:200]}..." if result.state.result and len(result.state.result) > 200 else f"Result: {result.state.result}")
        print(f"({elapsed:.1f}s)")


if __name__ == "__main__":
    import asyncio

    asyncio.run(main())
