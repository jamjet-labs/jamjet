"""
Deliberative Collective Intelligence (DCI) pattern using JamJet.

Inspired by arXiv:2603.11781. Four reasoning archetypes collaborate
through structured deliberation to solve complex problems.
"""
from jamjet import Agent, Workflow, tool
from pydantic import BaseModel


# 4 DCI reasoning archetypes as JamJet agents
@tool
async def propose_frame(problem: str) -> str:
    """Propose a problem framing."""
    return f"Framing: {problem}"


framer = Agent(
    name="framer",
    model="claude-haiku-4-5-20251001",
    tools=[propose_frame],
    instructions="You are the Framer. Structure the problem space. Identify key dimensions and constraints.",
    strategy="react",
    max_iterations=3,
)

explorer = Agent(
    name="explorer",
    model="claude-haiku-4-5-20251001",
    instructions="You are the Explorer. Generate alternative approaches. Think divergently.",
    strategy="react",
    max_iterations=3,
)

challenger = Agent(
    name="challenger",
    model="claude-haiku-4-5-20251001",
    instructions="You are the Challenger. Stress-test proposals. Find weaknesses and edge cases.",
    strategy="critic",
    max_iterations=3,
)

integrator = Agent(
    name="integrator",
    model="claude-haiku-4-5-20251001",
    instructions="You are the Integrator. Synthesize insights from all perspectives into a coherent conclusion.",
    strategy="plan-and-execute",
    max_iterations=4,
)


class DeliberationState(BaseModel):
    problem: str
    framing: str | None = None
    alternatives: str | None = None
    challenges: str | None = None
    synthesis: str | None = None


workflow = Workflow("deliberation")


@workflow.state
class State(DeliberationState):
    pass


@workflow.step
async def frame(state: State) -> State:
    result = await framer.run(f"Frame this problem: {state.problem}")
    return state.model_copy(update={"framing": result.output})


@workflow.step
async def explore(state: State) -> State:
    result = await explorer.run(
        f"Problem: {state.problem}\nFraming: {state.framing}\nGenerate alternative approaches."
    )
    return state.model_copy(update={"alternatives": result.output})


@workflow.step
async def challenge(state: State) -> State:
    result = await challenger.run(
        f"Problem: {state.problem}\nFraming: {state.framing}\n"
        f"Alternatives: {state.alternatives}\nChallenge these proposals."
    )
    return state.model_copy(update={"challenges": result.output})


@workflow.step
async def integrate(state: State) -> State:
    result = await integrator.run(
        f"Problem: {state.problem}\nFraming: {state.framing}\n"
        f"Alternatives: {state.alternatives}\nChallenges: {state.challenges}\n"
        "Synthesize all perspectives into a final recommendation."
    )
    return state.model_copy(update={"synthesis": result.output})


async def main() -> None:
    """Run the deliberation workflow on a sample problem."""
    import time

    problem = (
        "How should a city redesign its public transit system to reduce "
        "carbon emissions by 50% while maintaining accessibility for low-income residents?"
    )

    print(f"\n{'='*60}")
    print("DCI Deliberation Example")
    print(f"{'='*60}")
    print(f"\nProblem: {problem}\n")

    start = time.monotonic()
    result = await workflow.run(State(problem=problem))
    elapsed = time.monotonic() - start

    print(f"\n--- Framing ---\n{result.state.framing}")
    print(f"\n--- Alternatives ---\n{result.state.alternatives}")
    print(f"\n--- Challenges ---\n{result.state.challenges}")
    print(f"\n--- Synthesis ---\n{result.state.synthesis}")
    print(f"\nCompleted in {elapsed:.1f}s")


if __name__ == "__main__":
    import asyncio

    asyncio.run(main())
