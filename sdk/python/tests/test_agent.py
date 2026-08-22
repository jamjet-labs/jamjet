"""Tests for the Agent and @task syntactic sugar."""

import asyncio
import re
import warnings

import pytest

from jamjet import Agent, ApprovalNotEnforceableError, task, tool
from jamjet.agents.agent import AgentResult

# ── Fixtures ──────────────────────────────────────────────────────────────────


@tool
async def search(query: str) -> str:
    return f"Results for: {query}"


@tool
async def calculator(query: str) -> str:
    return "42"


# ── Agent tests ───────────────────────────────────────────────────────────────


class TestAgent:
    def test_create_minimal(self):
        agent = Agent("test", model="gpt-5.2", tools=[search])
        assert agent.name == "test"
        assert agent.model == "gpt-5.2"
        assert agent.tool_names == ["search"]

    def test_create_with_instructions(self):
        agent = Agent(
            "helper",
            model="gpt-5.2",
            tools=[search],
            instructions="Be helpful.",
        )
        assert agent.instructions == "Be helpful."

    def test_create_multiple_tools(self):
        agent = Agent("multi", model="gpt-5.2", tools=[search, calculator])
        assert agent.tool_names == ["search", "calculator"]

    def test_rejects_non_tool_functions(self):
        def plain_fn(x: str) -> str:
            return x

        with pytest.raises(TypeError, match="not a @tool-decorated function"):
            Agent("bad", model="gpt-5.2", tools=[plain_fn])

    def test_compile_produces_ir(self):
        from jamjet.spec import AgentSpec

        agent = Agent("compile_test", model="gpt-5.2", tools=[search])
        spec = agent.compile()
        assert isinstance(spec, AgentSpec)
        assert spec.name == "compile_test"
        assert spec.llm.model == "gpt-5.2"
        assert len(spec.tools) == 1

    def test_run_returns_result(self):
        agent = Agent(
            "runner",
            model="gpt-5.2",
            tools=[search],
            instructions="Search and summarize.",
        )
        result = asyncio.run(agent.run("test query"))
        assert isinstance(result, AgentResult)
        assert "Results for: test query" in result.output
        assert len(result.tool_calls) > 0

    def test_run_sync(self):
        agent = Agent("sync", model="gpt-5.2", tools=[search])
        result = agent.run_sync("test query")
        assert isinstance(result, AgentResult)
        assert "Results for: test query" in result.output

    def test_result_str(self):
        agent = Agent("str_test", model="gpt-5.2", tools=[search])
        result = agent.run_sync("hello")
        assert str(result) == result.output

    def test_repr(self):
        agent = Agent("repr_test", model="gpt-5.2", tools=[search])
        r = repr(agent)
        assert "repr_test" in r
        assert "gpt-5.2" in r

    def test_custom_limits(self):
        agent = Agent(
            "limits",
            model="gpt-5.2",
            tools=[search],
            max_iterations=5,
            max_cost_usd=0.5,
            timeout_seconds=60,
        )
        assert agent.limits.max_iterations == 5
        assert agent.limits.max_cost_usd == 0.5
        assert agent.limits.timeout_seconds == 60

    def test_explicit_one_dollar_ceiling_is_enforced(self):
        """The old default (1.0) doubled as the "not set" sentinel.

        The fold ran only when ``max_cost_usd != 1.0``, so a caller who
        explicitly asked for a $1.00 ceiling got NO ceiling — silently, and for
        the one figure a reader of the signature was most likely to copy. The
        sentinel is ``None`` now, so an explicit value is always honoured.
        """
        agent = Agent("dollar", model="gpt-5.2", tools=[search], max_cost_usd=1.0)
        assert agent.governance.budget is not None, (
            "an explicitly requested $1.00 ceiling must be enforced, not swallowed by a sentinel collision"
        )
        assert agent.governance.budget.cost_usd == 1.0

    def test_omitting_max_cost_usd_leaves_no_governance_ceiling(self):
        """Omitting it still means no enforced ceiling — unchanged behaviour.

        The strategy runners still get a concrete figure for their iteration
        budget; only the governance fold is gated on the argument being given.
        """
        agent = Agent("nocap", model="gpt-5.2", tools=[search])
        assert agent.governance.budget is None
        assert agent.limits.max_cost_usd == 1.0

    def test_budget_takes_precedence_over_max_cost_usd(self):
        agent = Agent("both", model="gpt-5.2", tools=[search], max_cost_usd=5.0, budget=2.0)
        assert agent.governance.budget.cost_usd == 2.0


# ── approval_required warning copy ────────────────────────────────────────────
#
# The warning on agent.run() is security guidance, so its wording is under test.
# It has to say two things and no more: (a) THIS call is not enforced, and (b)
# where enforcement actually lives. The durable path is genuinely enforced now,
# but by the engine server-side — the guard on the work-item claim route runs
# before the external tool worker ever receives the payload — not by the SDK.
# The old copy promised "the Rust engine enforces it fail-closed" while
# sitting on the unenforced in-process path, which reads as a guarantee about
# the call the developer just made.


def _agent_with_approval_required() -> Agent:
    return Agent(
        "approval_copy",
        model="gpt-5.2",
        tools=[search],
        approval_required=True,
    )


def _approval_refusal(agent: Agent) -> str:
    """The message run() refuses with when approval_required is set.

    It raises rather than warning: a UserWarning prints once per location and is
    dropped by `-W ignore`, PYTHONWARNINGS or a pytest config, so the old
    behaviour was to announce the gap to nobody and then run every tool ungated.
    """
    with pytest.raises(ApprovalNotEnforceableError) as excinfo:
        asyncio.run(agent.run("hi"))
    return str(excinfo.value)


# "worker" is allowed in the copy — "before any worker receives the payload" is
# the whole point. What is banned is the worker appearing as the SUBJECT of the
# enforcing, in either voice ("the worker enforces it" / "enforced by the tool
# worker"). Matched as a class rather than as one literal: the reword that
# actually happened on this branch said "Tells the Rust worker ... policy must be
# evaluated", which no single banned phrase would have caught. `[^.]` keeps each
# match inside one sentence so an innocent "worker" cannot pair with an enforcing
# verb from the next one.
_ENFORCE = r"(?:enforc|evaluat|check|block|gate|decid|den|held|hold)\w*"
_WORKER_CREDITED = re.compile(
    rf"\bworkers?\b[^.]{{0,40}}?{_ENFORCE}|{_ENFORCE}[^.]{{0,40}}?\bby\b[^.]{{0,20}}?\bworkers?\b",
    re.IGNORECASE,
)


class TestApprovalRefusalCopy:
    def test_in_process_run_does_not_promise_enforcement_here(self):
        """run() must refuse without implying the in-process path is enforced."""
        message = _approval_refusal(_agent_with_approval_required())
        assert "cannot hold a run at a gate" in message
        # The durable path is genuinely enforced (C1), but this message is about
        # the in-process path and must not read as a guarantee about this call.
        assert "fail-closed" not in message

    def test_in_process_run_points_at_the_real_enforcement_point(self):
        """The redirect must name where enforcement lives, accurately.

        Server-side, before the payload leaves the engine — that is what makes
        the durable claim true even for a stale or hostile external tool worker.
        """
        message = _approval_refusal(_agent_with_approval_required())
        assert "run_durable" in message
        assert "before any worker" in message

    def test_in_process_run_never_credits_the_worker(self):
        """The durable guarantee belongs to the engine, never to the worker.

        Crediting the worker is not just imprecise, it inverts the property:
        enforcement holds BECAUSE the external worker is untrusted and the
        decision is made before it sees the payload. Copy that reads "the worker
        enforces it" describes a system where a stale worker build could opt out.
        """
        message = _approval_refusal(_agent_with_approval_required())
        hit = _WORKER_CREDITED.search(message)
        assert hit is None, f"the refusal credits the worker with enforcing: {hit.group(0)!r}"

    def test_it_refuses_rather_than_running_ungated(self):
        """The property the message copy exists to serve.

        Before this, the same agent warned and then executed every tool with no
        gate — the knob read as configured and did nothing.
        """
        agent = _agent_with_approval_required()
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")  # the old signal died here
            with pytest.raises(ApprovalNotEnforceableError):
                asyncio.run(agent.run("hi"))


# ── @task tests ───────────────────────────────────────────────────────────────


class TestTask:
    def test_basic_task(self):
        @task(model="gpt-5.2", tools=[search])
        async def research(question: str) -> str:
            """You are a research assistant."""

        result = asyncio.run(research("test question"))
        assert "Results for: test question" in result

    def test_task_preserves_name(self):
        @task(model="gpt-5.2", tools=[search])
        async def named_task(q: str) -> str:
            """Do stuff."""

        assert named_task.__name__ == "named_task"

    def test_task_uses_docstring_as_instructions(self):
        @task(model="gpt-5.2", tools=[search])
        async def documented(q: str) -> str:
            """These are my instructions."""

        agent = documented._jamjet_agent
        assert agent.instructions == "These are my instructions."

    def test_task_with_kwargs(self):
        @task(model="gpt-5.2", tools=[search])
        async def kw_task(question: str) -> str:
            """Answer questions."""

        result = asyncio.run(kw_task(question="test"))
        assert "Results for: test" in result

    def test_task_requires_argument(self):
        @task(model="gpt-5.2", tools=[search])
        async def empty_task(q: str) -> str:
            """Do something."""

        with pytest.raises(TypeError, match="requires at least one argument"):
            asyncio.run(empty_task())

    def test_task_no_tools(self):
        @task(model="gpt-5.2")
        async def no_tools_task(q: str) -> str:
            """Think hard."""

        # Should not raise on creation — it's valid to have a model-only task
        agent = no_tools_task._jamjet_agent
        assert agent.tool_names == []
