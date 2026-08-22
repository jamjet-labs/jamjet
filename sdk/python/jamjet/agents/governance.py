"""Governance configuration for JamJet agents.

``GovernanceConfig`` is the single, frozen source of truth that the seam-
middleware factory (T3-2..4) and the IR compiler (T3-5) read.  It is built
once in ``Agent.__init__`` via :func:`normalize_governance` and stored as
``agent.governance``.

This module is deliberately side-effect free — it carries typed config only.
No enforcement happens here; enforcement is added in later tasks.
"""

from __future__ import annotations

import dataclasses
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


class _Unset:
    """Sentinel for "this governance knob was not passed".

    A plain default cannot express it. `pii` defaults to `True`, so an agent that
    explicitly asks for `pii=True` is indistinguishable from one that said nothing
    — and `Team` used exactly that comparison to decide whether a sub-agent had
    opted out of inheriting the team default. An explicit choice that happens to
    equal the default was therefore silently overridden, including being turned
    OFF by a team default of `pii=False`.
    """

    _instance = None

    def __new__(cls) -> _Unset:
        if cls._instance is None:
            cls._instance = super().__new__(cls)
        return cls._instance

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return "<unset>"


UNSET = _Unset()


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
        Enforced engine-side on the DURABLE path (``agent.run_durable``) only.
        The in-process ``agent.run()`` path has no policy engine in its loop and
        CANNOT enforce a gate: it refuses with ``ApprovalNotEnforceableError``
        rather than running ungated. ``run_durable()`` is the enforceable path,
        where the engine decides before any worker receives the payload.
        The mechanism — and why it holds even against an untrusted worker — is
        documented once, at the warning site in :meth:`jamjet.Agent.run`.
    budget
        Optional per-run spending cap.  ``None`` when uncapped.
    pii
        Redact PII from outbound prompts at the model seam.  ON by default.
        Enforced on the in-process ``agent.run()`` path by the seam middleware
        (``PiiRedactionMiddleware``) and on the durable path by the model-seam
        SIDECAR (made prod-mandatory by 2e's fail-loud coverage guard).  The
        compiled ``data_policy`` IR is emitted as metadata for the audit-log
        redactor, but the native Rust model adapters do NOT perform outbound PII
        redaction without the sidecar (dev/fallback path) — see
        :mod:`jamjet.compiler.agent_ir` and F-t3-durable-data-policy.
    audit
        Emit a signed, hash-chained audit record per governed ACTION (each tool
        call + the model turn) on the in-process / SDK path (``agent.run`` /
        ``agent.run_durable``), attached to ``AgentResult.audit`` and verifiable
        with :func:`jamjet.agents.audit.verify_chain`.  ON by default; ``audit=
        False`` emits none.  Signed with ``JAMJET_AUDIT_SIGNING_KEY`` (unsigned-
        but-chained, with a loud warning, until a key is provisioned).  The
        durable engine additionally signs approval-decision events; per-node
        engine-internal audit emission is tracked as F-t3-audit-emit.
    receipts
        Mint AgentBoundary receipts per turn.  ON by default.
    """

    policy: PolicyRef = None
    approval_required: ApprovalRequired = False
    budget: Budget | None = None
    pii: bool = True
    audit: bool = True
    receipts: bool = True
    #: Names of the knobs the caller passed EXPLICITLY, whatever value they gave.
    #:
    #: Provenance, not value. `Team` inherits its default only into a sub-agent
    #: that set nothing, and "set nothing" cannot be inferred by comparing values:
    #: an agent that deliberately passed `pii=True` looks identical to one that
    #: passed nothing at all.
    #:
    #: `compare=False` so it never affects equality — two configs with the same
    #: values remain equal regardless of how they were reached — and `repr=False`
    #: to keep it out of user-facing output.
    explicit: frozenset[str] = dataclasses.field(default_factory=frozenset, compare=False, repr=False)


# ---------------------------------------------------------------------------
# Normaliser
# ---------------------------------------------------------------------------


def normalize_governance(
    *,
    policy: PolicyRef | _Unset = UNSET,
    approval_required: ApprovalRequired | _Unset = UNSET,
    budget: Budget | float | int | dict | None | _Unset = UNSET,
    pii: bool | _Unset = UNSET,
    audit: bool | _Unset = UNSET,
    receipts: bool | _Unset = UNSET,
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
    explicit = frozenset(
        name
        for name, value in (
            ("policy", policy),
            ("approval_required", approval_required),
            ("budget", budget),
            ("pii", pii),
            ("audit", audit),
            ("receipts", receipts),
        )
        if not isinstance(value, _Unset)
    )

    # Substitute the documented defaults for anything not passed. Resolved field
    # by field rather than through a dict so each keeps its own type — a
    # dict[str, object] would erase them and every call below would need a cast.
    resolved_policy: PolicyRef = None if isinstance(policy, _Unset) else policy
    resolved_approval_in: ApprovalRequired = False if isinstance(approval_required, _Unset) else approval_required
    resolved_budget_in: Budget | float | int | dict | None = None if isinstance(budget, _Unset) else budget

    return GovernanceConfig(
        policy=resolved_policy,
        approval_required=_parse_approval_required(resolved_approval_in),
        budget=_parse_budget(resolved_budget_in),
        pii=True if isinstance(pii, _Unset) else bool(pii),
        audit=True if isinstance(audit, _Unset) else bool(audit),
        receipts=True if isinstance(receipts, _Unset) else bool(receipts),
        explicit=explicit,
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
    raise TypeError(f"budget must be a Budget, number, dict, or None — got {type(value).__name__!r}")


def _parse_approval_required(value: ApprovalRequired) -> ApprovalRequired:
    if isinstance(value, bool):
        return value
    if isinstance(value, list):
        if not all(isinstance(item, str) for item in value):
            raise TypeError("approval_required list entries must be strings (tool-name globs)")
        return list(value)
    raise TypeError(f"approval_required must be bool or list[str] — got {type(value).__name__!r}")


class ApprovalNotEnforceableError(RuntimeError):
    """Raised when an approval gate is declared on a path that cannot hold a run.

    The in-process path executes tools directly, with no policy engine between
    the model and the call, so a gate can be neither evaluated nor held there.
    Raising is the fail-closed answer: an approval gate that silently does not
    exist is worse than a run that does not start.
    """


def require_enforceable_approval(governance: object | None, *, where: str) -> None:
    """Refuse if *governance* declares an approval gate this path cannot honour.

    Keyed on the RESOLVED policy rather than on ``approval_required`` alone,
    because the same control has two spellings: ``approval_required=[...]`` and
    ``policy={"require_approval_for": [...]}``. Checking only the first left the
    second running ungated — the same half-fix this function exists to prevent.

    Called at the executor chokepoint, so it also covers callers who reach
    ``LocalRuntime.execute(..., governance=...)`` directly instead of going
    through ``Agent.run()``.
    """
    if governance is None:
        return
    from jamjet.compiler.agent_ir import effective_policy

    policy = effective_policy(governance)  # type: ignore[arg-type]
    if not (policy or {}).get("require_approval_for"):
        return
    raise ApprovalNotEnforceableError(
        f"{where}: an approval gate is declared (require_approval_for="
        f"{(policy or {}).get('require_approval_for')!r}), but the in-process path "
        "cannot hold a run at a gate. Use run_durable(), where the engine evaluates "
        "every tool call against policy before any worker receives the payload. To "
        "run without gates deliberately, remove the approval rules."
    )
