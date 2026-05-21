"""``@gate`` — the smallest adoption step for AgentBoundary.

Wraps a single Python callable as an *action boundary*. On every call:

1. Bind the call-site arguments via :func:`inspect.signature`.
2. Compute the canonical SHA-256 of those arguments (``arguments_hash``).
3. Ask the policy decider for a decision.
4. If ``allow``: invoke the wrapped function, emit a *success* receipt,
   return the result.
5. If ``require-approval`` / ``escalate``: delegate to the approval channel.
   On approval, execute and emit a success receipt with the ``approval``
   block populated. On denial, raise :class:`PolicyDeniedError` and emit a
   *blocked* receipt.
6. If ``deny``: do not invoke; raise :class:`PolicyDeniedError` and emit a
   *blocked* receipt.

The emitted receipt is a spec-compliant AgentBoundary v0.1 Action Receipt;
anyone can recompute the canonical SHA-256 of the body (minus
``receipt_hash``) and confirm integrity. The default emitter writes a single
JSON line to ``stderr``; programs that want to ship receipts elsewhere
supply their own callable.

Async functions are auto-detected: the wrapped coroutine awaits the policy
decision and the approval channel synchronously (offloaded via
:func:`asyncio.to_thread`) so the receipt is bound to the exact moment of
the production-action boundary, exactly as the spec requires.

Example::

    from jamjet import gate

    @gate("github.merge", require_approval=True)
    def merge_pr(repo: str, pr: int) -> str:
        return github.merge(repo, pr)

    @gate("stripe.refund")  # auto-allow + audit
    def refund(charge_id: str, amount: int) -> str:
        return stripe.refunds.create(charge=charge_id, amount=amount).id
"""

from __future__ import annotations

import asyncio
import functools
import inspect
import json
import sys
import uuid
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Any, ParamSpec, TypeVar

from agentboundary.hashing import compute_arguments_hash, compute_receipt_hash

from jamjet.policies.approval import (
    ApprovalChannel,
    ApprovalDecision,
    StdinApprover,
)
from jamjet.policies.decider import (
    PolicyDecider,
    PolicyOutcome,
    static_decider,
)

P = ParamSpec("P")
R = TypeVar("R")


class PolicyDeniedError(Exception):
    """Raised when the policy decision (or human approver) blocks the call."""

    def __init__(self, message: str, *, receipt: dict[str, Any]) -> None:
        super().__init__(message)
        self.receipt = receipt


ReceiptEmitter = Callable[[dict[str, Any]], None]
"""A receipt emitter takes the finalised receipt dict and ships it somewhere."""


def stderr_emitter(receipt: dict[str, Any]) -> None:
    """Default emitter — writes a single JSON line to stderr."""
    print(json.dumps(receipt, sort_keys=True), file=sys.stderr, flush=True)


@dataclass(frozen=True)
class _AgentMeta:
    framework: str = "jamjet"
    framework_version: str = "0.8.6"
    model: str = "unknown"


def gate(
    action_id: str,
    *,
    require_approval: bool = False,
    decider: PolicyDecider | None = None,
    approval: ApprovalChannel | None = None,
    emitter: ReceiptEmitter | None = None,
    actor_id: str | None = None,
    actor_type: str = "agent",
    actor_display_name: str | None = None,
    target_system: str = "local",
    target_environment: str = "prod",
    agent_meta: _AgentMeta | None = None,
) -> Callable[[Callable[P, R]], Callable[P, R]]:
    """Wrap a callable as an AgentBoundary v0.1 action boundary.

    :param action_id: Capability identifier, e.g. ``"github.merge"`` or
        ``"stripe.refund"``. Becomes the ``tool.capability`` field on the
        emitted receipt. Choose stable, hierarchical, ``vendor.action`` names.
    :param require_approval: When ``True`` *and* no explicit ``decider`` is
        passed, the built-in decider returns ``require-approval`` for every
        call, routing through ``approval`` (default :class:`StdinApprover`).
    :param decider: Optional policy decider. Overrides ``require_approval``.
    :param approval: Optional approval channel. Defaults to
        :class:`StdinApprover` (interactive y/N).
    :param emitter: Optional receipt emitter. Defaults to writing a single
        JSON line to stderr.
    :param actor_id: Identifier for the actor that is *about to* call the
        wrapped function. If ``None`` a stable ``agent:jamjet:<pid>`` is used.
    :param target_system: Free-form ``target.system`` value (spec §). Defaults
        to ``"local"``; production callers should set this (e.g.
        ``"github.com/jamjet-labs/jamjet"``).

    Returns a decorator. Works on both sync and ``async def`` callables.
    """

    chosen_decider: PolicyDecider = (
        decider
        if decider is not None
        else (
            static_decider("require-approval", reason="require_approval=True")
            if require_approval
            else static_decider("allow", reason="auto-allow (no policy configured)")
        )
    )
    chosen_approval: ApprovalChannel = approval if approval is not None else StdinApprover()
    chosen_emitter: ReceiptEmitter = emitter if emitter is not None else stderr_emitter
    meta = agent_meta if agent_meta is not None else _AgentMeta()
    chosen_actor_id = actor_id or _default_actor_id()

    def _wrap(func: Callable[P, R]) -> Callable[P, R]:
        sig = inspect.signature(func)

        if asyncio.iscoroutinefunction(func):

            @functools.wraps(func)
            async def _async_wrapper(*args: P.args, **kwargs: P.kwargs) -> R:
                bound = _bind_args(sig, args, kwargs)
                args_hash = compute_arguments_hash(bound)
                outcome = chosen_decider(bound)
                approval_decision: ApprovalDecision | None = None
                if outcome.decision in ("require-approval", "escalate"):
                    approval_decision = await asyncio.to_thread(
                        chosen_approval.request_approval,
                        action_id=action_id,
                        arguments=bound,
                        arguments_hash=args_hash,
                        reason=outcome.reason,
                    )
                if _is_blocked(outcome, approval_decision):
                    receipt = _build_receipt(
                        action_id=action_id,
                        outcome=outcome,
                        approval=approval_decision,
                        execution=_blocked_execution(outcome, approval_decision),
                        result_ref=None,
                        bound=bound,
                        args_hash=args_hash,
                        actor_id=chosen_actor_id,
                        actor_type=actor_type,
                        actor_display_name=actor_display_name,
                        target_system=target_system,
                        target_environment=target_environment,
                        meta=meta,
                    )
                    chosen_emitter(receipt)
                    raise PolicyDeniedError(
                        _block_message(outcome, approval_decision),
                        receipt=receipt,
                    )
                result = await func(*args, **kwargs)
                receipt = _build_receipt(
                    action_id=action_id,
                    outcome=outcome,
                    approval=approval_decision,
                    execution="success",
                    result_ref=_result_ref(result),
                    bound=bound,
                    args_hash=args_hash,
                    actor_id=chosen_actor_id,
                    actor_type=actor_type,
                    actor_display_name=actor_display_name,
                    target_system=target_system,
                    target_environment=target_environment,
                    meta=meta,
                )
                chosen_emitter(receipt)
                return result

            return _async_wrapper  # type: ignore[return-value]

        @functools.wraps(func)
        def _sync_wrapper(*args: P.args, **kwargs: P.kwargs) -> R:
            bound = _bind_args(sig, args, kwargs)
            args_hash = compute_arguments_hash(bound)
            outcome = chosen_decider(bound)
            approval_decision: ApprovalDecision | None = None
            if outcome.decision in ("require-approval", "escalate"):
                approval_decision = chosen_approval.request_approval(
                    action_id=action_id,
                    arguments=bound,
                    arguments_hash=args_hash,
                    reason=outcome.reason,
                )
            if _is_blocked(outcome, approval_decision):
                receipt = _build_receipt(
                    action_id=action_id,
                    outcome=outcome,
                    approval=approval_decision,
                    execution=_blocked_execution(outcome, approval_decision),
                    result_ref=None,
                    bound=bound,
                    args_hash=args_hash,
                    actor_id=chosen_actor_id,
                    actor_type=actor_type,
                    actor_display_name=actor_display_name,
                    target_system=target_system,
                    target_environment=target_environment,
                    meta=meta,
                )
                chosen_emitter(receipt)
                raise PolicyDeniedError(
                    _block_message(outcome, approval_decision),
                    receipt=receipt,
                )
            result = func(*args, **kwargs)
            receipt = _build_receipt(
                action_id=action_id,
                outcome=outcome,
                approval=approval_decision,
                execution="success",
                result_ref=_result_ref(result),
                bound=bound,
                args_hash=args_hash,
                actor_id=chosen_actor_id,
                actor_type=actor_type,
                actor_display_name=actor_display_name,
                target_system=target_system,
                target_environment=target_environment,
                meta=meta,
            )
            chosen_emitter(receipt)
            return result

        return _sync_wrapper

    return _wrap


def _bind_args(sig: inspect.Signature, args: tuple[Any, ...], kwargs: Mapping[str, Any]) -> dict[str, Any]:
    bound = sig.bind_partial(*args, **kwargs)
    bound.apply_defaults()
    out: dict[str, Any] = {}
    for name, value in bound.arguments.items():
        out[name] = _json_safe(value)
    return out


def _json_safe(value: Any) -> Any:
    """Coerce non-JSON-native values into stable string representations.

    The arguments object MUST round-trip through canonical JSON, so opaque
    Python objects need to be representable. Strings, numbers, bools, None,
    list, dict, tuple are passed through (with tuples → lists). Anything else
    is rendered via :func:`repr` — deterministic enough for hashing.
    """
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, (list, tuple)):
        return [_json_safe(x) for x in value]
    if isinstance(value, dict):
        return {str(k): _json_safe(v) for k, v in value.items()}
    return repr(value)


def _result_ref(result: Any) -> str:
    """Render a stable, compact reference to the function's return value."""
    if result is None:
        return ""
    if isinstance(result, (str, int, float, bool)):
        return str(result)
    return repr(result)


def _is_blocked(outcome: PolicyOutcome, approval: ApprovalDecision | None) -> bool:
    if outcome.decision == "deny":
        return True
    if outcome.decision in ("require-approval", "escalate"):
        return approval is not None and not approval.approved
    return False


def _blocked_execution(outcome: PolicyOutcome, approval: ApprovalDecision | None) -> str:
    """Always returns ``"blocked"``; kept separate for clarity at call sites."""
    return "blocked"


def _block_message(outcome: PolicyOutcome, approval: ApprovalDecision | None) -> str:
    if outcome.decision == "deny":
        return f"policy denied: {outcome.reason or outcome.name}"
    if approval is not None:
        return f"approval declined by {approval.approver_id}"
    return f"blocked: {outcome.decision}"


def _default_actor_id() -> str:
    import os as _os

    return f"agent:jamjet:pid/{_os.getpid()}"


def _build_receipt(
    *,
    action_id: str,
    outcome: PolicyOutcome,
    approval: ApprovalDecision | None,
    execution: str,
    result_ref: str | None,
    bound: Mapping[str, Any],
    args_hash: str,
    actor_id: str,
    actor_type: str,
    actor_display_name: str | None,
    target_system: str,
    target_environment: str,
    meta: _AgentMeta,
) -> dict[str, Any]:
    issued_at = datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
    actor: dict[str, Any] = {"type": actor_type, "id": actor_id}
    if actor_display_name:
        actor["display_name"] = actor_display_name

    tool_name, _, _capability = action_id.partition(".")
    receipt: dict[str, Any] = {
        "version": "agentboundary/v0.1",
        "receipt_id": str(uuid.uuid4()),
        "issued_at": issued_at,
        "actor": actor,
        "agent": {
            "framework": meta.framework,
            "framework_version": meta.framework_version,
            "model": meta.model,
        },
        "tool": {"name": tool_name or "jamjet.gate", "capability": action_id},
        "target": {"system": target_system, "environment": target_environment},
        "arguments_hash": args_hash,
        "policy": {
            "name": outcome.name,
            "version": outcome.version,
            "decision": outcome.decision,
        },
        "execution": {
            "status": execution,
            "completed_at": issued_at,
        },
    }
    if execution == "success" and result_ref:
        receipt["execution"]["result_ref"] = result_ref
    if approval is not None:
        approval_block: dict[str, Any] = {
            "approver": {"id": approval.approver_id},
            "approved_at": issued_at,
        }
        if approval.approver_display_name:
            approval_block["approver"]["display_name"] = approval.approver_display_name
        if approval.approver_role:
            approval_block["approver"]["role"] = approval.approver_role
        if approval.context:
            approval_block["context"] = approval.context
        receipt["approval"] = approval_block

    receipt["receipt_hash"] = compute_receipt_hash(receipt)
    return receipt


__all__ = ["PolicyDeniedError", "ReceiptEmitter", "gate", "stderr_emitter"]
