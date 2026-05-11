"""JamJet policy guardrail for the OpenAI Agents SDK.

Usage::

    from openai_agents import tool
    from jamjet.integrations.openai_guardrail import jamjet_guardrail

    refund = tool(
        name="payments.refund",
        input_guardrails=[jamjet_guardrail(policy="~/.jamjet/policy.yaml")],
        execute=refund_customer,
    )

Loads policy from the canonical lookup order: explicit path > ``JAMJET_POLICY_FILE``
env > cwd ``./policy.yaml`` > ``~/.jamjet/policy.yaml``. Emits audit events conformant
with the v1 portable schema to ``~/.jamjet/audit/<YYYY-MM-DD>/openai-guardrail.jsonl``.

Note: we define a small ``_AuditEvent`` dataclass parallel to
``jamjet.cli._demo_audit.DemoAuditEvent`` rather than reusing the latter because
``DemoAuditEvent`` carries a ``demo`` field that is meaningless outside the
``jamjet demo`` flow. Both serialise to the same v1 schema shape.
"""

from __future__ import annotations

import json
import os
import secrets
from collections.abc import Callable
from dataclasses import asdict, dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Literal

import yaml

from jamjet.cloud.policy import PolicyEvaluator


class JamjetPolicyBlocked(RuntimeError):
    """Raised when JamJet policy blocks a tool call."""

    def __init__(self, tool: str, rule: str | None) -> None:
        super().__init__(f"JamJet policy: BLOCKED (tool: {tool}, rule: {rule or 'unknown'})")
        self.tool = tool
        self.rule = rule


class JamjetApprovalRequired(RuntimeError):
    """Raised when JamJet policy requires approval for a tool call.

    v0.1 surfaces as an exception; v0.2 will integrate with the OpenAI Agents SDK
    approval API + JamJet's ApprovalQueue.
    """

    def __init__(self, tool: str, rule: str | None) -> None:
        super().__init__(
            f"JamJet policy: WAITING_FOR_APPROVAL (tool: {tool}, rule: {rule or 'unknown'})"
        )
        self.tool = tool
        self.rule = rule


@dataclass
class _AuditEvent:
    run_id: str
    decision: str
    tool: str
    args: dict[str, Any]
    rule: str | None
    rule_kind: Literal["allow", "block", "require_approval", "audit"] | None
    executed: bool
    adapter: str = "openai-guardrail"
    host: str = "openai-agents-sdk"
    server: str | None = None
    trace_id: str | None = None
    decision_id: str | None = None
    policy_version: str = "1"
    schema_version: int = 1
    ts: str = field(default_factory=lambda: datetime.now(UTC).isoformat())


def _resolve_policy_path(explicit: str | None) -> Path:
    if explicit:
        return Path(os.path.expanduser(explicit))
    env_path = os.environ.get("JAMJET_POLICY_FILE")
    if env_path:
        return Path(os.path.expanduser(env_path))
    cwd_candidate = Path.cwd() / "policy.yaml"
    if cwd_candidate.exists():
        return cwd_candidate
    home_candidate = Path.home() / ".jamjet" / "policy.yaml"
    if home_candidate.exists():
        return home_candidate
    raise FileNotFoundError(
        "No policy file found. Set JAMJET_POLICY_FILE, or place policy.yaml in cwd or ~/.jamjet/"
    )


def _load_policy_into_evaluator(path: Path) -> PolicyEvaluator:
    raw = yaml.safe_load(path.read_text())
    if not isinstance(raw, dict) or raw.get("version") != 1:
        version = raw.get("version") if isinstance(raw, dict) else "n/a"
        raise ValueError(f"unsupported policy version: {version}")
    rules = raw.get("rules") or []
    ev = PolicyEvaluator()
    for i, rule in enumerate(rules):
        if not isinstance(rule, dict) or "match" not in rule or "action" not in rule:
            raise ValueError(f"rule[{i}] missing match/action")
        action = rule["action"]
        if action not in {"allow", "block", "require_approval", "audit"}:
            raise ValueError(f"rule[{i}]: unknown action: {action}")
        ev.add(action, rule["match"])
    return ev


def _write_audit(event: _AuditEvent, audit_destination: str | None) -> None:
    base = (
        Path(os.path.expanduser(audit_destination))
        if audit_destination
        else Path.home() / ".jamjet" / "audit"
    )
    day_dir = base / event.ts[:10]
    day_dir.mkdir(parents=True, exist_ok=True)
    path = day_dir / "openai-guardrail.jsonl"
    payload = asdict(event)
    # Backward-compat alias for jamjet 0.8.1 consumers that read `timestamp`.
    payload["timestamp"] = payload["ts"]
    existing = path.read_text() if path.exists() else ""
    path.write_text(existing + json.dumps(payload, sort_keys=True) + "\n")


def jamjet_guardrail(
    *, policy: str | None = None, audit_destination: str | None = None
) -> Callable[[dict[str, Any]], None]:
    """Build a JamJet guardrail callable for the OpenAI Agents SDK.

    Returns a callable taking ``{"toolName": str, "toolArgs": dict}`` and either
    raising (BLOCKED / WAITING_FOR_APPROVAL) or returning None (ALLOWED / AUDIT).
    """
    evaluator = _load_policy_into_evaluator(_resolve_policy_path(policy))

    def guardrail(input: dict[str, Any]) -> None:
        tool_name = str(input.get("toolName", ""))
        tool_args = input.get("toolArgs") or {}
        d = evaluator.evaluate(tool_name)

        rule_kind: Literal["allow", "block", "require_approval", "audit"] | None
        if d.blocked:
            decision, rule_kind, executed = "BLOCKED", "block", False
        elif d.policy_kind == "require_approval":
            decision, rule_kind, executed = "WAITING_FOR_APPROVAL", "require_approval", False
        elif d.policy_kind == "audit":
            decision, rule_kind, executed = "AUDIT", "audit", True
        else:
            decision = "ALLOWED"
            rule_kind = "allow" if d.pattern else None
            executed = True

        event = _AuditEvent(
            run_id=f"run_{secrets.token_hex(6)}",
            decision=decision,
            tool=tool_name,
            args=dict(tool_args) if isinstance(tool_args, dict) else {},
            rule=d.pattern,
            rule_kind=rule_kind,
            executed=executed,
        )
        _write_audit(event, audit_destination)

        if decision == "BLOCKED":
            raise JamjetPolicyBlocked(tool_name, d.pattern)
        if decision == "WAITING_FOR_APPROVAL":
            raise JamjetApprovalRequired(tool_name, d.pattern)

    return guardrail
