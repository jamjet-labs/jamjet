"""
JamJet safety demos — zero-setup, deterministic, no API key required.

These demos prove the policy / approval / budget / MCP-shaped enforcement
paths work. The model is mocked. The enforcement path is real.
"""

from __future__ import annotations

import json
import re
import uuid
from pathlib import Path

import typer

_RUN_ID_RE = re.compile(r"^[A-Za-z0-9._-]+$")

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
        rule_kind="block" if decision.blocked else ("allow" if decision.pattern else None),
        executed=False if decision.blocked else True,
        trace_id="jj_7f21c9",
        decision_id="dec_91ab2",
        args=dict(plan.arguments),
    )
    audit_path = write_audit_event(event)

    if json_output:
        typer.echo(json.dumps(event.to_dict(), indent=2, sort_keys=True))
        return

    typer.echo("JamJet demo: unsafe tool-call blocking")
    typer.echo("")
    typer.echo("Scenario:")
    typer.echo("  An AI agent wants to clean up old customer records.")
    typer.echo("")
    typer.echo(f"Agent ({agent.name()}) requested tool:")
    typer.echo(f"  {plan.tool}({plan.arguments!r})")
    typer.echo("")
    typer.echo("Policy check:")
    typer.echo("  blocked patterns: '*delete*'")
    typer.echo("")
    typer.echo(f"Decision: {event.decision}")
    typer.echo(f"Reason:   tool name matches blocked pattern '{event.rule}'")
    typer.echo("")
    typer.echo("Audit event:")
    typer.echo(f"  trace_id:    {event.trace_id}")
    typer.echo(f"  decision_id: {event.decision_id}")
    typer.echo(f"  executed:    {str(event.executed).lower()}")
    typer.echo(f"  audit_path:  {audit_path}")
    typer.echo("")
    typer.echo("The model is mocked. The enforcement path is real.")


@demo_app.command("approval")
def approval(
    approve: str | None = typer.Option(None, "--approve", help="Run ID to approve."),
    json_output: bool = typer.Option(False, "--json", help="Machine-readable audit event."),
) -> None:
    """Mock agent attempts a risky action. JamJet pauses for approval; --approve <id> resumes."""
    from jamjet.cli._demo_agent import DeterministicDemoAgent
    from jamjet.cli._demo_audit import DemoAuditEvent, write_audit_event

    runs_dir = Path.cwd() / ".jamjet-demo" / "runs"

    if approve:
        if not _RUN_ID_RE.fullmatch(approve):
            typer.echo(f"Invalid run id: {approve}", err=True)
            raise typer.Exit(code=1)
        path = (runs_dir / f"{approve}.json").resolve()
        if path.parent != runs_dir.resolve():
            typer.echo(f"Invalid run id: {approve}", err=True)
            raise typer.Exit(code=1)
        if not path.exists():
            typer.echo(f"No pending approval found for run id: {approve}", err=True)
            raise typer.Exit(code=1)
        state = json.loads(path.read_text())
        if state["decision"] != "WAITING_FOR_APPROVAL":
            typer.echo(f"Run {approve} is not waiting for approval (state: {state['decision']}).", err=True)
            raise typer.Exit(code=1)
        state["decision"] = "APPROVED"
        state["executed"] = True
        path.write_text(json.dumps(state, indent=2, sort_keys=True))
        if json_output:
            typer.echo(json.dumps(state, indent=2, sort_keys=True))
            return
        typer.echo(f"Approved: {approve}")
        typer.echo(f"Tool executed: {state['tool']}")
        typer.echo(f"Audit:    {path}")
        typer.echo("")
        typer.echo("  The model is mocked. The enforcement path is real.")
        return

    agent = DeterministicDemoAgent(scenario="approval")
    plan = agent.plan_tool_calls()[0]
    run_id = f"approval-{uuid.uuid4().hex[:12]}"

    event = DemoAuditEvent(
        run_id=run_id,
        demo="approval",
        decision="WAITING_FOR_APPROVAL",
        tool=plan.tool,
        rule="payments.* requires approval",
        rule_kind="require_approval",
        executed=False,
        args=dict(plan.arguments),
        extra={"arguments": plan.arguments},
    )
    audit_path = write_audit_event(event)

    if json_output:
        typer.echo(json.dumps(event.to_dict(), indent=2, sort_keys=True))
        return

    typer.echo("JamJet demo: human approval")
    typer.echo("")
    typer.echo(f"  Agent:    {agent.name()}")
    typer.echo(f"  Planned:  {plan.tool}({plan.arguments!r})")
    typer.echo("")
    typer.echo("  Policy: payments.* requires approval")
    typer.echo("")
    typer.echo("  Decision: WAITING_FOR_APPROVAL")
    typer.echo(f"  Run ID:   {run_id}")
    typer.echo("")
    typer.echo(f"  Approve with:   jamjet demo approval --approve {run_id}")
    typer.echo(f"  Audit:    {audit_path}")
    typer.echo("")
    typer.echo("  The model is mocked. The enforcement path is real.")


@demo_app.command("budget-cap")
def budget_cap(
    json_output: bool = typer.Option(False, "--json", help="Machine-readable audit event."),
) -> None:
    """Mock agent loop hits a $0.05 budget cap. JamJet blocks before further spend."""
    from jamjet.cli._demo_agent import DeterministicDemoAgent
    from jamjet.cli._demo_audit import DemoAuditEvent, write_audit_event

    cap_usd = 0.05
    agent = DeterministicDemoAgent(scenario="budget-cap")
    plans = agent.plan_tool_calls()

    spent = 0.0
    log: list[dict[str, object]] = []
    blocked_at: int | None = None
    for i, plan in enumerate(plans, start=1):
        if spent + plan.estimated_cost_usd > cap_usd:
            log.append({"step": i, "tool": plan.tool, "cost": plan.estimated_cost_usd, "decision": "BUDGET_EXCEEDED"})
            blocked_at = i
            break
        spent += plan.estimated_cost_usd
        log.append({"step": i, "tool": plan.tool, "cost": plan.estimated_cost_usd, "decision": "ALLOWED"})

    event = DemoAuditEvent(
        run_id="budget-cap-001",
        demo="budget-cap",
        decision="BUDGET_EXCEEDED" if blocked_at else "ALLOWED",
        tool=plans[blocked_at - 1].tool if blocked_at else "—",
        rule=f"budget cap ${cap_usd:.2f}",
        rule_kind=None,  # budget is a separate concern, not a rule action
        executed=False if blocked_at else True,
        args=dict(plans[blocked_at - 1].arguments) if blocked_at else {},
        extra={"spent_usd": round(spent, 2), "cap_usd": cap_usd, "log": log},
    )
    audit_path = write_audit_event(event)

    if json_output:
        typer.echo(json.dumps(event.to_dict(), indent=2, sort_keys=True))
        return

    typer.echo("JamJet demo: budget cap")
    typer.echo("")
    typer.echo(f"  Agent:    {agent.name()}")
    typer.echo(f"  Budget:   ${cap_usd:.2f}")
    typer.echo("")
    for entry in log:
        marker = "✓" if entry["decision"] == "ALLOWED" else "✗"
        typer.echo(f"  {marker} Step {entry['step']}  {entry['tool']}  ${entry['cost']:.2f}  {entry['decision']}")
    typer.echo("")
    typer.echo(f"  Spent:    ${spent:.2f}")
    typer.echo(f"  Decision: {event.decision}")
    typer.echo(f"  Audit:    {audit_path}")
    typer.echo("")
    typer.echo("  The model is mocked. The enforcement path is real.")


@demo_app.command("mcp-tool-policy")
def mcp_tool_policy(
    json_output: bool = typer.Option(False, "--json", help="Machine-readable audit event."),
) -> None:
    """Mock MCP-shaped tool request. JamJet evaluates policy. Foreshadows JamJet Gateway."""
    from jamjet.cli._demo_agent import DeterministicDemoAgent
    from jamjet.cli._demo_audit import DemoAuditEvent, write_audit_event
    from jamjet.cloud.policy import PolicyEvaluator

    evaluator = PolicyEvaluator()
    evaluator.add("block", "*delete*")

    agent = DeterministicDemoAgent(scenario="mcp-tool-policy")
    plan = agent.plan_tool_calls()[0]
    decision = evaluator.evaluate(plan.tool)

    event = DemoAuditEvent(
        run_id="mcp-tool-policy-001",
        demo="mcp-tool-policy",
        decision="BLOCKED" if decision.blocked else "ALLOWED",
        tool=plan.tool,
        rule=decision.pattern,
        rule_kind="block" if decision.blocked else ("allow" if decision.pattern else None),
        executed=False if decision.blocked else True,
        server="postgres-mcp",
        args=dict(plan.arguments),
        extra={
            "server": "postgres-mcp",
            "envelope": {"server": "postgres-mcp", "tool": plan.tool, "arguments": plan.arguments},
        },
    )
    audit_path = write_audit_event(event)

    if json_output:
        typer.echo(json.dumps(event.to_dict(), indent=2, sort_keys=True))
        return

    typer.echo("JamJet demo: MCP tool policy")
    typer.echo("")
    typer.echo("  This demo uses an MCP-shaped request envelope to show policy evaluation.")
    typer.echo("  It is not yet an MCP proxy. Full MCP proxy support is planned for JamJet Gateway.")
    typer.echo("")
    typer.echo(f"  Agent:    {agent.name()}")
    typer.echo("  MCP request:")
    typer.echo("    server: postgres-mcp")
    typer.echo(f"    tool:   {plan.tool}")
    typer.echo("")
    typer.echo("  Policy: block tools matching '*delete*'")
    typer.echo("")
    typer.echo(f"  Decision: {event.decision}  (rule: {event.rule})")
    typer.echo(f"  Executed: {str(event.executed).lower()}")
    typer.echo(f"  Audit:    {audit_path}")
    typer.echo("")
    typer.echo("  The model is mocked. The enforcement path is real.")
