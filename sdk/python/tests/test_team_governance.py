"""T6-5 — governance + session inheritance for team sub-agents.

Two guarantees:

1. **No governance bypass.** A sub-agent compiles and enforces its OWN governance
   (budget / PII / allowlist). A team adds no enforcement layer and removes none —
   a budgeted sub-agent in a team STILL denies an over-budget call (the denial is
   isolated into ``TeamResult``, proving it fired). A team ``governance=`` default
   is inherited only by un-governed sub-agents; an explicitly-governed sub-agent is
   never overridden (and so never weakened).

2. **Session model.** A team ``session=`` namespaces a per-sub-agent CHILD session
   (distinct id per sub-agent) so concurrent siblings never race on one SQLite row.
   No team session -> sub-agents run stateless.
"""

from __future__ import annotations

import sys

import pytest

from jamjet import tool
from jamjet.agents.agent import Agent
from jamjet.agents.governance import Budget, GovernanceConfig
from jamjet.agents.session import Session, SessionStore
from jamjet.compiler.team_ir import compile_team_to_ir
from jamjet.model.middleware import BudgetExceededError
from jamjet.team import Parallel, Sequential
from jamjet.team.team import derive_child_session
from tests.team_fakes import scripted_agent


def _agent(name: str, **kw: object) -> Agent:
    return Agent(name, model="anthropic/claude-sonnet-4-6", tools=[], **kw)


# ── Governance inheritance (config + compiled IR) ─────────────────────────────


def test_explicit_budget_subagent_keeps_its_budget_in_a_team() -> None:
    governed = _agent("g", budget=0.5)
    Sequential([governed])  # team construction must NOT strip the sub-agent's budget
    assert governed.governance.budget == Budget(cost_usd=0.5)


def test_team_default_inherited_by_an_ungoverned_subagent() -> None:
    plain = _agent("p")  # all-default governance
    assert plain.governance == GovernanceConfig()  # precondition
    Sequential([plain], governance={"budget": 0.5})
    assert plain.governance.budget == Budget(cost_usd=0.5)
    # and the inheritance reaches the compiled IR (enforcement-ready).
    plan = compile_team_to_ir(Sequential([plain]))  # plain now carries the inherited budget
    assert plan.sub_agents[0].ir["cost_budget_usd"] == 0.5


def test_team_default_does_not_override_an_explicit_subagent() -> None:
    explicit = _agent("e", budget=2.0)
    Sequential([explicit], governance={"budget": 0.5})
    # explicit beats inherited, wholesale — the team never weakens it.
    assert explicit.governance.budget == Budget(cost_usd=2.0)


def test_no_team_governance_means_no_mutation() -> None:
    plain = _agent("p")
    Sequential([plain])  # no governance= default
    assert plain.governance == GovernanceConfig()


def test_team_governance_accepts_a_governanceconfig() -> None:
    plain = _agent("p")
    Sequential([plain], governance=GovernanceConfig(pii=False))
    assert plain.governance.pii is False


def test_team_governance_bad_type_raises() -> None:
    with pytest.raises(TypeError, match="governance must be"):
        Sequential([_agent("p")], governance="not-a-config")  # type: ignore[arg-type]


def test_compiled_subagent_ir_carries_budget_pii_and_allowlist() -> None:
    """The plan's 'budget/PII/allowlist' all survive into the per-sub-agent IR."""
    governed = _agent("g", budget=0.5, policy="strict")  # strict => anthropic allowlist + pii on
    plan = compile_team_to_ir(Sequential([governed]))
    ir = plan.sub_agents[0].ir
    assert ir["cost_budget_usd"] == 0.5
    assert ir["policy"]["model_allowlist"] == ["anthropic"]
    assert "data_policy" in ir  # PII metadata present


# ── No governance bypass — a real over-budget denial through the team ──────────


@tool
async def _search(query: str) -> str:
    """A trivial tool that forces the in-process react loop to make 2 model calls."""
    return f"Results for: {query}"


def _install_backend(monkeypatch: pytest.MonkeyPatch, *, cost: float) -> dict:
    """Wrap the conftest litellm mock with a fixed per-call cost + a call counter,
    so the run-scoped budget accumulator trips deterministically."""
    lm = sys.modules["litellm"]
    rec = {"calls": 0}
    original = lm.acompletion

    async def _acompletion(model: str, messages: list, tools: list | None = None, **kw: object) -> object:
        rec["calls"] += 1
        return await original(model=model, messages=messages, tools=tools, **kw)

    monkeypatch.setattr(lm, "acompletion", _acompletion)
    monkeypatch.setattr(lm, "completion_cost", lambda completion_response=None, **kw: cost)
    return rec


async def test_team_does_not_bypass_budget_enforcement(monkeypatch: pytest.MonkeyPatch) -> None:
    """A budgeted sub-agent in a team STILL denies the over-budget call.

    cost=0.5/call, budget=0.4: call 1 proceeds (spends $0.50), call 2's pre-provider
    check sees $0.50 >= $0.40 and denies. The team ISOLATES that denial into
    per_agent (proving the budget fired, and the team did not swallow or bypass it).
    """
    rec = _install_backend(monkeypatch, cost=0.5)
    budgeted = Agent("b", model="gpt-5.2", tools=[_search], strategy="react", budget=0.4)
    result = await Sequential([budgeted]).run("q")

    assert isinstance(result.per_agent["b"], BudgetExceededError)
    assert rec["calls"] == 1  # the over-budget 2nd call never reached the provider
    assert "b" in result.errors


# ── Session inheritance — distinct race-safe child sessions ───────────────────


def test_derive_child_session_gives_distinct_stable_ids(tmp_path) -> None:
    store = SessionStore(str(tmp_path / "s.db"))
    base = store.create("team1")
    a0 = derive_child_session(base, 0, "a")
    b1 = derive_child_session(base, 1, "b")
    a0_again = derive_child_session(base, 0, "a")
    assert a0 is not None and b1 is not None
    assert a0.id == "team1::0-a"
    assert b1.id == "team1::1-b"
    assert a0.id != b1.id  # distinct per sub-agent (race-safe under parallel)
    assert a0_again.id == a0.id  # stable across runs (thread persists)


def test_no_team_session_is_stateless() -> None:
    assert derive_child_session(None, 0, "a") is None


async def test_team_session_threads_distinct_child_sessions_to_subagents(tmp_path) -> None:
    store = SessionStore(str(tmp_path / "sessions.db"))
    team_session = store.create("desk")
    a = scripted_agent("a", output="ra")
    b = scripted_agent("b", output="rb")

    await Parallel([a, b], session=team_session).run("go")

    # each sub-agent received its OWN derived child Session (distinct ids, namespaced
    # by the team session id + agent) — never the same row, so parallel can't race.
    a_session = a.calls[0][2]
    b_session = b.calls[0][2]
    assert isinstance(a_session, Session)
    assert isinstance(b_session, Session)
    assert a_session.id == "desk::0-a"
    assert b_session.id == "desk::1-b"
    assert a_session.id != b_session.id


async def test_no_team_session_runs_subagents_stateless(tmp_path) -> None:
    a = scripted_agent("a", output="ra")
    await Sequential([a]).run("go")
    assert a.calls[0][2] is None
