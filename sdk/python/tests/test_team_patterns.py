"""T6-3 — the in-process coordination patterns (over ``Agent.run``).

Sequential threads output a->b; Parallel fans out + merges (Collect / First);
the coordinator routes to the right specialist; a failing sub-agent is isolated
(the team never crashes — the error lands in ``TeamResult``). No engine: the
sub-agents are scripted real Agents (see ``tests/team_fakes.py``).
"""

from __future__ import annotations

import pytest

from jamjet.team import Collect, First, Parallel, Sequential, Team
from tests.team_fakes import scripted_agent

# ── Sequential: threads output forward ────────────────────────────────────────


async def test_sequential_threads_output_a_to_b() -> None:
    a = scripted_agent("a", transform=lambda p: f"A({p})")
    b = scripted_agent("b", transform=lambda p: f"B({p})")
    result = await Sequential([a, b]).run("x")

    # b's input was a's output, and the final output is b(a(x)).
    assert b.calls[0][1] == "A(x)"
    assert result.output == "B(A(x))"
    assert result.pattern == "sequential"
    assert set(result.per_agent) == {"a", "b"}


async def test_sequential_halts_on_error_and_isolates_it() -> None:
    a = scripted_agent("a", output="a-out")
    boom = scripted_agent("boom", fail=RuntimeError("kaboom"))
    c = scripted_agent("c", output="c-out")
    result = await Sequential([a, boom, c]).run("go")

    # a ran, boom failed (isolated), c never ran (halt-on-error).
    assert result.per_agent["a"].output == "a-out"
    assert isinstance(result.per_agent["boom"], RuntimeError)
    assert "c" not in result.per_agent
    assert c.calls == []
    # the team did not crash; output is the last successful output.
    assert result.output == "a-out"
    assert result.ok is False
    assert "boom" in result.errors


# ── Parallel: fan out + merge ─────────────────────────────────────────────────


async def test_parallel_runs_all_and_collects() -> None:
    a = scripted_agent("a", output="ra")
    b = scripted_agent("b", output="rb")
    c = scripted_agent("c", output="rc")
    result = await Parallel([a, b, c], merge=Collect()).run("same-input")

    # every sub-agent saw the SAME input (fan-out, not a pipeline).
    assert a.calls[0][1] == "same-input"
    assert b.calls[0][1] == "same-input"
    assert c.calls[0][1] == "same-input"
    assert result.output == "[a] ra\n[b] rb\n[c] rc"
    assert set(result.per_agent) == {"a", "b", "c"}


async def test_parallel_first_merge_returns_first_successful() -> None:
    a = scripted_agent("a", output="first-out")
    b = scripted_agent("b", output="second-out")
    result = await Parallel([a, b], merge=First()).run("in")
    assert result.output == "first-out"


async def test_parallel_isolates_a_failing_subagent() -> None:
    good = scripted_agent("good", output="ok")
    bad = scripted_agent("bad", fail=ValueError("nope"))
    result = await Parallel([good, bad], merge=Collect()).run("in")

    # the team did not crash; the failure is captured, the sibling succeeded.
    assert result.per_agent["good"].output == "ok"
    assert isinstance(result.per_agent["bad"], ValueError)
    assert result.output == "[good] ok"  # Collect skips the failure
    assert list(result.errors) == ["bad"]


# ── Coordinator: route to a specialist ────────────────────────────────────────


async def test_coordinator_router_agent_routes_to_named_specialist() -> None:
    researcher = scripted_agent("researcher", output="researched")
    writer = scripted_agent("writer", output="written")
    # the router's output NAMES the specialist to run.
    router = scripted_agent("router", output="writer")
    result = await Team([researcher, writer], coordinator=router).run("task")

    assert result.output == "written"
    assert "writer" in result.per_agent
    assert "router" in result.per_agent
    # the unchosen specialist never ran.
    assert "researcher" not in result.per_agent
    assert researcher.calls == []
    assert result.pattern == "coordinator"


async def test_coordinator_router_matches_name_in_freetext() -> None:
    researcher = scripted_agent("researcher", output="researched")
    writer = scripted_agent("writer", output="written")
    router = scripted_agent("router", output="I think the writer should handle this.")
    result = await Team([researcher, writer], coordinator=router).run("task")
    assert result.output == "written"


async def test_coordinator_callable_picks_by_return() -> None:
    x = scripted_agent("x", output="x-out")
    y = scripted_agent("y", output="y-out")

    def route(inp: str, agents: list) -> object:
        return agents[1]  # always pick y

    result = await Team([x, y], coordinator=route).run("task")
    assert result.output == "y-out"
    assert "y" in result.per_agent
    assert "x" not in result.per_agent


async def test_coordinator_callable_can_return_a_name() -> None:
    x = scripted_agent("x", output="x-out")
    y = scripted_agent("y", output="y-out")
    result = await Team([x, y], coordinator=lambda inp, agents: "x").run("task")
    assert result.output == "x-out"


async def test_coordinator_none_runs_first_agent() -> None:
    only = scripted_agent("only", output="only-out")
    result = await Team([only]).run("task")
    assert result.output == "only-out"


async def test_coordinator_isolates_a_failing_specialist() -> None:
    bad = scripted_agent("bad", fail=RuntimeError("specialist down"))
    result = await Team([bad], coordinator=lambda inp, agents: agents[0]).run("task")
    # routing succeeded but the specialist crashed: isolated, not a team crash.
    assert isinstance(result.per_agent["bad"], RuntimeError)
    assert result.output == ""


async def test_coordinator_isolates_a_failing_router() -> None:
    specialist = scripted_agent("s", output="s-out")
    router = scripted_agent("router", fail=RuntimeError("router down"))
    result = await Team([specialist], coordinator=router).run("task")
    # the router crashed: recorded, no specialist ran, no team crash.
    assert isinstance(result.per_agent["router"], RuntimeError)
    assert specialist.calls == []
    assert result.output == ""


async def test_callable_coordinator_unknown_name_raises_clear_error() -> None:
    """A routing CALLABLE that returns an invalid name is a code bug -> fail loud
    (unlike a router AGENT's fuzzy free-text, which falls back to the first agent)."""
    x = scripted_agent("x", output="x")
    with pytest.raises(ValueError, match="unknown agent name"):
        await Team([x], coordinator=lambda inp, agents: "does-not-exist").run("t")
