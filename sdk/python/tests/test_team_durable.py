"""T6-4 — ``Team.run_durable`` over N per-sub-agent durable executions.

Each sub-agent runs as its OWN durable execution (the shipped 2j path:
create_workflow -> start_execution -> poll -> extract). The team composes them per
the pattern. Two levels of test: (1) orchestration over a scripted ``run_durable``
(confirms the durable method is used + ``TeamResult.durable``), and (2) the REAL
``Agent.run_durable`` driven by a fake ``JamjetClient`` (confirms each sub-agent's
execution actually starts and that a failed/limit_exceeded terminal is isolated —
never a team crash or hang).
"""

from __future__ import annotations

import pytest

from jamjet.team import Collect, Parallel, Sequential, Team
from tests.team_fakes import (
    MultiAgentFakeClient,
    completed_terminal,
    failed_terminal,
    scripted_agent,
)


def _patch_client(monkeypatch: pytest.MonkeyPatch, fake: MultiAgentFakeClient, urls: list | None = None) -> None:
    """Patch JamjetClient (constructed inside Agent.run_durable) to the *fake*.

    Optionally capture the runtime_url each construction received into *urls*.
    """

    def factory(*args: object, **kwargs: object) -> MultiAgentFakeClient:
        # Record the runtime_url from a positional OR keyword construction so the
        # capture survives whether JamjetClient(url) or JamjetClient(base_url=url)
        # is used. One append per construction == one per sub-agent (the count).
        if urls is not None:
            if args:
                urls.append(args[0])
            elif "base_url" in kwargs:
                urls.append(kwargs["base_url"])
            elif "runtime_url" in kwargs:
                urls.append(kwargs["runtime_url"])
        return fake

    monkeypatch.setattr("jamjet.client.JamjetClient", factory)


# ── Orchestration level: scripted run_durable ─────────────────────────────────


async def test_run_durable_uses_the_durable_path_and_marks_result() -> None:
    a = scripted_agent("a", transform=lambda p: f"A({p})")
    b = scripted_agent("b", transform=lambda p: f"B({p})")
    result = await Sequential([a, b]).run_durable("x", runtime_url="http://engine:9000")

    assert result.durable is True
    # the DURABLE method was used (not the in-process run), with the threaded url.
    assert a.calls[0][0] == "run_durable"
    assert b.calls[0][0] == "run_durable"
    assert a.durable_runtime_urls == ["http://engine:9000"]
    # sequential threading still holds on the durable path.
    assert b.calls[0][1] == "A(x)"
    assert result.output == "B(A(x))"


async def test_run_marks_result_not_durable() -> None:
    a = scripted_agent("a", output="o")
    result = await Sequential([a]).run("x")
    assert result.durable is False
    assert a.calls[0][0] == "run"


# ── Client level: REAL Agent.run_durable through a fake JamjetClient ───────────


async def test_run_durable_sequential_starts_each_execution(monkeypatch: pytest.MonkeyPatch) -> None:
    fake = MultiAgentFakeClient(
        {"alpha": completed_terminal("alpha-answer"), "beta": completed_terminal("beta-answer")}
    )
    _patch_client(monkeypatch, fake)
    from jamjet.agents.agent import Agent

    # REAL agents -> the real Agent.run_durable runs through the fake client.
    a = Agent("alpha", model="anthropic/claude-sonnet-4-6", tools=[])
    b = Agent("beta", model="anthropic/claude-sonnet-4-6", tools=[])

    result = await Sequential([a, b]).run_durable("start")

    # both sub-agents started their OWN durable execution.
    assert set(fake.created) == {"alpha", "beta"}
    assert [w for w, _ in fake.started] == ["alpha", "beta"]
    # sequential threaded alpha's answer into beta's prompt (durable seed messages).
    _, beta_input = next((w, i) for w, i in fake.started if w == "beta")
    assert beta_input["messages"][-1]["content"] == "alpha-answer"
    assert result.output == "beta-answer"
    assert result.durable is True


async def test_run_durable_parallel_composes_all(monkeypatch: pytest.MonkeyPatch) -> None:
    fake = MultiAgentFakeClient({"x": completed_terminal("x-ans"), "y": completed_terminal("y-ans")})
    _patch_client(monkeypatch, fake)
    from jamjet.agents.agent import Agent

    x = Agent("x", model="anthropic/claude-sonnet-4-6", tools=[])
    y = Agent("y", model="anthropic/claude-sonnet-4-6", tools=[])

    result = await Parallel([x, y], merge=Collect()).run_durable("go")

    assert set(fake.created) == {"x", "y"}
    assert result.output == "[x] x-ans\n[y] y-ans"


async def test_run_durable_isolates_a_failed_terminal(monkeypatch: pytest.MonkeyPatch) -> None:
    fake = MultiAgentFakeClient({"good": completed_terminal("good-ans"), "bad": failed_terminal("failed")})
    _patch_client(monkeypatch, fake)
    from jamjet.agents.agent import Agent

    good = Agent("good", model="anthropic/claude-sonnet-4-6", tools=[])
    bad = Agent("bad", model="anthropic/claude-sonnet-4-6", tools=[])

    result = await Parallel([good, bad], merge=Collect()).run_durable("go")

    # bad's failed terminal -> RuntimeError isolated into per_agent (not a team crash).
    assert isinstance(result.per_agent["bad"], RuntimeError)
    assert result.per_agent["good"].output == "good-ans"
    assert result.output == "[good] good-ans"
    assert list(result.errors) == ["bad"]


async def test_run_durable_isolates_a_limit_exceeded_terminal(monkeypatch: pytest.MonkeyPatch) -> None:
    fake = MultiAgentFakeClient({"capped": failed_terminal("limit_exceeded")})
    _patch_client(monkeypatch, fake)
    from jamjet.agents.agent import Agent

    capped = Agent("capped", model="anthropic/claude-sonnet-4-6", tools=[])

    result = await Team([capped], coordinator=lambda inp, agents: agents[0]).run_durable("go")

    assert isinstance(result.per_agent["capped"], RuntimeError)
    assert result.output == ""


async def test_run_durable_threads_runtime_url_to_each_subagent(monkeypatch: pytest.MonkeyPatch) -> None:
    fake = MultiAgentFakeClient({"a": completed_terminal("a"), "b": completed_terminal("b")})
    urls: list = []
    _patch_client(monkeypatch, fake, urls=urls)
    from jamjet.agents.agent import Agent

    a = Agent("a", model="anthropic/claude-sonnet-4-6", tools=[])
    b = Agent("b", model="anthropic/claude-sonnet-4-6", tools=[])

    await Parallel([a, b]).run_durable("go", runtime_url="http://engine:7777")

    # one durable client constructed per sub-agent (count), each against the team's
    # runtime_url — robust to positional OR keyword JamjetClient construction.
    assert len(urls) == 2
    assert all(u == "http://engine:7777" for u in urls)
