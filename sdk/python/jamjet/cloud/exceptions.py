# Exception class names follow the JamJet* prefix convention rather than the
# *Error suffix convention. These names are part of the public SDK surface; the
# convention is established and renaming would be a breaking API change.
# ruff: noqa: N818

from __future__ import annotations


class JamJetBudgetExceeded(Exception):
    """Raised when a call would exceed the configured cost budget."""

    def __init__(self, spent: float, limit: float) -> None:
        self.spent = spent
        self.limit = limit
        super().__init__(f"Budget exceeded: spent ${spent:.4f} of ${limit:.4f} limit")


class JamJetPolicyBlocked(Exception):
    """Raised when a tool call is blocked by a policy rule."""

    def __init__(self, tool: str, pattern: str) -> None:
        self.tool = tool
        self.pattern = pattern
        super().__init__(f"Tool '{tool}' blocked by policy pattern '{pattern}'")


class JamJetUnenforceableCall(JamJetPolicyBlocked):
    """Raised for an SDK method this package does not enforce (for example the
    OpenAI Responses API or Anthropic ``messages.stream()``) while a block or
    require_approval rule is active. Subclasses JamJetPolicyBlocked so existing
    handlers still catch it."""

    def __init__(self, method: str) -> None:
        super().__init__(tool=method, pattern="*")
        self.method = method
        self.args = (
            f"{method} is not policy-enforced by jamjet.cloud, so it is refused while block or "
            "require_approval rules are active. Use chat.completions.create or messages.create instead.",
        )

    def __str__(self) -> str:
        return str(self.args[0])


class JamJetPIIBlocked(JamJetPolicyBlocked):
    """Raised when a pre-LLM call is blocked because the request contained PII.

    Subclasses JamJetPolicyBlocked so existing `except JamJetPolicyBlocked`
    handlers still catch this case. Adds sanitized evidence: which rule pattern
    matched + which PII types were detected (TYPES + COUNT only — the raw PII
    values are NEVER carried on the exception object)."""

    def __init__(self, rule_pattern: str, types_detected: list[str]) -> None:
        # Initialize the parent with synthetic tool="llm-call" so the existing
        # exception message format still works.
        super().__init__(tool="llm-call", pattern=rule_pattern)
        self.rule_pattern = rule_pattern
        self.types_detected = list(types_detected)


class JamJetApprovalRejected(Exception):
    """Raised when a human-in-the-loop approval is rejected."""

    def __init__(self, approval_id: str, reason: str | None = None) -> None:
        self.approval_id = approval_id
        self.reason = reason
        msg = f"Approval {approval_id} rejected"
        if reason:
            msg += f": {reason}"
        super().__init__(msg)


class JamJetApprovalTimeout(Exception):
    """Raised when a human-in-the-loop approval times out, or when Cloud could
    not be reached to create or poll it (``reason`` says which)."""

    def __init__(self, approval_id: str, timeout_seconds: float, reason: str | None = None) -> None:
        self.approval_id = approval_id
        self.timeout_seconds = timeout_seconds
        self.reason = reason
        msg = f"Approval {approval_id} timed out after {timeout_seconds}s"
        if reason:
            msg = f"Approval {approval_id or '(not created)'} failed: {reason}"
        super().__init__(msg)
