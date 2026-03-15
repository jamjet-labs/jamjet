"""Tests for the typed delegation failure taxonomy (B.2)."""

from __future__ import annotations

import pytest

from jamjet.protocols.failures import (
    DelegationFailure,
    DelegationFailureInfo,
    FailureSeverity,
)

# ── Factory construction tests ──────────────────────────────────────────────


class TestDelegationFailureFactories:
    """Each factory classmethod should populate the correct type + fields."""

    def test_delegate_unreachable(self) -> None:
        f = DelegationFailure.delegate_unreachable(url="https://agent.example.com", message="connection refused")
        assert f.type == "delegate_unreachable"
        assert f.url == "https://agent.example.com"
        assert f.message == "connection refused"

    def test_capability_mismatch(self) -> None:
        f = DelegationFailure.capability_mismatch(requested="summarize", available=["translate", "transcribe"])
        assert f.type == "capability_mismatch"
        assert f.requested == "summarize"
        assert f.available == ["translate", "transcribe"]

    def test_policy_denied(self) -> None:
        f = DelegationFailure.policy_denied(policy_id="pol-42", reason="external calls disabled")
        assert f.type == "policy_denied"
        assert f.policy_id == "pol-42"
        assert f.reason == "external calls disabled"

    def test_approval_required(self) -> None:
        f = DelegationFailure.approval_required(prompt="Allow agent X to proceed?")
        assert f.type == "approval_required"
        assert f.prompt == "Allow agent X to proceed?"

    def test_budget_exceeded(self) -> None:
        f = DelegationFailure.budget_exceeded(limit=10.0, actual=15.5, unit="usd")
        assert f.type == "budget_exceeded"
        assert f.limit == 10.0
        assert f.actual == 15.5
        assert f.unit == "usd"

    def test_trust_crossing_denied(self) -> None:
        f = DelegationFailure.trust_crossing_denied(from_domain="internal.corp", to_domain="external.io")
        assert f.type == "trust_crossing_denied"
        assert f.from_domain == "internal.corp"
        assert f.to_domain == "external.io"

    def test_verification_failed(self) -> None:
        f = DelegationFailure.verification_failed(check="signature", message="invalid RSA signature")
        assert f.type == "verification_failed"
        assert f.check == "signature"
        assert f.message == "invalid RSA signature"

    def test_tool_dependency_failed(self) -> None:
        f = DelegationFailure.tool_dependency_failed(tool="web_search", error="rate limited")
        assert f.type == "tool_dependency_failed"
        assert f.tool == "web_search"
        assert f.error == "rate limited"

    def test_partial_completion(self) -> None:
        f = DelegationFailure.partial_completion(completed_steps=3, total_steps=5, output={"partial": True})
        assert f.type == "partial_completion"
        assert f.completed_steps == 3
        assert f.total_steps == 5
        assert f.output == {"partial": True}

    def test_partial_completion_no_output(self) -> None:
        f = DelegationFailure.partial_completion(completed_steps=1, total_steps=4)
        assert f.type == "partial_completion"
        assert f.output is None

    def test_fallback_invoked(self) -> None:
        f = DelegationFailure.fallback_invoked(
            original_delegate="agent-a",
            fallback_delegate="agent-b",
            reason="agent-a timed out",
        )
        assert f.type == "fallback_invoked"
        assert f.original_delegate == "agent-a"
        assert f.fallback_delegate == "agent-b"
        assert f.reason == "agent-a timed out"

    def test_timeout(self) -> None:
        f = DelegationFailure.timeout(deadline_secs=30, elapsed_secs=35)
        assert f.type == "timeout"
        assert f.deadline_secs == 30
        assert f.elapsed_secs == 35


# ── Serialization tests ─────────────────────────────────────────────────────


class TestDelegationFailureSerialization:
    """to_dict() should include the type tag and only relevant variant fields."""

    def test_delegate_unreachable_dict(self) -> None:
        f = DelegationFailure.delegate_unreachable("http://x", "down")
        d = f.to_dict()
        assert d == {"type": "delegate_unreachable", "url": "http://x", "message": "down"}

    def test_capability_mismatch_dict(self) -> None:
        f = DelegationFailure.capability_mismatch("foo", ["bar", "baz"])
        d = f.to_dict()
        assert d == {
            "type": "capability_mismatch",
            "requested": "foo",
            "available": ["bar", "baz"],
        }

    def test_policy_denied_dict(self) -> None:
        d = DelegationFailure.policy_denied("p1", "nope").to_dict()
        assert d == {"type": "policy_denied", "policy_id": "p1", "reason": "nope"}

    def test_approval_required_dict(self) -> None:
        d = DelegationFailure.approval_required("ok?").to_dict()
        assert d == {"type": "approval_required", "prompt": "ok?"}

    def test_budget_exceeded_dict(self) -> None:
        d = DelegationFailure.budget_exceeded(5.0, 7.0, "tokens").to_dict()
        assert d == {
            "type": "budget_exceeded",
            "limit": 5.0,
            "actual": 7.0,
            "unit": "tokens",
        }

    def test_trust_crossing_denied_dict(self) -> None:
        d = DelegationFailure.trust_crossing_denied("a.com", "b.com").to_dict()
        assert d == {
            "type": "trust_crossing_denied",
            "from_domain": "a.com",
            "to_domain": "b.com",
        }

    def test_verification_failed_dict(self) -> None:
        d = DelegationFailure.verification_failed("hash", "mismatch").to_dict()
        assert d == {
            "type": "verification_failed",
            "check": "hash",
            "message": "mismatch",
        }

    def test_tool_dependency_failed_dict(self) -> None:
        d = DelegationFailure.tool_dependency_failed("t", "e").to_dict()
        assert d == {"type": "tool_dependency_failed", "tool": "t", "error": "e"}

    def test_partial_completion_dict_with_output(self) -> None:
        d = DelegationFailure.partial_completion(2, 4, {"data": 1}).to_dict()
        assert d == {
            "type": "partial_completion",
            "completed_steps": 2,
            "total_steps": 4,
            "output": {"data": 1},
        }

    def test_partial_completion_dict_no_output(self) -> None:
        d = DelegationFailure.partial_completion(1, 3).to_dict()
        # output is None so should not appear
        assert d == {
            "type": "partial_completion",
            "completed_steps": 1,
            "total_steps": 3,
        }

    def test_fallback_invoked_dict(self) -> None:
        d = DelegationFailure.fallback_invoked("a", "b", "r").to_dict()
        assert d == {
            "type": "fallback_invoked",
            "original_delegate": "a",
            "fallback_delegate": "b",
            "reason": "r",
        }

    def test_timeout_dict(self) -> None:
        d = DelegationFailure.timeout(10, 12).to_dict()
        assert d == {"type": "timeout", "deadline_secs": 10, "elapsed_secs": 12}

    def test_no_extra_fields_in_dict(self) -> None:
        """to_dict() must not leak fields from other variants."""
        f = DelegationFailure.timeout(5, 6)
        d = f.to_dict()
        assert "url" not in d
        assert "tool" not in d
        assert "policy_id" not in d


# ── FailureSeverity tests ───────────────────────────────────────────────────


class TestFailureSeverity:
    """FailureSeverity should be a string enum with three values."""

    def test_values(self) -> None:
        assert FailureSeverity.WARNING.value == "warning"
        assert FailureSeverity.ERROR.value == "error"
        assert FailureSeverity.FATAL.value == "fatal"

    def test_is_str(self) -> None:
        """FailureSeverity members should be usable as plain strings."""
        assert isinstance(FailureSeverity.WARNING, str)
        assert FailureSeverity.ERROR == "error"

    def test_from_string(self) -> None:
        assert FailureSeverity("warning") is FailureSeverity.WARNING
        assert FailureSeverity("fatal") is FailureSeverity.FATAL

    def test_invalid_value_raises(self) -> None:
        with pytest.raises(ValueError):
            FailureSeverity("critical")

    def test_all_members(self) -> None:
        assert len(FailureSeverity) == 3


# ── DelegationFailureInfo tests ─────────────────────────────────────────────


class TestDelegationFailureInfo:
    """DelegationFailureInfo wraps a failure with metadata."""

    def _make_info(self, **overrides: object) -> DelegationFailureInfo:
        defaults: dict[str, object] = {
            "failure": DelegationFailure.timeout(10, 12),
            "severity": FailureSeverity.ERROR,
            "retryable": True,
            "partial_output": None,
            "recommended_fallback": None,
            "audit_ref": None,
            "timestamp": "2026-03-15T00:00:00Z",
        }
        defaults.update(overrides)
        return DelegationFailureInfo(**defaults)  # type: ignore[arg-type]

    def test_construction_all_fields(self) -> None:
        info = self._make_info(
            partial_output={"p": 1},
            recommended_fallback="agent-b",
            audit_ref="ref-99",
        )
        assert info.failure.type == "timeout"
        assert info.severity is FailureSeverity.ERROR
        assert info.retryable is True
        assert info.partial_output == {"p": 1}
        assert info.recommended_fallback == "agent-b"
        assert info.audit_ref == "ref-99"
        assert info.timestamp == "2026-03-15T00:00:00Z"

    def test_construction_minimal(self) -> None:
        info = self._make_info()
        assert info.partial_output is None
        assert info.recommended_fallback is None
        assert info.audit_ref is None

    def test_to_dict_full(self) -> None:
        info = self._make_info(
            partial_output={"x": 2},
            recommended_fallback="fb",
            audit_ref="aud",
        )
        d = info.to_dict()
        assert d["failure"] == {"type": "timeout", "deadline_secs": 10, "elapsed_secs": 12}
        assert d["severity"] == "error"
        assert d["retryable"] is True
        assert d["partial_output"] == {"x": 2}
        assert d["recommended_fallback"] == "fb"
        assert d["audit_ref"] == "aud"
        assert d["timestamp"] == "2026-03-15T00:00:00Z"

    def test_to_dict_minimal(self) -> None:
        info = self._make_info()
        d = info.to_dict()
        assert "partial_output" not in d
        assert "recommended_fallback" not in d
        assert "audit_ref" not in d

    def test_retryable_false(self) -> None:
        info = self._make_info(
            failure=DelegationFailure.policy_denied("p", "denied"),
            severity=FailureSeverity.FATAL,
            retryable=False,
        )
        assert info.retryable is False
        d = info.to_dict()
        assert d["retryable"] is False
        assert d["severity"] == "fatal"

    def test_warning_severity(self) -> None:
        info = self._make_info(severity=FailureSeverity.WARNING)
        assert info.severity is FailureSeverity.WARNING
        assert info.to_dict()["severity"] == "warning"


# ── Import from package tests ───────────────────────────────────────────────


class TestPackageExports:
    """The types should be importable from the protocols package."""

    def test_import_from_protocols(self) -> None:
        from jamjet.protocols import (
            DelegationFailure,
            DelegationFailureInfo,
            FailureSeverity,
        )

        assert DelegationFailure is not None
        assert DelegationFailureInfo is not None
        assert FailureSeverity is not None
