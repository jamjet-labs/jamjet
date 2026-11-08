"""Every directory in examples/ is exercised here or says why it cannot be.

Three levels, cheapest that still catches the breakage that kept examples
broken for months:

- OFFLINE_RUN: run with ``python <file>`` and no API keys; must exit 0.
- VALIDATE: YAML definitions compiled the way ``jamjet validate`` compiles
  them; at least one node, no unknown keys, no schema warnings.
- IMPORT_ONLY: examples that call a model or a runtime are imported (their
  ``__main__`` block does not run) and every module-level ``Agent`` and
  ``Workflow`` is compiled, so a changed constructor or step signature fails.

``test_every_example_is_accounted_for`` makes a new example pick one.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import textwrap
import warnings
from pathlib import Path

import pytest
import yaml

from jamjet.workflow.bundle import compile_bundle, is_bundle
from jamjet.workflow.ir_compiler import WorkflowSchemaWarning, compile_yaml

EXAMPLES_DIR = Path(__file__).resolve().parents[3] / "examples"

OFFLINE_RUN = {
    "05-mcp-tool-call": "main.py",
    "agent-as-tool": "main.py",
    "basic-tool-flow": "workflow.py",
    "code-review-routing": "main.py",
    "coordinator-routing": "main.py",
    "custom-coordinator-strategy": "main.py",
    "database-safety": "main.py",
    "ldp-identity-routing": "main.py",
    "session-memory": "main.py",
    "team-multi-agent": "main.py",
}

VALIDATE = {
    "a2a-delegation": "workflow.yaml",
    "eval-harness": "workflow.yaml",
    "fleet": "fleet.yaml",
    "hitl-approval": "workflow.yaml",
    "mcp-tool-consumer": "workflow.yaml",
    "multi-agent-review": "workflow.yaml",
    "plan-and-execute-agent": "agent.yaml",
    "rag-assistant": "workflow.yaml",
    "react-agent": "agent.yaml",
}

IMPORT_ONLY = {
    "claims-processing": "main.py",
    "deploy-an-agent": "status_agent.py",
    "mcp-tool-provider": "server.py",
    "react-agent-durable": "weather_agent.py",
    "research-deliberation": "deliberation.py",
    "vertex-ai-agents": "python/workflow.py",
    "wealth-management-agents": "jamjet_impl.py",
}

ELSEWHERE = {
    "01-block-unsafe-tool": "run against expected output in test_examples_run.py",
    "02-human-approval": "run against expected output in test_examples_run.py",
    "03-budget-cap": "run against expected output in test_examples_run.py",
    "04-mcp-tool-policy": "run against expected output in test_examples_run.py",
    "trajectory-eval": "an evalset with no code; see test_trajectory_eval_fixture.py",
}

# What makes a directory a Python-suite example: Python code anywhere, or a
# JamJet definition or dataset at its top. A directory with example files but
# neither (a Java or TypeScript app, even one with an application.yml) belongs
# to another suite.
_PYTHON_SUITE_SUFFIXES = {".py"}
_PYTHON_SUITE_TOP_LEVEL_SUFFIXES = {".yaml", ".yml", ".jsonl"}
_EXAMPLE_FILE_SUFFIXES = {
    ".py",
    ".yaml",
    ".yml",
    ".jsonl",
    ".java",
    ".kt",
    ".ts",
    ".tsx",
    ".js",
    ".json",
    ".xml",
    ".sh",
}
_SKIP_PARTS = {"node_modules", "__pycache__", "target", "build", "dist"}


def _offline_env() -> dict[str, str]:
    """The current environment without model or cloud credentials."""
    return {k: v for k, v in os.environ.items() if not k.endswith("_API_KEY") and k != "JAMJET_TOKEN"}


def _example_files(directory: Path) -> list[Path]:
    return [
        p
        for p in directory.rglob("*")
        if p.is_file()
        and p.suffix in _EXAMPLE_FILE_SUFFIXES
        and not any(part in _SKIP_PARTS or part.startswith(".") for part in p.relative_to(directory).parts)
    ]


def _example_dirs() -> dict[str, list[Path]]:
    """Example directories (not hidden, not caches) that hold example files."""
    out = {}
    for p in EXAMPLES_DIR.iterdir():
        if p.is_dir() and not p.name.startswith((".", "_")):
            files = _example_files(p)
            if files:
                out[p.name] = files
    return out


def _python_suite_examples() -> set[str]:
    def is_python(name: str, files: list[Path]) -> bool:
        top = EXAMPLES_DIR / name
        return any(
            f.suffix in _PYTHON_SUITE_SUFFIXES or (f.parent == top and f.suffix in _PYTHON_SUITE_TOP_LEVEL_SUFFIXES)
            for f in files
        )

    return {name for name, files in _example_dirs().items() if is_python(name, files)}


def test_every_example_is_accounted_for() -> None:
    dirs = _python_suite_examples()
    covered = set(OFFLINE_RUN) | set(VALIDATE) | set(IMPORT_ONLY) | set(ELSEWHERE)
    assert not dirs - covered, f"Python examples with no test: {sorted(dirs - covered)}"
    assert not covered - dirs, f"tests for examples that do not exist: {sorted(covered - dirs)}"


def test_readme_example_count_matches() -> None:
    readme = EXAMPLES_DIR.parent / "README.md"
    match = re.search(r"All (\d+) examples", readme.read_text())
    assert match, "README.md no longer says 'All N examples'; update this test with it"
    assert int(match.group(1)) == len(_example_dirs())


@pytest.mark.parametrize("example", sorted(OFFLINE_RUN))
def test_offline_example_runs(example: str) -> None:
    script = EXAMPLES_DIR / example / OFFLINE_RUN[example]
    assert script.is_file(), f"{script} missing"
    proc = subprocess.run(
        [sys.executable, script.name],
        cwd=script.parent,
        capture_output=True,
        text=True,
        timeout=120,
        env=_offline_env(),
    )
    assert proc.returncode == 0, f"{example} exit={proc.returncode}\n{proc.stdout[-2000:]}\n{proc.stderr[-4000:]}"


@pytest.mark.parametrize("example", sorted(VALIDATE))
def test_yaml_example_validates(example: str) -> None:
    path = EXAMPLES_DIR / example / VALIDATE[example]
    source = path.read_text()
    with warnings.catch_warnings():
        warnings.simplefilter("error", WorkflowSchemaWarning)
        data = yaml.safe_load(source)
        irs = compile_bundle(data).workflows if is_bundle(data) else [compile_yaml(source)]
    assert irs, f"{example}: compiled to no workflows"
    for ir in irs:
        assert ir["nodes"], f"{example}: workflow {ir['workflow_id']} compiled to zero nodes"


_IMPORT_AND_COMPILE = textwrap.dedent(
    """
    import runpy, sys
    from pathlib import Path

    path = Path(sys.argv[1])
    sys.path.insert(0, str(path.parent))
    namespace = runpy.run_path(str(path), run_name="example_under_test")

    from jamjet import Agent, Workflow

    for value in list(namespace.values()):
        if isinstance(value, (Agent, Workflow)):
            value.compile()
    """
)


@pytest.mark.parametrize("example", sorted(IMPORT_ONLY))
def test_model_example_imports_and_compiles(example: str) -> None:
    script = EXAMPLES_DIR / example / IMPORT_ONLY[example]
    assert script.is_file(), f"{script} missing"
    proc = subprocess.run(
        [sys.executable, "-c", _IMPORT_AND_COMPILE, script.name],
        cwd=script.parent,
        capture_output=True,
        text=True,
        timeout=120,
        env=_offline_env(),
    )
    assert proc.returncode == 0, f"{example} exit={proc.returncode}\n{proc.stderr[-4000:]}"


# Graph workflows (not strategy agents, whose prompts the strategy compiler writes).
GRAPH_YAML = sorted(name for name, file in VALIDATE.items() if file == "workflow.yaml")


@pytest.mark.parametrize("example", GRAPH_YAML)
def test_graph_example_prompts_read_only_what_the_engine_writes(example: str) -> None:
    from tests.test_yaml_strict import state_reads_nothing_writes

    assert state_reads_nothing_writes((EXAMPLES_DIR / example / VALIDATE[example]).read_text()) == []
