"""Governance configuration for JamJet agents.

``GovernanceConfig`` is the single, frozen source of truth that the seam-
middleware factory (T3-2..4) and the IR compiler (T3-5) read.  It is built
once in ``Agent.__init__`` via :func:`normalize_governance` and stored as
``agent.governance``.

This module is deliberately side-effect free — it carries typed config only.
No enforcement happens here; enforcement is added in later tasks.
"""

from __future__ import annotations

from dataclasses import dataclass

# ---------------------------------------------------------------------------
# Value types
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Budget:
    """Per-run spending cap.  Either or both fields may be set.

    ``tokens``    – total token cap (input + output combined).
    ``cost_usd``  – wall-cost cap in US dollars.
    """

    tokens: int | None = None
    cost_usd: float | None = None

    def __post_init__(self) -> None:
        if self.tokens is not None and self.tokens <= 0:
            raise ValueError("Budget.tokens must be positive")
        if self.cost_usd is not None and self.cost_usd <= 0:
            raise ValueError("Budget.cost_usd must be positive")


# PolicyRef: for now any str / dict is accepted as a policy reference or
# inline spec.  T3-5 / T3-6 will type-narrow this as the DSL matures.
PolicyRef = str | dict | None

# approval_required can be True (all tools) or a list of tool-name globs.
ApprovalRequired = bool | list[str]


@dataclass(frozen=True)
class GovernanceConfig:
    """Immutable governance configuration attached to every Agent.

    Fields
    ------
    policy
        A policy reference or inline spec (str YAML path / dict IR block /
        None).  ``None`` means no explicit policy; defaults apply.
    approval_required
        ``False``  – no approval gate (default).
        ``True``   – every tool call requires approval.
        ``list``   – tool-name globs that require approval (e.g.
                     ``["delete_*", "send_*"]``).
    budget
        Optional per-run spending cap.  ``None`` when uncapped.
    pii
        Redact PII from prompts/outputs at the seam.  ON by default.
    audit
        Emit signed audit records for every governed action.  ON by default.
    receipts
        Mint AgentBoundary receipts per turn.  ON by default.
    """

    policy: PolicyRef = None
    approval_required: ApprovalRequired = False
    budget: Budget | None = None
    pii: bool = True
    audit: bool = True
    receipts: bool = True


# ---------------------------------------------------------------------------
# Normaliser
# ---------------------------------------------------------------------------


def normalize_governance(
    *,
    policy: PolicyRef = None,
    approval_required: ApprovalRequired = False,
    budget: Budget | float | int | dict | None = None,
    pii: bool = True,
    audit: bool = True,
    receipts: bool = True,
) -> GovernanceConfig:
    """Parse and validate governance kwargs into a frozen :class:`GovernanceConfig`.

    ``budget`` coercions
    --------------------
    * ``None``                         -> ``None`` (uncapped)
    * ``int`` or ``float``             -> ``Budget(cost_usd=value)``
    * ``Budget``                       -> returned as-is
    * ``dict`` with ``tokens``/``cost_usd`` keys -> ``Budget(**dict)``

    ``approval_required`` coercions
    --------------------------------
    * ``bool``        -> stored directly
    * ``list[str]``   -> stored directly (each entry is a tool-name glob)
    """
    resolved_budget = _parse_budget(budget)
    resolved_approval = _parse_approval_required(approval_required)

    return GovernanceConfig(
        policy=policy,
        approval_required=resolved_approval,
        budget=resolved_budget,
        pii=pii,
        audit=audit,
        receipts=receipts,
    )


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------


def _parse_budget(value: Budget | float | int | dict | None) -> Budget | None:
    if value is None:
        return None
    if isinstance(value, Budget):
        return value
    if isinstance(value, (int, float)):
        return Budget(cost_usd=float(value))
    if isinstance(value, dict):
        known_keys = {"tokens", "cost_usd"}
        unknown = set(value) - known_keys
        if unknown:
            raise ValueError(f"Unknown budget keys: {unknown!r}.  Expected subset of {known_keys!r}.")
        return Budget(
            tokens=value.get("tokens"),
            cost_usd=value.get("cost_usd"),
        )
    raise TypeError(
        f"budget must be a Budget, number, dict, or None — got {type(value).__name__!r}"
    )


def _parse_approval_required(value: ApprovalRequired) -> ApprovalRequired:
    if isinstance(value, bool):
        return value
    if isinstance(value, list):
        if not all(isinstance(item, str) for item in value):
            raise TypeError("approval_required list entries must be strings (tool-name globs)")
        return list(value)
    raise TypeError(
        f"approval_required must be bool or list[str] — got {type(value).__name__!r}"
    )
