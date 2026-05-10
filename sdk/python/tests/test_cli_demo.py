from typer.testing import CliRunner

from jamjet.cli.main import app

runner = CliRunner()


def test_demo_help_lists_four_subcommands():
    result = runner.invoke(app, ["demo", "--help"])
    assert result.exit_code == 0
    assert "unsafe-tool-call" in result.stdout
    assert "approval" in result.stdout
    assert "budget-cap" in result.stdout
    assert "mcp-tool-policy" in result.stdout
