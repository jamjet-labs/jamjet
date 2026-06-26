"""The moat: outside jamjet.model, no hot-path module imports a provider SDK."""

import ast
import pathlib

import jamjet

PKG = pathlib.Path(jamjet.__file__).parent

HOT_PATH = [
    "runtime/local/executor.py",
    "runtime/local/strategies/plan_and_execute.py",
    "runtime/local/strategies/base.py",
    "runtime/local/llm_adapters/__init__.py",
    "runtime/local/llm_adapters/seam_adapter.py",
    "agents/agent.py",
    "llm/client.py",
    "coordinator/default_strategy.py",
]
FORBIDDEN = {"litellm", "openai", "anthropic"}


def _all_imports(path: pathlib.Path) -> set[str]:
    """Return root module names for every import at ANY nesting depth (catches lazy/deferred imports)."""
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


def test_hot_path_modules_do_not_import_provider_sdks():
    # Uses _all_imports so lazy/deferred provider imports inside functions are also caught.
    offenders = {}
    for rel in HOT_PATH:
        imported = _all_imports(PKG / rel)
        leaked = imported & FORBIDDEN
        if leaked:
            offenders[rel] = leaked
    assert not offenders, f"provider SDK imported outside jamjet.model: {offenders}"


def test_only_litellm_backend_imports_litellm():
    # Sanity: the seam's backend is the intended single importer.
    # Uses _top_level_imports so the intentional lazy `import litellm` inside
    # _import_litellm() is correctly NOT flagged, while a top-level import would be.
    backend_imports = _top_level_imports(PKG / "model" / "litellm_backend.py")
    assert "litellm" not in backend_imports
