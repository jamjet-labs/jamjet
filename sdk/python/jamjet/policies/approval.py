"""Approval channels for :func:`jamjet.gate`.

When a policy returns ``require-approval`` or ``escalate``, the decorator
delegates to an :class:`ApprovalChannel` to obtain a human (or programmatic)
verdict. This module ships:

- :class:`StdinApprover` — interactive y/N prompt, the default in scripts/notebooks
- :class:`NeverApprove` — always denies, useful in tests
- :func:`callable_approver` — wraps a plain callable into an approval channel

Cloud / HTTP / Discord approval channels are deferred to follow-up work; they
plug in via the same :class:`ApprovalChannel` Protocol without touching the
decorator itself.
"""

from __future__ import annotations

import getpass
import os
import sys
from dataclasses import dataclass
from typing import Any, Callable, Mapping, Protocol


@dataclass(frozen=True)
class ApprovalDecision:
    """The verdict produced by an :class:`ApprovalChannel`.

    Maps directly to the AgentBoundary v0.1 ``approval`` block of the receipt.
    ``approver_id`` should already be the redacted/published identifier;
    callers MUST NOT pass a raw secret or PII here.
    """

    approved: bool
    approver_id: str
    approver_display_name: str | None = None
    approver_role: str | None = None
    context: str | None = None


class ApprovalChannel(Protocol):
    """Pluggable approval source. Implementations must be synchronous."""

    def request_approval(
        self,
        *,
        action_id: str,
        arguments: Mapping[str, Any],
        arguments_hash: str,
        reason: str,
    ) -> ApprovalDecision:
        """Return the approval decision for a pending action.

        Implementations may block indefinitely or impose their own timeout;
        the caller treats the return value as authoritative.
        """


@dataclass
class StdinApprover:
    """Interactive y/N approver. Default when ``require_approval`` is ``True``.

    Prints a one-line summary of the proposed action to stderr and reads a line
    from stdin. Anything starting with ``y`` (case-insensitive) is treated as
    approval. The terminal user becomes the ``approver_id`` — handy for solo
    dev workflows; not appropriate in CI.
    """

    prompt_stream: Any = sys.stderr
    input_stream: Any | None = None  # if None, uses :mod:`builtins.input`

    def request_approval(
        self,
        *,
        action_id: str,
        arguments: Mapping[str, Any],
        arguments_hash: str,
        reason: str,
    ) -> ApprovalDecision:
        approver = _local_approver_id()
        msg = (
            f"\n[jamjet.gate] approval requested\n"
            f"  action:  {action_id}\n"
            f"  args:    {dict(arguments)!r}\n"
            f"  hash:    {arguments_hash}\n"
            f"  reason:  {reason or '<none>'}\n"
            f"  Approve? [y/N] "
        )
        print(msg, end="", file=self.prompt_stream, flush=True)
        if self.input_stream is None:
            line = input()
        else:
            line = self.input_stream.readline().rstrip("\n")
        approved = line.strip().lower().startswith("y")
        return ApprovalDecision(
            approved=approved,
            approver_id=approver,
            approver_display_name=approver,
            approver_role="local-operator",
            context="stdin approval",
        )


@dataclass(frozen=True)
class NeverApprove:
    """An approval channel that always denies. Useful in tests."""

    approver_id: str = "system:never-approve"

    def request_approval(
        self,
        *,
        action_id: str,
        arguments: Mapping[str, Any],
        arguments_hash: str,
        reason: str,
    ) -> ApprovalDecision:
        return ApprovalDecision(
            approved=False,
            approver_id=self.approver_id,
            context="programmatic deny",
        )


def callable_approver(
    fn: Callable[[Mapping[str, Any]], bool], *, approver_id: str
) -> ApprovalChannel:
    """Wrap a plain function into an :class:`ApprovalChannel`.

    Convenient for tests and for custom approval logic that doesn't need the
    full Protocol surface::

        ch = callable_approver(lambda args: args["amount"] < 1_000, approver_id="ci:robot")
    """

    @dataclass(frozen=True)
    class _CallableApprover:
        _fn: Callable[[Mapping[str, Any]], bool]
        _aid: str

        def request_approval(
            self,
            *,
            action_id: str,
            arguments: Mapping[str, Any],
            arguments_hash: str,
            reason: str,
        ) -> ApprovalDecision:
            return ApprovalDecision(
                approved=bool(self._fn(arguments)),
                approver_id=self._aid,
                context="callable_approver",
            )

    return _CallableApprover(fn, approver_id)


def _local_approver_id() -> str:
    """Best-effort identifier for the local terminal user."""
    try:
        u = getpass.getuser()
    except Exception:
        u = os.environ.get("USER") or "unknown"
    return f"local:{u}"
