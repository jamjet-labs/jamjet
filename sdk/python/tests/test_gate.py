"""Truth-table + receipt-shape tests for ``@jamjet.gate``.

Locks the contract:
  * Auto-allow path emits a spec-compliant success receipt.
  * Deny path raises :class:`PolicyDeniedError` AND emits a blocked receipt.
  * Require-approval path round-trips through the approval channel.
  * Hash machinery binds the *exact* arguments observed.
  * Async functions get the same treatment.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any

import pytest
from agentboundary.hashing import compute_arguments_hash, compute_receipt_hash
from agentboundary.validator import validate_receipt

from jamjet import gate
from jamjet.gate import PolicyDeniedError
from jamjet.policies.approval import NeverApprove, callable_approver
from jamjet.policies.decider import static_decider


class CaptureEmitter:
    """Test emitter that stashes every emission for assertion."""

    def __init__(self) -> None:
        self.receipts: list[dict[str, Any]] = []

    def __call__(self, receipt: dict[str, Any]) -> None:
        self.receipts.append(receipt)


def _validate(receipt: dict[str, Any]) -> None:
    errs = validate_receipt(receipt)
    assert errs == [], f"receipt failed v0.1 schema validation: {errs}"


def test_auto_allow_emits_success_receipt_and_returns_value() -> None:
    emit = CaptureEmitter()

    @gate("github.merge", emitter=emit)
    def merge(repo: str, pr: int) -> str:
        return f"sha:{repo}#{pr}"

    out = merge("jamjet-labs/jamjet", 412)

    assert out == "sha:jamjet-labs/jamjet#412"
    assert len(emit.receipts) == 1
    r = emit.receipts[0]
    _validate(r)
    assert r["version"] == "agentboundary/v0.1"
    assert r["policy"]["decision"] == "allow"
    assert r["execution"]["status"] == "success"
    assert r["execution"]["result_ref"] == "sha:jamjet-labs/jamjet#412"
    assert r["tool"]["capability"] == "github.merge"


def test_receipt_hash_canonical() -> None:
    emit = CaptureEmitter()

    @gate("stripe.refund", emitter=emit)
    def refund(charge_id: str, amount: int) -> str:
        return f"re_{charge_id}_{amount}"

    refund("ch_42", 1500)
    r = emit.receipts[0]
    assert r["receipt_hash"] == compute_receipt_hash(r)


def test_arguments_hash_binds_exact_inputs() -> None:
    emit = CaptureEmitter()

    @gate("stripe.refund", emitter=emit)
    def refund(charge_id: str, amount: int) -> str:
        return charge_id

    refund("ch_42", 1500)
    r = emit.receipts[0]
    expected = compute_arguments_hash({"charge_id": "ch_42", "amount": 1500})
    assert r["arguments_hash"] == expected


def test_arguments_hash_changes_when_args_mutate() -> None:
    emit = CaptureEmitter()

    @gate("stripe.refund", emitter=emit)
    def refund(charge_id: str, amount: int) -> str:
        return charge_id

    refund("ch_42", 1500)
    refund("ch_42", 2000)
    assert emit.receipts[0]["arguments_hash"] != emit.receipts[1]["arguments_hash"]


def test_deny_decision_raises_and_emits_blocked() -> None:
    emit = CaptureEmitter()

    @gate(
        "stripe.refund",
        emitter=emit,
        decider=static_decider("deny", reason="no allowlist match"),
    )
    def refund(charge_id: str) -> str:
        raise AssertionError("must not be invoked on deny")

    with pytest.raises(PolicyDeniedError):
        refund("ch_42")

    assert len(emit.receipts) == 1
    r = emit.receipts[0]
    _validate(r)
    assert r["policy"]["decision"] == "deny"
    assert r["execution"]["status"] == "blocked"
    assert "result_ref" not in r["execution"]


def test_require_approval_yes_executes_and_records_approval() -> None:
    emit = CaptureEmitter()
    approver = callable_approver(lambda _args: True, approver_id="ci:robot")

    @gate(
        "github.merge",
        require_approval=True,
        approval=approver,
        emitter=emit,
    )
    def merge(repo: str, pr: int) -> str:
        return f"sha:{pr}"

    out = merge("jamjet-labs/jamjet", 7)

    assert out == "sha:7"
    r = emit.receipts[0]
    _validate(r)
    assert r["policy"]["decision"] == "require-approval"
    assert r["execution"]["status"] == "success"
    assert r["approval"]["approver"]["id"] == "ci:robot"


def test_require_approval_no_blocks_and_emits_block_receipt() -> None:
    emit = CaptureEmitter()

    @gate(
        "github.merge",
        require_approval=True,
        approval=NeverApprove(approver_id="ci:never"),
        emitter=emit,
    )
    def merge(repo: str, pr: int) -> str:
        raise AssertionError("must not be invoked when approval is denied")

    with pytest.raises(PolicyDeniedError):
        merge("jamjet-labs/jamjet", 7)

    r = emit.receipts[0]
    _validate(r)
    assert r["policy"]["decision"] == "require-approval"
    assert r["execution"]["status"] == "blocked"
    assert r["approval"]["approver"]["id"] == "ci:never"


def test_async_function_is_supported() -> None:
    emit = CaptureEmitter()

    @gate("github.merge", emitter=emit)
    async def merge(repo: str, pr: int) -> str:
        await asyncio.sleep(0)
        return f"async-sha:{pr}"

    out = asyncio.run(merge("jamjet-labs/jamjet", 9))
    assert out == "async-sha:9"
    r = emit.receipts[0]
    _validate(r)
    assert r["execution"]["status"] == "success"
    assert r["execution"]["result_ref"] == "async-sha:9"


def test_policy_denied_error_exposes_the_blocked_receipt() -> None:
    emit = CaptureEmitter()

    @gate(
        "stripe.refund",
        emitter=emit,
        decider=static_decider("deny"),
    )
    def refund(amount: int) -> str:
        return str(amount)

    with pytest.raises(PolicyDeniedError) as excinfo:
        refund(50)

    # The receipt attached to the exception should equal the one we emitted.
    assert excinfo.value.receipt == emit.receipts[0]


def test_target_overrides_propagate_to_receipt() -> None:
    emit = CaptureEmitter()

    @gate(
        "github.merge",
        emitter=emit,
        target_system="github.com/jamjet-labs/agentboundary",
        target_environment="prod",
    )
    def merge(repo: str, pr: int) -> str:
        return "ok"

    merge("jamjet-labs/agentboundary", 1)
    r = emit.receipts[0]
    assert r["target"]["system"] == "github.com/jamjet-labs/agentboundary"
    assert r["target"]["environment"] == "prod"


def test_stderr_default_emitter_runs_without_crashing(capsys: pytest.CaptureFixture[str]) -> None:
    @gate("github.merge")
    def merge(repo: str, pr: int) -> str:
        return "ok"

    merge("jamjet-labs/jamjet", 1)
    captured = capsys.readouterr()
    # Default emitter writes JSON to stderr — confirm something landed and parses
    line = captured.err.strip().splitlines()[-1]
    parsed = json.loads(line)
    assert parsed["version"] == "agentboundary/v0.1"
    assert parsed["receipt_hash"] == compute_receipt_hash(parsed)
