"""
JamJet safety demos — zero-setup, deterministic, no API key required.

These demos prove the policy / approval / budget / MCP-shaped enforcement
paths work. The model is mocked. The enforcement path is real.
"""

from __future__ import annotations

import typer

demo_app = typer.Typer(
    name="demo",
    help="Zero-setup safety demos. No API key. No Docker. No cloud.",
    no_args_is_help=True,
)


@demo_app.command("unsafe-tool-call")
def unsafe_tool_call(
    json_output: bool = typer.Option(False, "--json", help="Machine-readable audit event."),
) -> None:
    """Mock agent attempts a destructive tool call. JamJet blocks it before execution."""
    raise NotImplementedError


@demo_app.command("approval")
def approval(
    approve: str | None = typer.Option(None, "--approve", help="Run ID to approve."),
    json_output: bool = typer.Option(False, "--json", help="Machine-readable audit event."),
) -> None:
    """Mock agent attempts a risky action. JamJet pauses for approval; --approve <id> resumes."""
    raise NotImplementedError


@demo_app.command("budget-cap")
def budget_cap(
    json_output: bool = typer.Option(False, "--json", help="Machine-readable audit event."),
) -> None:
    """Mock agent loop hits a $0.05 budget cap. JamJet blocks before further spend."""
    raise NotImplementedError


@demo_app.command("mcp-tool-policy")
def mcp_tool_policy(
    json_output: bool = typer.Option(False, "--json", help="Machine-readable audit event."),
) -> None:
    """Mock MCP-shaped tool request. JamJet evaluates policy. Foreshadows JamJet Gateway."""
    raise NotImplementedError
