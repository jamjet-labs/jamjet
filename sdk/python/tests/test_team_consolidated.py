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
