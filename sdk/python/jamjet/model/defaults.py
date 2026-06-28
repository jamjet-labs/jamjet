"""Default model-seam middleware chain. ONE wiring point for Track 3."""

from __future__ import annotations

from typing import TYPE_CHECKING

from jamjet.model.budget import BudgetMiddleware
from jamjet.model.metering import MeteringMiddleware
from jamjet.model.middleware import ModelAllowlistMiddleware, ModelMiddleware

if TYPE_CHECKING:
    from jamjet.agents.governance import GovernanceConfig


def default_model_middleware(
    governance: GovernanceConfig | None = None,
) -> list[ModelMiddleware]:
    """The default seam middleware chain.

    Track 1 default: allow-all allowlist + no-budget + metering.
    Track 3 (T3-2): passing ``governance`` enables budget enforcement.

    Chain order
    -----------
    1. ``ModelAllowlistMiddleware`` — deny disallowed models (allow-all when
       ``allowed=None``; Track 3 T3-6 wires the real policy-derived list).
    2. ``BudgetMiddleware`` — fail-closed per-run budget enforcement; no-op
       when ``governance`` is ``None`` or ``governance.budget`` is ``None``.
    3. ``MeteringMiddleware`` — passive cost/token recorder; always active.

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
    return [
        ModelAllowlistMiddleware(None),
        BudgetMiddleware(budget),
        MeteringMiddleware(),
    ]
