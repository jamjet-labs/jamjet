"""Tests for jamjet.integrations.openai_guardrail."""

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path

import pytest

from jamjet.integrations.openai_guardrail import (
    JamjetApprovalRequired,
    JamjetPolicyBlocked,
    jamjet_guardrail,
)


def _write_policy(tmp_path: Path, body: str) -> Path:
    p = tmp_path / "policy.yaml"
    p.write_text(body)
    return p


def test_block_destructive_tool(tmp_path: Path) -> None:
    policy = _write_policy(tmp_path, 'version: 1\nrules:\n  - { match: "*delete*", action: block }\n')
    audit_dir = tmp_path / "audit"
    guard = jamjet_guardrail(policy=str(policy), audit_destination=str(audit_dir))
    with pytest.raises(JamjetPolicyBlocked, match=r"\*delete\*"):
        guard({"toolName": "database.delete_all", "toolArgs": {}})


def test_require_approval(tmp_path: Path) -> None:
    policy = _write_policy(
        tmp_path, 'version: 1\nrules:\n  - { match: "payments.*", action: require_approval }\n'
    )
    audit_dir = tmp_path / "audit"
    guard = jamjet_guardrail(policy=str(policy), audit_destination=str(audit_dir))
    with pytest.raises(JamjetApprovalRequired):
        guard({"toolName": "payments.refund", "toolArgs": {"amount": 100}})


def test_allow_passes_through(tmp_path: Path) -> None:
    policy = _write_policy(tmp_path, 'version: 1\nrules:\n  - { match: "*delete*", action: block }\n')
    audit_dir = tmp_path / "audit"
    guard = jamjet_guardrail(policy=str(policy), audit_destination=str(audit_dir))
    guard({"toolName": "database.read_orders", "toolArgs": {}})  # no exception


def test_audit_event_written_v1_schema(tmp_path: Path) -> None:
    policy = _write_policy(tmp_path, 'version: 1\nrules:\n  - { match: "*delete*", action: block }\n')
    audit_dir = tmp_path / "audit"
    guard = jamjet_guardrail(policy=str(policy), audit_destination=str(audit_dir))
    try:
        guard({"toolName": "database.delete_all", "toolArgs": {"reason": "cleanup"}})
    except JamjetPolicyBlocked:
        pass
    today = datetime.now(UTC).isoformat()[:10]
    path = audit_dir / today / "openai-guardrail.jsonl"
    assert path.exists()
    lines = [line for line in path.read_text().splitlines() if line]
    event = json.loads(lines[-1])
    assert event["adapter"] == "openai-guardrail"
    assert event["host"] == "openai-agents-sdk"
    assert event["schema_version"] == 1
    assert event["decision"] == "BLOCKED"
    assert event["rule"] == "*delete*"
    assert event["rule_kind"] == "block"
    assert event["executed"] is False
