from __future__ import annotations

import fnmatch
import threading
from typing import Any

from .models import PolicyDecision


class PolicyEvaluator:
    """Evaluate tool names against glob-based allow/block/require_approval rules."""

    def __init__(self) -> None:
        self._rules: list[tuple[str, str]] = []  # (action, pattern)
        self._lock = threading.Lock()

    def add(self, action: str, pattern: str) -> None:
        """Add a policy rule. action is 'block', 'allow', or 'require_approval'."""
        with self._lock:
            self._rules.append((action, pattern))

    def evaluate(self, tool_name: str) -> PolicyDecision:
        """Check a tool name against all rules. First matching rule wins.

        Matches the conformance suite at jamjet-policy/conformance/policy-decisions.yaml
        and the TS `@jamjet/cloud` evaluator: rules are evaluated top-to-bottom and
        the first matching rule is returned. Later rules cannot override an earlier
        match.
        """
        with self._lock:
            rules = list(self._rules)
        matched_action: str | None = None
        matched_pattern: str | None = None
        for action, pattern in rules:
            if fnmatch.fnmatch(tool_name, pattern):
                matched_action = action
                matched_pattern = pattern
                break
        if matched_action is None:
            return PolicyDecision(blocked=False, policy_kind="allow", pattern=None, tool_name=tool_name)
        if matched_action == "block":
            return PolicyDecision(
                blocked=True,
                policy_kind="block",
                pattern=matched_pattern,
                tool_name=tool_name,
            )
        # require_approval or allow
        return PolicyDecision(
            blocked=False,
            policy_kind=matched_action,
            pattern=matched_pattern,
            tool_name=tool_name,
        )

    def filter_tools(self, tools: list[dict[str, Any]]) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
        """Split an OpenAI-format tool list into (allowed, blocked).

        Each tool dict is expected to have a ``function`` key with a ``name`` field.
        """
        allowed: list[dict[str, Any]] = []
        blocked: list[dict[str, Any]] = []
        for tool in tools:
            name = tool.get("function", {}).get("name", "")
            decision = self.evaluate(name)
            if decision.blocked:
                blocked.append(tool)
            else:
                allowed.append(tool)
        return allowed, blocked


# ---------------------------------------------------------------------------
# Policy schema validation
# ---------------------------------------------------------------------------

_PII_TYPES = {"EMAIL", "US_SSN", "CREDIT_CARD", "PHONE_NUMBER", "IBAN_CODE", "IP_ADDRESS"}
_REDACT_ON_DETECT = {"block", "replace"}
_REDACT_SCOPE = {"messages", "tools"}


def _validate_redact_rule(rule: dict) -> None:
    types = rule.get("types")
    if not types or not isinstance(types, list):
        raise ValueError(f"redact rule requires non-empty `types` list (rule: {rule!r})")
    for t in types:
        if t not in _PII_TYPES:
            raise ValueError(f"redact rule has unknown PII type: {t!r} (allowed: {sorted(_PII_TYPES)})")
    on_detect = rule.get("on_detect", "block")
    if on_detect not in _REDACT_ON_DETECT:
        raise ValueError(f"redact rule `on_detect` must be one of {sorted(_REDACT_ON_DETECT)}, got {on_detect!r}")
    scope = rule.get("scope", ["messages", "tools"])
    if not isinstance(scope, list) or any(s not in _REDACT_SCOPE for s in scope):
        raise ValueError(f"redact rule `scope` items must be in {sorted(_REDACT_SCOPE)}, got {scope!r}")


def validate(parsed: dict) -> dict:
    """Validate a parsed policy dict against the JamJet policy schema.

    Raises ``ValueError`` for any structural or semantic violation.
    Returns the original ``parsed`` dict unchanged on success (for chaining).

    Recognised actions: ``block``, ``allow``, ``require_approval``, ``audit``,
    ``redact``, ``cache`` (Phase 2 reserved), ``fallback`` (Phase 3 reserved).
    """
    if not isinstance(parsed, dict) or parsed.get("version") != 1:
        version = parsed.get("version") if isinstance(parsed, dict) else "n/a"
        raise ValueError(f"unsupported policy version: {version}")
    rules = parsed.get("rules") or []
    for i, rule in enumerate(rules):
        if not isinstance(rule, dict) or "match" not in rule or "action" not in rule:
            raise ValueError(f"rule[{i}] missing match/action")
        action = rule["action"]
        if action == "redact":
            _validate_redact_rule(rule)
        elif action in {"cache", "fallback"}:
            pass  # Phases 2/3 — accepted by schema, instantiated only when those phases ship
        elif action not in {"block", "allow", "require_approval", "audit"}:
            raise ValueError(f"unknown rule action: {action!r}")
    return parsed


# ---------------------------------------------------------------------------
# Module-level singleton
# ---------------------------------------------------------------------------

_evaluator: PolicyEvaluator | None = None
_module_lock = threading.Lock()


def get_evaluator() -> PolicyEvaluator:
    """Return the global policy evaluator, creating one if needed."""
    global _evaluator
    if _evaluator is None:
        with _module_lock:
            if _evaluator is None:
                _evaluator = PolicyEvaluator()
    return _evaluator
