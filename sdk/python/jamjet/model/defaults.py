"""Default model-seam middleware chain. ONE wiring point for Track 3."""

from __future__ import annotations

from typing import TYPE_CHECKING

from jamjet.model.budget import BudgetMiddleware
from jamjet.model.metering import MeteringMiddleware
from jamjet.model.middleware import ModelAllowlistMiddleware, ModelMiddleware
from jamjet.model.pii import PiiRedactionMiddleware
from jamjet.model.policy_resolver import resolve_policy_allowlist

if TYPE_CHECKING:
    from jamjet.agents.governance import GovernanceConfig


def default_model_middleware(
    governance: GovernanceConfig | None = None,
) -> list[ModelMiddleware]:
    """The default seam middleware chain.

    Track 1 default: allow-all allowlist + no-budget + metering.
    Track 3 (T3-2): passing ``governance`` enables budget enforcement.
    Track 3 (T3-3): PII redaction is ON by default; ``pii=False`` omits it.

    Chain order
    -----------
    1. ``ModelAllowlistMiddleware`` -- deny disallowed models.  T3-6 derives the
       allowlist from ``governance.policy``: ``None`` -> allow-all; dict with
       ``model_allowlist`` -> use that set; string -> resolved from the built-in
       named-policy table (unknown string raises ``ValueError``, never silently
       allows).
    2. ``PiiRedactionMiddleware`` -- redact PII from outbound prompt messages
       BEFORE the provider or any other middleware sees them; fail-closed
       (redact-or-deny).  Omitted when ``governance.pii`` is ``False``.
    3. ``BudgetMiddleware`` -- fail-closed per-run budget enforcement; no-op
       when ``governance`` is ``None`` or ``governance.budget`` is ``None``.
    4. ``MeteringMiddleware`` -- passive cost/token recorder; always active.

    PII is DEFAULT ON
    -----------------
    When ``governance`` is ``None`` (no explicit governance config), PII
    redaction is ENABLED (the safe default).  Pass
    ``GovernanceConfig(pii=False)`` or ``normalize_governance(pii=False)``
    to disable it explicitly and receive unredacted prompts at the provider.

    Threading the budget
    --------------------
    Pass ``agent.governance`` (set by T3-1 in ``Agent.__init__``) so that
    every seam call site inherits budget enforcement from one place:

    * ``Agent.stream()`` passes ``self.governance`` here.
    * ``SeamAdapter`` (in-process via ``LocalRuntime``) creates a fresh
      instance per run via ``default_model_middleware()``.  For the
      ``Agent.run()`` path, governance threading through ``LocalRuntime`` is a
      T3-7 follow-up; the sidecar path delegates to the Rust-side budget (T3-5).

    Each call to ``default_model_middleware()`` returns FRESH instances so
    middleware state (budget accumulator, metering records) never leaks between
    runs.
    """
    budget = governance.budget if governance is not None else None
    pii_on = governance.pii if governance is not None else True
    policy = governance.policy if governance is not None else None

    # T3-6: resolve the policy to a model allowlist.
    # - None              -> allow-all (ModelAllowlistMiddleware(None))
    # - dict w/ allowlist -> use that set
    # - known str         -> built-in named policy's allowlist
    # - unknown str       -> ValueError raised here (fail loud, never silent-allow)
    allowlist = resolve_policy_allowlist(policy)

    chain: list[ModelMiddleware] = [ModelAllowlistMiddleware(allowlist)]
    if pii_on:
        chain.append(PiiRedactionMiddleware())
    chain.append(BudgetMiddleware(budget))
    chain.append(MeteringMiddleware())
    return chain
