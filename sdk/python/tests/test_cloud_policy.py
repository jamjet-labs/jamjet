"""PolicyEvaluator semantics: first-match-wins, mirrored against the
shared conformance contract at jamjet-policy/conformance/policy-decisions.yaml
and the TS `@jamjet/cloud` reference implementation.

A previous Python release (jamjet <= 0.8.3) used last-match-wins, which
disagreed with TS and with the conformance YAML. This test pins the
corrected semantics so the bug cannot regress.
"""

from __future__ import annotations

from jamjet.cloud.policy import PolicyEvaluator


def test_first_match_wins_allow_then_block() -> None:
    """`* → allow` placed before `*delete* → block` must yield ALLOW.

    Mirrors conformance case `first-match-wins` (allow-first beats a later block).
    Pre-fix Python returned BLOCKED because the later rule overrode the earlier one.
    """
    ev = PolicyEvaluator()
    ev.add("allow", "*")
    ev.add("block", "*delete*")

    decision = ev.evaluate("database.delete_x")

    assert decision.blocked is False
    assert decision.policy_kind == "allow"
    assert decision.pattern == "*"


def test_first_match_wins_block_then_allow() -> None:
    """Reverse order: `*delete* → block` first, `* → allow` second → BLOCKED."""
    ev = PolicyEvaluator()
    ev.add("block", "*delete*")
    ev.add("allow", "*")

    decision = ev.evaluate("database.delete_x")

    assert decision.blocked is True
    assert decision.policy_kind == "block"
    assert decision.pattern == "*delete*"


def test_no_rule_match_defaults_to_allow() -> None:
    ev = PolicyEvaluator()
    ev.add("block", "shell.exec")

    decision = ev.evaluate("database.read_orders")

    assert decision.blocked is False
    assert decision.policy_kind == "allow"
    assert decision.pattern is None


def test_require_approval_action() -> None:
    ev = PolicyEvaluator()
    ev.add("require_approval", "payments.*")

    decision = ev.evaluate("payments.refund")

    assert decision.blocked is False
    assert decision.policy_kind == "require_approval"
    assert decision.pattern == "payments.*"


def test_audit_action() -> None:
    ev = PolicyEvaluator()
    ev.add("audit", "*")

    decision = ev.evaluate("anything")

    assert decision.blocked is False
    assert decision.policy_kind == "audit"
    assert decision.pattern == "*"


def test_filter_tools_splits_on_blocked() -> None:
    ev = PolicyEvaluator()
    ev.add("block", "*delete*")

    tools = [
        {"function": {"name": "database.read"}},
        {"function": {"name": "database.delete"}},
    ]
    allowed, blocked = ev.filter_tools(tools)

    assert [t["function"]["name"] for t in allowed] == ["database.read"]
    assert [t["function"]["name"] for t in blocked] == ["database.delete"]
