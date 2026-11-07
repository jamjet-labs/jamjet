from __future__ import annotations

import re
import threading
from typing import Any

from .models import PolicyDecision


def _field(obj: Any, key: str) -> Any:
    """Read ``key`` from a dict or an SDK object (pydantic model) alike."""
    if obj is None:
        return None
    if isinstance(obj, dict):
        return obj.get(key)
    return getattr(obj, key, None)


def tool_name_of(tool: Any) -> str:
    """Name of a tool definition, in any shape the patchers see.

    Covers OpenAI function tools ``{"type": "function", "function": {"name"}}``,
    OpenAI custom tools ``{"type": "custom", "custom": {"name"}}`` and Anthropic
    tools ``{"name", "input_schema"}`` (client and server tools). Also reads a
    forced ``tool_choice`` in either vendor's shape. Returns ``""`` when no
    name is present.
    """
    for key in ("function", "custom"):
        inner = _field(tool, key)
        if inner is not None:
            name = _field(inner, "name")
            if isinstance(name, str):
                return name
    name = _field(tool, "name")
    return name if isinstance(name, str) else ""


def glob_to_regex(pattern: str) -> re.Pattern[str]:
    """Compile a tool-name glob, per sdk/conformance/README.md.

    ``*`` is any run of characters (line breaks included), ``?`` is one code
    point, everything else is literal, and matching is case-sensitive on every
    platform. ``fnmatch`` is not used: it reads ``[a-z]`` as a character class
    and folds case on Windows, which the TypeScript SDK does not.
    """
    parts = [".*" if ch == "*" else "." if ch == "?" else re.escape(ch) for ch in pattern]
    return re.compile("".join(parts), re.DOTALL)


class PolicyEvaluator:
    """Evaluate tool names against glob-based allow/block/require_approval rules."""

    def __init__(self) -> None:
        self._rules: list[tuple[str, str, re.Pattern[str]]] = []  # (action, pattern, compiled)
        self._lock = threading.Lock()

    def add(self, action: str, pattern: str) -> None:
        """Add a policy rule. action is 'block', 'allow', 'require_approval' or 'audit'."""
        compiled = glob_to_regex(pattern)
        with self._lock:
            self._rules.append((action, pattern, compiled))

    def evaluate(self, tool_name: str) -> PolicyDecision:
        """Check a tool name against all rules. First matching rule wins.

        Matches the conformance suite (sdk/conformance/policy-decisions.yaml,
        vendored from jamjet-policy) and the TS `@jamjet/cloud` evaluator: rules
        are evaluated top-to-bottom and the first matching rule is returned.
        Later rules cannot override an earlier match. The name is trimmed, as
        Cloud trims the tool name it evaluates.
        """
        tool_name = tool_name.strip()
        with self._lock:
            rules = list(self._rules)
        for action, pattern, compiled in rules:
            if compiled.fullmatch(tool_name):
                return PolicyDecision(
                    blocked=action == "block",
                    policy_kind=action,
                    pattern=pattern,
                    tool_name=tool_name,
                )
        return PolicyDecision(blocked=False, policy_kind="allow", pattern=None, tool_name=tool_name)

    def has_rules(self) -> bool:
        """True once any rule has been added."""
        with self._lock:
            return bool(self._rules)

    def has_restrictive_rules(self) -> bool:
        """True when a block or require_approval rule exists."""
        with self._lock:
            return any(action in ("block", "require_approval") for action, _, _ in self._rules)

    def filter_tools(self, tools: list[Any]) -> tuple[list[Any], list[Any]]:
        """Split a tool list into (allowed, blocked).

        Accepts OpenAI and Anthropic tool shapes; see :func:`tool_name_of`.
        """
        allowed: list[Any] = []
        blocked: list[Any] = []
        for tool in tools:
            decision = self.evaluate(tool_name_of(tool))
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
