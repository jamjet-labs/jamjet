"""The moat: outside jamjet.model, no module imports a provider SDK.

Fail-closed by default: every *.py under the jamjet package is scanned.
Only the files/directories listed in EXEMPT_PATHS / EXEMPT_DIR_PREFIXES may
import {litellm, openai, anthropic} directly. Adding a new file that
imports a provider SDK will cause this test to fail, forcing an explicit
exemption decision.
"""

import ast
import pathlib

import jamjet

PKG = pathlib.Path(jamjet.__file__).parent

# ---------------------------------------------------------------------------
# Exempt set — files / directories whose provider-SDK imports are intentional.
# ---------------------------------------------------------------------------
# model/litellm_backend.py : the ONE sanctioned caller of litellm (behind the seam)
# cloud/patcher.py         : telemetry instrumentation wraps the openai client
# eval/scorers.py          : offline LLM-as-judge (used in test runs, not runtime)
EXEMPT_PATHS: frozenset[str] = frozenset(
    {
        "model/litellm_backend.py",
        "cloud/patcher.py",
        "eval/scorers.py",
    }
)

# integrations/      : provider/framework integration adapters (openai_guardrail etc.)
# anthropic_agent/   : thin Anthropic-native agent shim
# langchain/         : LangChain bridge shim
# crewai/            : CrewAI bridge shim
# adk/               : Google ADK bridge shim
# openai_agents/     : OpenAI Agents SDK bridge shim
# templates/         : scaffolding/codegen — may lazy-import providers for demo stubs
EXEMPT_DIR_PREFIXES: tuple[str, ...] = (
    "integrations/",
    "anthropic_agent/",
    "langchain/",
    "crewai/",
    "adk/",
    "openai_agents/",
    "templates/",
)

FORBIDDEN: frozenset[str] = frozenset({"litellm", "openai", "anthropic"})


# ---------------------------------------------------------------------------
# AST helpers
# ---------------------------------------------------------------------------


def _all_imports(path: pathlib.Path) -> set[str]:
    """Return root module names for every import at ANY nesting depth.

    Uses recursive ``ast.walk`` so lazy / deferred imports inside functions
    or methods are also caught.
    """
    tree = ast.parse(path.read_text())
    names: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            names.update(alias.name.split(".")[0] for alias in node.names)
        elif isinstance(node, ast.ImportFrom) and node.module:
            names.add(node.module.split(".")[0])
    return names


def _top_level_imports(path: pathlib.Path) -> set[str]:
    """Return root module names imported at module scope only (not inside functions/classes)."""
    tree = ast.parse(path.read_text())
    names: set[str] = set()
    for node in tree.body:  # module-level statements only
        if isinstance(node, ast.Import):
            names.update(alias.name.split(".")[0] for alias in node.names)
        elif isinstance(node, ast.ImportFrom) and node.module:
            names.add(node.module.split(".")[0])
    return names


def _is_exempt(rel: str) -> bool:
    """Return True if this POSIX path (relative to the jamjet package root) is exempt."""
    if rel in EXEMPT_PATHS:
        return True
    return any(rel.startswith(prefix) for prefix in EXEMPT_DIR_PREFIXES)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_no_unexempt_module_imports_a_provider_sdk() -> None:
    """Every *.py under jamjet/ must route model calls through the seam.

    Scans the entire package tree. Any file outside the exempt set that
    imports a provider SDK directly will be listed in the failure message.
    """
    offenders: dict[str, set[str]] = {}
    for py_file in sorted(PKG.rglob("*.py")):
        rel = py_file.relative_to(PKG).as_posix()
        if _is_exempt(rel):
            continue
        leaked = _all_imports(py_file) & FORBIDDEN
        if leaked:
            offenders[rel] = leaked
    assert not offenders, (
        "provider SDK imported outside the exempt set"
        " (route through jamjet.model instead):\n"
        + "\n".join(f"  {k}: {v}" for k, v in sorted(offenders.items()))
    )


def test_only_litellm_backend_imports_litellm() -> None:
    # Sanity: the seam's backend is the intended single importer.
    # Uses _top_level_imports so the intentional lazy `import litellm` inside
    # _import_litellm() is correctly NOT flagged, while a top-level import would be.
    backend_imports = _top_level_imports(PKG / "model" / "litellm_backend.py")
    assert "litellm" not in backend_imports
