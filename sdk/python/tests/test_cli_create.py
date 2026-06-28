"""Tests for `jamjet create` and the quickstart template."""

from __future__ import annotations

import ast

from typer.testing import CliRunner

from jamjet.cli.main import app
from jamjet.templates import AVAILABLE_TEMPLATES, render_template

runner = CliRunner()

# ---------------------------------------------------------------------------
# Template unit tests
# ---------------------------------------------------------------------------


def test_quickstart_in_available_templates():
    assert "quickstart" in AVAILABLE_TEMPLATES


def test_quickstart_renders_three_files():
    files = render_template("quickstart", "myagent")
    assert "agent.py" in files
    assert "README.md" in files
    assert "pyproject.toml" in files


def test_quickstart_substitutes_name():
    files = render_template("quickstart", "myagent")
    agent_src = files["agent.py"]
    assert "myagent" in agent_src
    # Placeholder must be replaced -- no literal {name} left
    assert "{name}" not in agent_src

    readme = files["README.md"]
    assert "myagent" in readme
    assert "{name}" not in readme

    toml = files["pyproject.toml"]
    assert "myagent" in toml
    assert "{name}" not in toml


def test_quickstart_agent_py_is_valid_python():
    files = render_template("quickstart", "myagent")
    agent_src = files["agent.py"]
    # Must parse without SyntaxError
    tree = ast.parse(agent_src)
    assert tree is not None


def test_quickstart_agent_py_imports_jamjet():
    files = render_template("quickstart", "myagent")
    agent_src = files["agent.py"]
    # Must import Agent and tool from jamjet
    assert "from jamjet import Agent, tool" in agent_src


def test_quickstart_agent_py_has_tool_decorator():
    files = render_template("quickstart", "myagent")
    agent_src = files["agent.py"]
    assert "@tool" in agent_src


def test_quickstart_agent_py_has_agent_constructor():
    files = render_template("quickstart", "myagent")
    agent_src = files["agent.py"]
    assert "Agent(" in agent_src


def test_quickstart_agent_py_uses_real_model_string():
    """The scaffold must reference the real model string, not a placeholder."""
    files = render_template("quickstart", "myagent")
    agent_src = files["agent.py"]
    assert "anthropic/claude-sonnet-4-6" in agent_src


def test_quickstart_pyproject_has_jamjet_dependency():
    files = render_template("quickstart", "myagent")
    toml = files["pyproject.toml"]
    assert "jamjet" in toml


def test_quickstart_pyproject_dependency_includes_model_extra():
    """The generated dependency must carry the [model] extra: the quickstart's
    "anthropic/claude-sonnet-4-6" model is called through the in-process litellm
    seam, which the [model] extra provides. A bare `jamjet` dep cannot run it."""
    files = render_template("quickstart", "myagent")
    toml = files["pyproject.toml"]
    assert "jamjet[model]" in toml
    # The README install command must match the dependency so a fresh project runs.
    readme = files["README.md"]
    assert "jamjet[model]" in readme


# ---------------------------------------------------------------------------
# `jamjet create` CLI tests
# ---------------------------------------------------------------------------


def test_create_writes_expected_files(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["create", "myagent"])
    assert result.exit_code == 0, result.output

    assert (tmp_path / "myagent" / "agent.py").exists()
    assert (tmp_path / "myagent" / "README.md").exists()
    assert (tmp_path / "myagent" / "pyproject.toml").exists()


def test_create_substitutes_name_in_files(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    runner.invoke(app, ["create", "myagent"])

    agent_src = (tmp_path / "myagent" / "agent.py").read_text()
    assert "myagent" in agent_src
    assert "{name}" not in agent_src


def test_create_rendered_agent_is_valid_python(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    runner.invoke(app, ["create", "myagent"])

    agent_src = (tmp_path / "myagent" / "agent.py").read_text()
    tree = ast.parse(agent_src)
    assert tree is not None


def test_create_fails_if_directory_exists(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    (tmp_path / "myagent").mkdir()

    result = runner.invoke(app, ["create", "myagent"])
    assert result.exit_code != 0
    assert "already exists" in result.output


def test_create_does_not_overwrite_existing_dir(tmp_path, monkeypatch):
    """Files in an existing directory must not be touched."""
    monkeypatch.chdir(tmp_path)
    existing_dir = tmp_path / "myagent"
    existing_dir.mkdir()
    sentinel = existing_dir / "sentinel.txt"
    sentinel.write_text("do not overwrite")

    runner.invoke(app, ["create", "myagent"])

    assert sentinel.read_text() == "do not overwrite"


def test_create_prints_cd_hint(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["create", "myagent"])
    assert result.exit_code == 0
    assert "cd myagent" in result.output


def test_create_default_template_is_quickstart(tmp_path, monkeypatch):
    """create with no --template flag uses quickstart."""
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["create", "myagent"])
    assert result.exit_code == 0
    assert "quickstart" in result.output


def test_create_with_explicit_template(tmp_path, monkeypatch):
    """create --template hello-agent should work."""
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["create", "myproj", "--template", "hello-agent"])
    assert result.exit_code == 0
    assert (tmp_path / "myproj" / "workflow.yaml").exists()


def test_create_unknown_template_fails(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["create", "myagent", "--template", "nonexistent"])
    assert result.exit_code != 0
    assert "unknown template" in result.output.lower() or "unknown template" in result.output


# ---------------------------------------------------------------------------
# Confirm `init` still works (regression guard)
# ---------------------------------------------------------------------------


def test_init_still_works(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["init", "myproj"])
    assert result.exit_code == 0
    assert (tmp_path / "myproj" / "workflow.yaml").exists()


def test_init_list_templates_still_works(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["init", "--list-templates"])
    assert result.exit_code == 0
    assert "hello-agent" in result.output
    assert "quickstart" in result.output
