"""
Typed failure taxonomy for delegated agent operations (B.2).

Mirrors the Rust ``DelegationFailure`` enum and ``DelegationFailureInfo``
struct in ``runtime/protocols/src/failure.rs``.  Uses the same flat
``type``-discriminated pattern as ``TaskEvent``.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any


class FailureSeverity(StrEnum):
    """Severity classification for delegation failures."""

    WARNING = "warning"
    ERROR = "error"
    FATAL = "fatal"


@dataclass
class DelegationFailure:
    """A canonical delegation failure.

    Discriminated by ``type``.  Use the factory class methods to create
    well-formed failures that match the Rust enum variants.
    """

    type: str
    # -- variant fields (populated per-type) --
    url: str | None = None
    message: str | None = None
    requested: str | None = None
    available: list[str] | None = None
    policy_id: str | None = None
    reason: str | None = None
    prompt: str | None = None
    limit: float | None = None
    actual: float | None = None
    unit: str | None = None
    from_domain: str | None = None
    to_domain: str | None = None
    check: str | None = None
    tool: str | None = None
    error: str | None = None
    completed_steps: int | None = None
    total_steps: int | None = None
    output: Any | None = None
    original_delegate: str | None = None
    fallback_delegate: str | None = None
    deadline_secs: int | None = None
    elapsed_secs: int | None = None

    # ── Variant field mappings (type -> set of field names) ──────────────
    _VARIANT_FIELDS: dict[str, tuple[str, ...]] = field(
        default=None,  # type: ignore[assignment]
        init=False,
        repr=False,
        compare=False,
    )

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "_VARIANT_FIELDS",
            {
                "delegate_unreachable": ("url", "message"),
                "capability_mismatch": ("requested", "available"),
                "policy_denied": ("policy_id", "reason"),
                "approval_required": ("prompt",),
                "budget_exceeded": ("limit", "actual", "unit"),
                "trust_crossing_denied": ("from_domain", "to_domain"),
                "verification_failed": ("check", "message"),
                "tool_dependency_failed": ("tool", "error"),
                "partial_completion": ("completed_steps", "total_steps", "output"),
                "fallback_invoked": (
                    "original_delegate",
                    "fallback_delegate",
                    "reason",
                ),
                "timeout": ("deadline_secs", "elapsed_secs"),
            },
        )

    # ── Factory class methods ───────────────────────────────────────────

    @classmethod
    def delegate_unreachable(cls, url: str, message: str) -> DelegationFailure:
        return cls(type="delegate_unreachable", url=url, message=message)

    @classmethod
    def capability_mismatch(cls, requested: str, available: list[str]) -> DelegationFailure:
        return cls(type="capability_mismatch", requested=requested, available=list(available))

    @classmethod
    def policy_denied(cls, policy_id: str, reason: str) -> DelegationFailure:
        return cls(type="policy_denied", policy_id=policy_id, reason=reason)

    @classmethod
    def approval_required(cls, prompt: str) -> DelegationFailure:
        return cls(type="approval_required", prompt=prompt)

    @classmethod
    def budget_exceeded(cls, limit: float, actual: float, unit: str) -> DelegationFailure:
        return cls(type="budget_exceeded", limit=limit, actual=actual, unit=unit)

    @classmethod
    def trust_crossing_denied(cls, from_domain: str, to_domain: str) -> DelegationFailure:
        return cls(
            type="trust_crossing_denied",
            from_domain=from_domain,
            to_domain=to_domain,
        )

    @classmethod
    def verification_failed(cls, check: str, message: str) -> DelegationFailure:
        return cls(type="verification_failed", check=check, message=message)

    @classmethod
    def tool_dependency_failed(cls, tool: str, error: str) -> DelegationFailure:
        return cls(type="tool_dependency_failed", tool=tool, error=error)

    @classmethod
    def partial_completion(
        cls,
        completed_steps: int,
        total_steps: int,
        output: Any | None = None,
    ) -> DelegationFailure:
        return cls(
            type="partial_completion",
            completed_steps=completed_steps,
            total_steps=total_steps,
            output=output,
        )

    @classmethod
    def fallback_invoked(
        cls,
        original_delegate: str,
        fallback_delegate: str,
        reason: str,
    ) -> DelegationFailure:
        return cls(
            type="fallback_invoked",
            original_delegate=original_delegate,
            fallback_delegate=fallback_delegate,
            reason=reason,
        )

    @classmethod
    def timeout(cls, deadline_secs: int, elapsed_secs: int) -> DelegationFailure:
        return cls(type="timeout", deadline_secs=deadline_secs, elapsed_secs=elapsed_secs)

    # ── Serialization ───────────────────────────────────────────────────

    def to_dict(self) -> dict[str, Any]:
        """Serialize to a dict matching the Rust JSON representation."""
        d: dict[str, Any] = {"type": self.type}
        fields = self._VARIANT_FIELDS.get(self.type, ())
        for name in fields:
            val = getattr(self, name, None)
            if val is not None:
                d[name] = val
        return d


@dataclass
class DelegationFailureInfo:
    """Enriched failure envelope with metadata.

    Mirrors the Rust ``DelegationFailureInfo`` struct.
    """

    failure: DelegationFailure
    severity: FailureSeverity
    retryable: bool
    partial_output: Any | None = None
    recommended_fallback: str | None = None
    audit_ref: str | None = None
    timestamp: str = ""

    def to_dict(self) -> dict[str, Any]:
        """Serialize to a dict matching the Rust JSON representation."""
        d: dict[str, Any] = {
            "failure": self.failure.to_dict(),
            "severity": self.severity.value,
            "retryable": self.retryable,
            "timestamp": self.timestamp,
        }
        if self.partial_output is not None:
            d["partial_output"] = self.partial_output
        if self.recommended_fallback is not None:
            d["recommended_fallback"] = self.recommended_fallback
        if self.audit_ref is not None:
            d["audit_ref"] = self.audit_ref
        return d
