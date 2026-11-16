"""Guards on the package metadata a first install depends on."""

from __future__ import annotations

import re
import tomllib
from pathlib import Path

import pytest

SDK_ROOT = Path(__file__).resolve().parents[1]
CI_WORKFLOW = SDK_ROOT.parents[1] / ".github" / "workflows" / "ci.yml"
DEPENDABOT_CONFIG = SDK_ROOT.parents[1] / ".github" / "dependabot.yml"


def _pyproject() -> dict:
    return tomllib.loads((SDK_ROOT / "pyproject.toml").read_text())


def test_model_extra_keeps_litellm_below_the_mcp2_import() -> None:
    # litellm 1.106 imports mcp 2.x-only types whenever a completion passes
    # tools, but lists mcp only under its own extras. An unbounded `litellm`
    # gave the first agent from `jamjet create` ModuleNotFoundError: mcp, and
    # `litellm[mcp]` would drag in mcp 2.x and break FastMCP (mcp 1.x) code.
    from packaging.requirements import Requirement

    model = [Requirement(r) for r in _pyproject()["project"]["optional-dependencies"]["model"]]
    litellm = [r for r in model if r.name == "litellm"]
    assert litellm, f"the model extra has no litellm requirement: {model}"
    spec = litellm[0].specifier
    assert "1.105.7" in spec, f"litellm 1.105.7 should be allowed: {spec}"
    assert "1.106.0" not in spec, f"litellm 1.106+ imports mcp 2.x on the completion path: {spec}"
    assert "mcp" not in litellm[0].extras, "litellm[mcp] pulls in mcp 2.x"
    for r in model:
        if r.name == "mcp":
            assert "2.0.0" not in r.specifier, f"the model extra must not require mcp 2.x: {r}"


def test_dependabot_ignores_litellm_only_while_it_is_capped() -> None:
    # For a range like ours Dependabot can only propose raising the upper
    # bound, and the test above fails every such PR. So the bound and the
    # ignore go together: with the bound and no ignore, a red PR comes back
    # every week; with the ignore and no bound, litellm updates stop coming.
    import yaml
    from packaging.requirements import Requirement

    if not DEPENDABOT_CONFIG.is_file():
        pytest.skip("Dependabot config not in this checkout")
    updates = yaml.safe_load(DEPENDABOT_CONFIG.read_text())["updates"]
    pip = [u for u in updates if u["package-ecosystem"] == "pip" and u["directory"] == "/sdk/python"]
    assert len(pip) == 1, f"expected one pip entry for /sdk/python, got {len(pip)}"
    # An entry with only a name ignores every version of that dependency.
    ignored = [e["dependency-name"] for e in pip[0].get("ignore", []) if set(e) == {"dependency-name"}]

    model = [Requirement(r) for r in _pyproject()["project"]["optional-dependencies"]["model"]]
    litellm = [r for r in model if r.name == "litellm"]
    assert len(litellm) == 1, f"expected one litellm requirement in the model extra: {model}"
    capped = any(s.operator in ("<", "<=") for s in litellm[0].specifier)

    if capped:
        assert "litellm" in ignored, "litellm has an upper bound, so Dependabot must ignore it"
    else:
        assert "litellm" not in ignored, "litellm has no upper bound now; remove its Dependabot ignore"


def test_supported_python_floor_agrees_everywhere() -> None:
    project = _pyproject()["project"]
    floor = re.fullmatch(r">=\s*(3\.\d+)", project["requires-python"])
    assert floor, f"unexpected requires-python: {project['requires-python']}"
    classified = sorted(
        (c.rsplit(" :: ", 1)[1] for c in project["classifiers"] if re.fullmatch(r".* :: 3\.\d+", c)),
        key=lambda v: tuple(map(int, v.split("."))),
    )
    assert classified and classified[0] == floor.group(1), (
        f"requires-python floor {floor.group(1)} but lowest classifier {classified[:1]}"
    )
    if not CI_WORKFLOW.is_file():
        pytest.skip("CI workflow not in this checkout")
    matrix = re.search(r"python-version: \[([^\]]*)\]", CI_WORKFLOW.read_text())
    assert matrix, "no python-version matrix in the CI workflow"
    versions = [v.strip().strip('"') for v in matrix.group(1).split(",")]
    assert floor.group(1) in versions, f"CI does not test the floor {floor.group(1)}: {versions}"


def test_memory_extra_is_limited_to_pythons_engram_supports() -> None:
    # jamjet-engram requires Python 3.12+; on 3.11 the extra must install
    # nothing rather than fail the whole install.
    memory = _pyproject()["project"]["optional-dependencies"]["memory"]
    assert memory and all("python_version >= '3.12'" in r for r in memory), memory
