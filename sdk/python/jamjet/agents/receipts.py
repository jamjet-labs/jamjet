"""Mint AgentBoundary Action Receipts for governed agent turns.

Receipts are ON by default (``GovernanceConfig.receipts``). A receipt is the
portable, tamper-evident proof that *this* agent took *this* action under a
policy decision -- the "Prove" pillar of governance-on-by-default.

This module reuses the **exact** mint path the opt-in :func:`jamjet.gate`
decorator uses (:func:`jamjet.gate._build_receipt` plus ``agentboundary``'s
canonical-hash helpers), so an agent-turn receipt is byte-for-byte the same
shape as a gated tool-call receipt and passes ``validate_receipt`` +
``check_conformance`` for the same reasons.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

ReceiptEmitter = Callable[[dict[str, Any]], None]


def agent_action_arguments(*, agent_name: str, model: str, prompt: str) -> dict[str, Any]:
    """The canonical ``arguments`` object a governed agent turn is hashed over.

    Exposed (and stable) so callers and tests can recompute the
    ``arguments_hash`` and prove the receipt is bound to *this* action -- the
    provenance link between the receipt and the agent run.
    """
    return {"agent": agent_name, "model": model, "prompt": prompt}


def mint_agent_receipt(
    *,
    agent_name: str,
    model: str,
    prompt: str,
    output: str,
    target_system: str = "local",
    target_environment: str = "prod",
    emitter: ReceiptEmitter | None = None,
) -> dict[str, Any]:
    """Mint a v0.1 Action Receipt for one governed agent turn.

    Reuses :func:`jamjet.gate._build_receipt` so the receipt is identical in
    shape to a ``@gate`` receipt. The turn is recorded as an ``allow`` decision
    (the agent ran to completion). Returns the receipt dict; when ``emitter`` is
    supplied the receipt is also shipped there (the dict is always returned so
    the caller can attach it to the run result regardless).
    """
    # Imported lazily to keep ``import jamjet`` light and avoid any import cycle
    # through the gate module.
    from agentboundary.hashing import compute_arguments_hash  # noqa: PLC0415

    from jamjet.gate import (  # noqa: PLC0415
        _AgentMeta,
        _build_receipt,
        _default_actor_id,
        _result_ref,
    )
    from jamjet.policies.decider import PolicyOutcome  # noqa: PLC0415

    arguments = agent_action_arguments(agent_name=agent_name, model=model, prompt=prompt)
    outcome = PolicyOutcome(
        decision="allow",
        name="jamjet.agent.governance",
        version="1",
        reason="governed agent turn (receipts on by default)",
    )
    receipt = _build_receipt(
        action_id=f"agent.{agent_name}",
        outcome=outcome,
        approval=None,
        execution="success",
        result_ref=_result_ref(output),
        bound=arguments,
        args_hash=compute_arguments_hash(arguments),
        actor_id=_default_actor_id(),
        actor_type="agent",
        actor_display_name=agent_name,
        target_system=target_system,
        target_environment=target_environment,
        meta=_AgentMeta(model=model),
    )
    if emitter is not None:
        emitter(receipt)
    return receipt


__all__ = ["ReceiptEmitter", "agent_action_arguments", "mint_agent_receipt"]
