"""
JamJet safety demos — zero-setup, deterministic, no API key required.

These demos prove the policy / approval / budget / MCP-shaped enforcement
paths work. The model is mocked. The enforcement path is real.
"""

from __future__ import annotations

import json
from pathlib import Path

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
    from jamjet.cli._demo_agent import DeterministicDemoAgent
    from jamjet.cli._demo_audit import DemoAuditEvent, write_audit_event
    from jamjet.cloud.policy import PolicyEvaluator

    evaluator = PolicyEvaluator()
    evaluator.add("block", "*delete*")

    agent = DeterministicDemoAgent(scenario="unsafe-tool-call")
    plan = agent.plan_tool_calls()[0]
    decision = evaluator.evaluate(plan.tool)

    event = DemoAuditEvent(
        run_id="unsafe-tool-call-001",
        demo="unsafe-tool-call",
        decision="BLOCKED" if decision.blocked else "ALLOWED",
        tool=plan.tool,
        rule=decision.pattern,
        executed=False if decision.blocked else True,
    )
    audit_path = write_audit_event(event)

    if json_output:
        typer.echo(json.dumps(event.to_dict(), indent=2, sort_keys=True))
        return

    typer.echo("JamJet demo: unsafe tool-call blocking")
    typer.echo("")
    typer.echo(f"  Agent:    {agent.name()}")
    typer.echo(f"  Planned:  {plan.tool}({plan.arguments!r})")
    typer.echo("")
    typer.echo("  Policy:")
    typer.echo("    block tools matching '*delete*'")
    typer.echo("")
    typer.echo(f"  Decision: {event.decision}  (rule: {event.rule})")
    typer.echo(f"  Executed: {str(event.executed).lower()}")
    typer.echo("")
    typer.echo(f"  Audit:    {audit_path}")
    typer.echo("")
    typer.echo("  The model is mocked. The enforcement path is real.")


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
