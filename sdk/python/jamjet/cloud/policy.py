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
        """Check a tool name against all rules. Last matching rule wins."""
        with self._lock:
            rules = list(self._rules)
        matched_action: str | None = None
        matched_pattern: str | None = None
        for action, pattern in rules:
            if fnmatch.fnmatch(tool_name, pattern):
                matched_action = action
                matched_pattern = pattern
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
