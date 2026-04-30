"""
Framework parity matrix: prove that @durable + durable_run() produce
exactly-once tool execution semantics regardless of which framework
shim sets the execution context.

We don't actually run each framework — that requires real LLMs and is
brittle. Instead we use each shim's durable_run() to set the context,
then call a @durable-wrapped tool directly, simulate a process restart
by clearing the in-memory function-call counter while reusing the same
SQLite cache file, and assert the side effect (counter increment) only
happens once across the simulated crash boundary.
"""

import pytest

from jamjet.durable import durable
from jamjet.durable import durable_run as core_durable_run
from jamjet.durable.cache import SqliteCache


class _FakeRunHandle:
    """Stand-in for any framework's run handle — exposes id and run_id."""

    def __init__(self, run_id: str):
        self.run_id = run_id
        self.id = run_id  # CrewAI uses .id, ADK uses .session_id, etc.
        self.session_id = run_id

    def __getattr__(self, _name):
        return self.run_id


@pytest.fixture(
    params=[
        "core",
        "langchain",
        "crewai",
        "adk",
        "anthropic_agent",
        "openai_agents",
    ]
)
def shim_durable_run(request):
    """Yield (durable_run_callable, shim_name) for each framework shim."""
    name = request.param
    if name == "core":
        return lambda h: core_durable_run(h.run_id), name
    pytest.importorskip(
        {
            "langchain": "langchain",
            "crewai": "crewai",
            "adk": "google.adk",
            "anthropic_agent": "anthropic",
            "openai_agents": "agents",
        }[name]
    )
    if name == "langchain":
        from jamjet.langchain import durable_run as dr
    elif name == "crewai":
        from jamjet.crewai import durable_run as dr
    elif name == "adk":
        from jamjet.adk import durable_run as dr
    elif name == "anthropic_agent":
        from jamjet.anthropic_agent import durable_run as dr
    elif name == "openai_agents":
        from jamjet.openai_agents import durable_run as dr
    return dr, name


def test_exactly_once_under_simulated_crash(tmp_path, shim_durable_run):
    """
    Across a simulated process restart, the side-effect-tracked function
    must execute exactly once for the same args within the same run.
    """
    dr, name = shim_durable_run
    cache = SqliteCache(tmp_path / "cache.db")
    side_effect_count = {"calls": 0}

    @durable(cache=cache)
    def charge_card(amount: float) -> dict:
        side_effect_count["calls"] += 1
        return {"id": f"ch_{side_effect_count['calls']}", "amount": amount}

    handle = _FakeRunHandle(run_id=f"parity-{name}")

    # First execution — completes normally.
    with dr(handle):
        result1 = charge_card(847.0)

    # Simulate a process restart: brand-new in-memory state, but same cache file.
    side_effect_count["calls"] = 0
    cache2 = SqliteCache(tmp_path / "cache.db")

    # Re-bind the same name to test cache hit behavior:
    @durable(cache=cache2)
    def charge_card(amount: float) -> dict:  # noqa: F811
        side_effect_count["calls"] += 1
        return {"id": f"ch_after_{side_effect_count['calls']}", "amount": amount}

    with dr(handle):
        result2 = charge_card(847.0)

    # The cache hit means the second call returned the result from before
    # the simulated crash, and the new function body did NOT execute.
    assert result1 == result2, f"[{name}] cache miss after simulated restart"
    assert side_effect_count["calls"] == 0, (
        f"[{name}] side effect ran again after restart — got {side_effect_count['calls']} calls"
    )
