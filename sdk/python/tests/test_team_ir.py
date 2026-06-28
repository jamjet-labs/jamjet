"""T6-2 — compile a Team to per-sub-agent IRs + a composition plan.

A Team is N agent IRs orchestrated in Python (Path A), NOT one giant multi-agent
IR. Each sub-agent compiles via the existing single-agent ``compile_agent_to_ir``
(so each carries its OWN governance). These tests assert the plan shape: order,
fan-out, merge, the router IR, and per-sub-agent governance isolation.
"""

from __future__ import annotations

from jamjet.agents.agent import Agent
from jamjet.compiler.team_ir import CompiledSubAgent, TeamPlan, compile_team_to_ir
from jamjet.team import Loop, Parallel, Sequential, Team


def _agent(name: str, **kw: object) -> Agent:
    return Agent(name, model="anthropic/claude-sonnet-4-6", tools=[], **kw)


def test_sequential_yields_per_agent_irs_in_order() -> None:
    a, b = _agent("alpha"), _agent("beta")
    plan = compile_team_to_ir(Sequential([a, b]))
    assert isinstance(plan, TeamPlan)
    assert plan.pattern == "sequential"
    assert [c.agent for c in plan.sub_agents] == [a, b]
    # each sub-agent compiled to its OWN agent-loop IR
    assert plan.sub_agents[0].ir["workflow_id"] == "alpha"
    assert plan.sub_agents[1].ir["workflow_id"] == "beta"
    assert plan.sub_agents[0].ir["labels"]["jamjet.agent.loop"] == "true"
    assert plan.coordinator is None
    assert plan.merge is None


def test_not_one_mega_ir() -> None:
    """N distinct agent IRs, never a single fused multi-agent IR (Path A)."""
    plan = compile_team_to_ir(Sequential([_agent("one"), _agent("two")]))
    irs = [c.ir for c in plan.sub_agents]
    assert len(irs) == 2
    assert irs[0]["workflow_id"] != irs[1]["workflow_id"]
    # there is no parent / fused IR object — only the per-sub-agent list
    assert all(ir["labels"]["jamjet.agent.loop"] == "true" for ir in irs)


def test_parallel_yields_all_irs_and_merge_name() -> None:
    plan = compile_team_to_ir(Parallel([_agent("a"), _agent("b")], merge="first"))
    assert plan.pattern == "parallel"
    assert len(plan.sub_agents) == 2
    assert plan.merge == "first"


def test_parallel_default_merge_is_collect() -> None:
    plan = compile_team_to_ir(Parallel([_agent("a")]))
    assert plan.merge == "collect"


def test_coordinator_compiles_the_router_agent_too() -> None:
    router, x, y = _agent("router"), _agent("x"), _agent("y")
    plan = compile_team_to_ir(Team([x, y], coordinator=router))
    assert plan.pattern == "coordinator"
    assert [c.agent for c in plan.sub_agents] == [x, y]
    assert isinstance(plan.coordinator, CompiledSubAgent)
    assert plan.coordinator.agent is router
    assert plan.coordinator.ir["workflow_id"] == "router"


def test_callable_coordinator_has_no_router_ir() -> None:
    plan = compile_team_to_ir(Team([_agent("x")], coordinator=lambda inp, agents: agents[0]))
    assert plan.coordinator is None


def test_loop_yields_single_sub_agent_ir() -> None:
    plan = compile_team_to_ir(Loop(_agent("refiner")))
    assert plan.pattern == "loop"
    assert len(plan.sub_agents) == 1
    assert plan.sub_agents[0].ir["workflow_id"] == "refiner"


def test_each_sub_agent_ir_carries_its_own_governance() -> None:
    """A budgeted sub-agent's IR carries cost_budget_usd; a default one does not.

    Proves the governance is compiled PER sub-agent (each its own IR), not merged
    or stripped — the no-bypass guarantee at the compile layer.
    """
    governed = _agent("governed", budget=0.5)
    plain = _agent("plain")
    plan = compile_team_to_ir(Sequential([governed, plain]))
    governed_ir = plan.sub_agents[0].ir
    plain_ir = plan.sub_agents[1].ir
    assert governed_ir["cost_budget_usd"] == 0.5
    assert "cost_budget_usd" not in plain_ir


def test_irs_convenience_lists_every_sub_agent_ir() -> None:
    plan = compile_team_to_ir(Parallel([_agent("a"), _agent("b")]))
    assert [ir["workflow_id"] for ir in plan.irs] == ["a", "b"]
