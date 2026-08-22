"""T6-6 — consolidated patterns coverage + the multi-agent example smoke.

Exercises all four patterns together (a regression guard), the Loop pattern's
behaviour (until-predicate, max-iters, output threading, isolation), and the
``examples/team-multi-agent`` example: it imports + constructs cleanly (engine-free
smoke), governance inheritance reaches the compiled IR, and ``main.py`` compiles.
"""

from __future__ import annotations

import importlib.util
import py_compile
import sys
from pathlib import Path
from types import ModuleType

from jamjet.compiler.team_ir import compile_team_to_ir
from jamjet.team import Collect, First, Loop, Parallel, Sequential, Team
from tests.team_fakes import scripted_agent

REPO_ROOT = Path(__file__).resolve().parents[3]
EXAMPLE_DIR = REPO_ROOT / "examples" / "team-multi-agent"


def _load(name: str, path: Path) -> ModuleType:
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    # Register under its name BEFORE exec so the in-process strategy can resolve a
    # tool's handler_ref ("<name>:<fn>") via importlib.import_module(name) — exactly
    # how the example resolves "specialists:web_search" when run for real.
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


# ── Consolidated: all four patterns compose ───────────────────────────────────


async def test_all_four_patterns_compose() -> None:
    # sequential threads a -> b
    seq = await Sequential(
        [scripted_agent("a", transform=lambda p: f"A({p})"), scripted_agent("b", transform=lambda p: f"B({p})")]
    ).run("x")
    assert seq.output == "B(A(x))"

    # parallel fans out + collects
    par = await Parallel([scripted_agent("a", output="ra"), scripted_agent("b", output="rb")], merge=Collect()).run(
        "in"
    )
    assert par.output == "[a] ra\n[b] rb"

    # coordinator routes to the named specialist
    coord = await Team(
        [scripted_agent("researcher", output="R"), scripted_agent("writer", output="W")],
        coordinator=scripted_agent("router", output="writer"),
    ).run("task")
    assert coord.output == "W"

    # loop refines until the predicate holds
    loop = await Loop(
        scripted_agent("refiner", transform=lambda p: p + "!"),
        until=lambda r: r.output.endswith("!!!"),
        max_iters=10,
    ).run("x")
    assert loop.output == "x!!!"


# ── Loop behaviour ─────────────────────────────────────────────────────────────


async def test_loop_threads_output_and_keys_each_iteration() -> None:
    result = await Loop(scripted_agent("r", transform=lambda p: p + "!"), max_iters=3).run("x")
    assert result.output == "x!!!"
    assert list(result.per_agent) == ["r#0", "r#1", "r#2"]
    assert result.pattern == "loop"


async def test_loop_stops_early_on_predicate() -> None:
    result = await Loop(
        scripted_agent("r", transform=lambda p: p + "!"),
        until=lambda res: res.output == "x!!",
        max_iters=10,
    ).run("x")
    assert result.output == "x!!"
    assert list(result.per_agent) == ["r#0", "r#1"]  # stopped at the 2nd iteration


async def test_loop_respects_max_iters_when_predicate_never_holds() -> None:
    result = await Loop(
        scripted_agent("r", transform=lambda p: p + "!"),
        until=lambda res: False,
        max_iters=2,
    ).run("x")
    assert list(result.per_agent) == ["r#0", "r#1"]
    assert result.output == "x!!"


async def test_loop_isolates_a_failing_iteration() -> None:
    result = await Loop(scripted_agent("r", fail=RuntimeError("boom")), max_iters=3).run("x")
    assert isinstance(result.per_agent["r#0"], RuntimeError)
    assert list(result.per_agent) == ["r#0"]  # the loop stopped on the failure
    assert result.output == ""


# ── Example smoke: import + construct + compile (engine-free) ─────────────────


def test_example_specialists_construct_the_teams() -> None:
    specialists = _load("team_example_specialists", EXAMPLE_DIR / "specialists.py")

    desk = specialists.build_desk()
    assert isinstance(desk, Team)
    assert desk.pattern == "coordinator"
    assert [a.name for a in desk.agents] == ["researcher", "writer"]
    assert desk.coordinator.name == "router"
    assert desk.name == "content-desk"

    pipeline = specialists.build_pipeline()
    assert isinstance(pipeline, Sequential)
    assert [a.name for a in pipeline.agents] == ["researcher", "writer"]


def test_example_governance_default_is_inherited_into_compiled_ir() -> None:
    specialists = _load("team_example_specialists_gov", EXAMPLE_DIR / "specialists.py")
    desk = specialists.build_desk()
    # the un-governed specialists inherited the team's budget cap...
    assert desk.agents[0].governance.budget.cost_usd == 0.50
    # ...and it reaches each sub-agent's compiled IR (enforcement-ready).
    plan = compile_team_to_ir(desk)
    assert plan.coordinator is not None  # the router compiled too
    assert all(c.ir["cost_budget_usd"] == 0.50 for c in plan.sub_agents)


async def test_example_pipeline_runs_end_to_end_under_the_mock_model() -> None:
    """The example's specialists actually EXECUTE through the team (researcher ->
    writer) under the conftest mock model — proves the example orchestrates, not
    just constructs. No engine, no network."""
    specialists = _load("team_example_run", EXAMPLE_DIR / "specialists.py")
    result = await specialists.build_pipeline().run("agent runtimes")
    assert result.pattern == "sequential"
    assert set(result.per_agent) == {"researcher", "writer"}
    assert result.ok
    assert result.output  # the writer produced a non-empty final answer


def test_example_main_compiles() -> None:
    main_py = EXAMPLE_DIR / "main.py"
    assert main_py.exists()
    py_compile.compile(str(main_py), doraise=True)


def test_example_readme_exists() -> None:
    assert (EXAMPLE_DIR / "README.md").exists()


async def test_parallel_first_merge_consolidated() -> None:
    result = await Parallel(
        [scripted_agent("a", output="winner"), scripted_agent("b", output="loser")], merge=First()
    ).run("in")
    assert result.output == "winner"


# ── Team governance inheritance is provenance-based, not value-based ──────────


def _gov_agent(name: str, **governance: object):
    """A minimal agent carrying whatever governance knobs the test passes."""
    from jamjet import Agent, tool

    @tool
    def echo(x: str) -> str:
        return x

    return Agent(name, model="anthropic/claude-sonnet-4-6", tools=[echo], **governance)


def test_an_explicit_knob_equal_to_the_default_is_not_overridden() -> None:
    """`Team` must not replace governance a sub-agent deliberately chose.

    Inheritance used to be decided by comparing the sub-agent's config against an
    all-default `GovernanceConfig`. An agent that explicitly asked for `pii=True`
    — the default value — was indistinguishable from one that asked for nothing,
    so its governance was replaced wholesale. With a team default of `pii=False`
    that turned an explicitly requested protection OFF, which the function's own
    docstring promises never happens.
    """
    silent = _gov_agent("silent")
    explicit = _gov_agent("explicit", pii=True)
    coordinator = _gov_agent("coordinator")

    Team(
        agents=[silent, explicit],
        coordinator=coordinator,
        governance={"pii": False},
    )

    assert silent.governance.pii is False, "a sub-agent that set nothing must inherit the team default"
    assert explicit.governance.pii is True, (
        "an EXPLICIT pii=True must survive a team default of pii=False — "
        "explicit beats inherited, whatever the value happens to equal"
    )


def test_provenance_is_recorded_without_changing_equality() -> None:
    """`explicit` is metadata: it must not make two equal configs unequal.

    Equality is used elsewhere to mean "same governance", and provenance is a
    different question from value.
    """
    silent = _gov_agent("silent")
    explicit = _gov_agent("explicit", pii=True)

    assert silent.governance.explicit == frozenset()
    assert explicit.governance.explicit == frozenset({"pii"})
    assert silent.governance == explicit.governance, "same values must stay equal regardless of how they were reached"


def test_every_governance_knob_is_tracked() -> None:
    """Each knob must record itself, or inheritance silently reverts for it."""
    for knob, value in (
        ("policy", "strict"),
        ("approval_required", True),
        ("budget", 2.0),
        ("pii", True),
        ("audit", True),
        ("receipts", True),
    ):
        agent = _gov_agent(f"a_{knob}", **{knob: value})
        assert knob in agent.governance.explicit, f"{knob} was not recorded as explicit"
