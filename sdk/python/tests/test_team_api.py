"""T6-1 — the ``Team`` / ``Sequential`` / ``Parallel`` / ``Loop`` composition API.

These tests assert ONLY construction + the composition shape (which agents, in
what order, with what merge / coordinator). Orchestration (``run`` / ``run_durable``)
is covered by the pattern tests (T6-3/T6-4). No engine, no network.
"""

from __future__ import annotations

from jamjet.agents.agent import Agent
from jamjet.team import (
    Collect,
    Custom,
    First,
    Loop,
    MergeStrategy,
    Parallel,
    Sequential,
    Team,
    TeamResult,
)


def _agent(name: str) -> Agent:
    return Agent(name, model="anthropic/claude-sonnet-4-6", tools=[])


# ── Sequential ────────────────────────────────────────────────────────────────


def test_sequential_constructs_and_carries_agents_in_order() -> None:
    a, b, c = _agent("a"), _agent("b"), _agent("c")
    seq = Sequential([a, b, c])
    assert seq.agents == [a, b, c]
    assert seq.pattern == "sequential"


def test_sequential_rejects_empty() -> None:
    import pytest

    with pytest.raises(ValueError, match="at least one"):
        Sequential([])


# ── Parallel ──────────────────────────────────────────────────────────────────


def test_parallel_constructs_with_default_collect_merge() -> None:
    a, b = _agent("a"), _agent("b")
    par = Parallel([a, b])
    assert par.agents == [a, b]
    assert par.pattern == "parallel"
    assert isinstance(par.merge, Collect)


def test_parallel_accepts_merge_strategy_instance() -> None:
    par = Parallel([_agent("a")], merge=First())
    assert isinstance(par.merge, First)


def test_parallel_accepts_string_merge_names() -> None:
    assert isinstance(Parallel([_agent("a")], merge="collect").merge, Collect)
    assert isinstance(Parallel([_agent("a")], merge="first").merge, First)


def test_parallel_wraps_a_plain_callable_as_custom() -> None:
    par = Parallel([_agent("a")], merge=lambda per_agent: "joined")
    assert isinstance(par.merge, Custom)
    assert par.merge.merge({}) == "joined"


# ── Team (coordinator) ────────────────────────────────────────────────────────


def test_team_constructs_with_router_agent_coordinator() -> None:
    router, x, y = _agent("router"), _agent("x"), _agent("y")
    team = Team([x, y], coordinator=router, name="desk")
    assert team.agents == [x, y]
    assert team.coordinator is router
    assert team.pattern == "coordinator"
    assert team.name == "desk"


def test_team_constructs_with_callable_coordinator() -> None:
    def route(input: str, agents: list[Agent]) -> Agent:
        return agents[0]

    team = Team([_agent("x")], coordinator=route)
    assert team.coordinator is route


def test_team_coordinator_optional() -> None:
    team = Team([_agent("only")])
    assert team.coordinator is None


# ── Loop ──────────────────────────────────────────────────────────────────────


def test_loop_constructs() -> None:
    a = _agent("refiner")
    loop = Loop(a, until=lambda r: True, max_iters=3)
    assert loop.agent is a
    assert loop.agents == [a]
    assert loop.pattern == "loop"
    assert loop.max_iters == 3


# ── TeamResult ────────────────────────────────────────────────────────────────


def test_team_result_str_is_output() -> None:
    tr = TeamResult(output="hi", per_agent={}, pattern="sequential", durable=False)
    assert str(tr) == "hi"
    assert tr.pattern == "sequential"
    assert tr.durable is False


# ── MergeStrategy semantics ───────────────────────────────────────────────────


class _Out:
    """Minimal AgentResult stand-in: only ``.output`` is read by merges."""

    def __init__(self, output: str) -> None:
        self.output = output


def test_collect_joins_labeled_successful_outputs_in_order() -> None:
    merged = Collect().merge({"a": _Out("one"), "b": _Out("two")})
    assert "[a] one" in merged
    assert "[b] two" in merged
    # order preserved
    assert merged.index("[a]") < merged.index("[b]")


def test_collect_skips_failures() -> None:
    merged = Collect().merge({"a": _Out("ok"), "b": RuntimeError("boom")})
    assert "[a] ok" in merged
    assert "boom" not in merged


def test_first_returns_first_successful_in_order() -> None:
    out = First().merge({"a": RuntimeError("x"), "b": _Out("second"), "c": _Out("third")})
    assert out == "second"


def test_first_empty_when_all_failed() -> None:
    assert First().merge({"a": RuntimeError("x")}) == ""


def test_merge_strategy_is_the_base() -> None:
    assert issubclass(Collect, MergeStrategy)
    assert issubclass(First, MergeStrategy)
    assert issubclass(Custom, MergeStrategy)


# ── Public exports ────────────────────────────────────────────────────────────


def test_public_exports() -> None:
    import jamjet

    assert jamjet.Team is Team
    assert jamjet.Sequential is Sequential
    assert jamjet.Parallel is Parallel
    assert jamjet.Loop is Loop
    assert jamjet.TeamResult is TeamResult
