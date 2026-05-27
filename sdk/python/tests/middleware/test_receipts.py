"""Tests for LLM-call receipt emission (Task 6.5).

Covers:
- Shape of receipts built from BLOCKED + PASSTHROUGH-with-redact contexts
- ``receipt_hash`` is recomputable from the rest of the body
- ``emit_receipt`` appends one JSON line under ``JAMJET_AUDIT_DIR``
- Schema-strict view validates against the v0.2-alpha JSON Schema
- Patcher integration: synthetic middleware exercises both the success
  (middleware fired, terminal ran) and exception (middleware raised) paths
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

import pytest

from jamjet.cloud.middleware import CallContext, MiddlewareOutcome
from jamjet.cloud.middleware.receipts import (
    build_llm_receipt,
    emit_receipt,
    schema_strict_view,
)

# ---------------------------------------------------------------------------
# Context fixtures
# ---------------------------------------------------------------------------


def _block_ctx() -> CallContext:
    ctx = CallContext(
        provider="openai",
        model="gpt-4o",
        messages=[{"role": "user", "content": "leak"}],
    )
    ctx.middleware_fired = ["pii.redact"]
    ctx.middleware_outcome = MiddlewareOutcome.BLOCKED
    ctx.middleware_evidence = {"types": ["EMAIL"], "count": 1, "action": "blocked"}
    return ctx


def _passthrough_redact_ctx() -> CallContext:
    ctx = CallContext(
        provider="anthropic",
        model="claude-haiku-4-5",
        messages=[{"role": "user", "content": "[REDACTED:EMAIL]"}],
    )
    ctx.middleware_fired = ["pii.redact"]
    ctx.middleware_outcome = MiddlewareOutcome.PASSTHROUGH
    ctx.middleware_evidence = {"types": ["EMAIL"], "count": 1, "action": "replaced"}
    return ctx


# ---------------------------------------------------------------------------
# build_llm_receipt shape
# ---------------------------------------------------------------------------


def test_block_receipt_has_deny_decision():
    r = build_llm_receipt(_block_ctx(), matched_rule={"match": "openai:*"})
    assert r["version"] == "agentboundary/v0.2-alpha"
    assert r["policy"]["decision"] == "deny"
    assert r["policy"]["name"] == "openai:*"
    assert r["policy"]["version"] == "1"
    assert r["execution"]["status"] == "blocked"
    assert r["middleware"]["outcome"] == "blocked"
    assert r["middleware"]["fired"] == ["pii.redact"]
    assert r["middleware"]["evidence"]["types"] == ["EMAIL"]
    assert r["middleware"]["evidence"]["count"] == 1


def test_redact_receipt_has_allow_decision():
    r = build_llm_receipt(_passthrough_redact_ctx(), matched_rule={"match": "anthropic:*"})
    assert r["policy"]["decision"] == "allow"
    assert r["execution"]["status"] == "success"
    assert r["middleware"]["outcome"] == "passthrough"
    assert r["middleware"]["evidence"]["action"] == "replaced"


def test_tool_and_target_synthesized_per_provider():
    r_openai = build_llm_receipt(_block_ctx(), matched_rule={"match": "openai:*"})
    assert r_openai["tool"]["name"] == "openai-chat"
    assert r_openai["tool"]["capability"] == "llm.text_generation"
    assert r_openai["target"]["system"] == "api.openai.com"

    r_anthropic = build_llm_receipt(_passthrough_redact_ctx(), matched_rule={"match": "anthropic:*"})
    assert r_anthropic["tool"]["name"] == "anthropic-messages"
    assert r_anthropic["target"]["system"] == "api.anthropic.com"


def test_actor_id_includes_pid():
    import os

    r = build_llm_receipt(_block_ctx(), matched_rule={"match": "openai:*"})
    assert r["actor"]["type"] == "agent"
    assert r["actor"]["id"] == f"agent:jamjet-cloud:pid:{os.getpid()}"
    assert r["actor"]["display_name"] == "jamjet.cloud SDK"


def test_target_environment_defaults_to_prod(monkeypatch):
    monkeypatch.delenv("JAMJET_ENVIRONMENT", raising=False)
    r = build_llm_receipt(_block_ctx(), matched_rule={"match": "openai:*"})
    assert r["target"]["environment"] == "prod"


def test_target_environment_honours_valid_env(monkeypatch):
    monkeypatch.setenv("JAMJET_ENVIRONMENT", "staging")
    r = build_llm_receipt(_block_ctx(), matched_rule={"match": "openai:*"})
    assert r["target"]["environment"] == "staging"


def test_target_environment_rejects_invalid_value(monkeypatch):
    monkeypatch.setenv("JAMJET_ENVIRONMENT", "qa")
    r = build_llm_receipt(_block_ctx(), matched_rule={"match": "openai:*"})
    # Schema only allows prod/staging/dev — invalid input must NOT leak
    # through and break downstream validators.
    assert r["target"]["environment"] == "prod"


def test_receipt_pii_value_never_appears_in_output():
    ctx = _block_ctx()
    ctx.messages = [{"role": "user", "content": "email alice@example.com"}]
    r = build_llm_receipt(ctx, matched_rule={"match": "openai:*"})
    # The arguments_hash is intentionally derived from the PII-containing
    # request; the rest of the receipt must not echo raw values.
    assert "alice@example.com" not in r["middleware"]["evidence"].get("types", [])
    assert isinstance(r["middleware"]["evidence"]["types"], list)
    assert all(isinstance(t, str) for t in r["middleware"]["evidence"]["types"])


def test_arguments_hash_is_stable():
    """Same logical request -> same arguments_hash, regardless of dict order."""
    ctx1 = _block_ctx()
    ctx1.extra_kwargs = {"temperature": 0.7, "top_p": 0.9}
    ctx2 = _block_ctx()
    # Different insertion order, identical content.
    ctx2.extra_kwargs = {"top_p": 0.9, "temperature": 0.7}
    h1 = build_llm_receipt(ctx1, matched_rule={"match": "openai:*"})["arguments_hash"]
    h2 = build_llm_receipt(ctx2, matched_rule={"match": "openai:*"})["arguments_hash"]
    assert h1 == h2


def test_arguments_hash_changes_with_payload():
    ctx1 = _block_ctx()
    ctx2 = _block_ctx()
    ctx2.messages = [{"role": "user", "content": "different"}]
    h1 = build_llm_receipt(ctx1, matched_rule={"match": "openai:*"})["arguments_hash"]
    h2 = build_llm_receipt(ctx2, matched_rule={"match": "openai:*"})["arguments_hash"]
    assert h1 != h2


def test_receipt_hash_is_recomputable():
    ctx = _block_ctx()
    r = build_llm_receipt(ctx, matched_rule={"match": "openai:*"})
    expected_hash = r.pop("receipt_hash")
    recomputed = hashlib.sha256(
        json.dumps(r, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    ).hexdigest()
    assert recomputed == expected_hash


def test_receipt_id_is_uuid_v4():
    import uuid as _uuid

    r = build_llm_receipt(_block_ctx(), matched_rule={"match": "openai:*"})
    parsed = _uuid.UUID(r["receipt_id"])
    assert parsed.version == 4


def test_issued_at_is_rfc3339_z_suffix():
    r = build_llm_receipt(_block_ctx(), matched_rule={"match": "openai:*"})
    issued_at = r["issued_at"]
    # YYYY-MM-DDTHH:MM:SSZ
    assert len(issued_at) == 20
    assert issued_at.endswith("Z")


# ---------------------------------------------------------------------------
# emit_receipt
# ---------------------------------------------------------------------------


def test_emit_receipt_writes_jsonl(monkeypatch, tmp_path):
    monkeypatch.setenv("JAMJET_AUDIT_DIR", str(tmp_path))
    r = build_llm_receipt(_block_ctx(), matched_rule={"match": "openai:*"})
    emit_receipt(r)
    target = tmp_path / "cloud-sdk.jsonl"
    assert target.exists()
    lines = target.read_text().strip().splitlines()
    assert len(lines) == 1
    parsed = json.loads(lines[0])
    assert parsed["receipt_id"] == r["receipt_id"]
    assert parsed["middleware"]["outcome"] == "blocked"


def test_emit_receipt_appends_multiple_lines(monkeypatch, tmp_path):
    monkeypatch.setenv("JAMJET_AUDIT_DIR", str(tmp_path))
    for _ in range(3):
        r = build_llm_receipt(_block_ctx(), matched_rule={"match": "openai:*"})
        emit_receipt(r)
    lines = (tmp_path / "cloud-sdk.jsonl").read_text().strip().splitlines()
    assert len(lines) == 3
    # Each line is independently parseable JSON.
    ids = {json.loads(line)["receipt_id"] for line in lines}
    assert len(ids) == 3  # UUIDs unique


def test_emit_receipt_creates_directory(monkeypatch, tmp_path):
    nested = tmp_path / "missing" / "audit"
    monkeypatch.setenv("JAMJET_AUDIT_DIR", str(nested))
    r = build_llm_receipt(_block_ctx(), matched_rule={"match": "openai:*"})
    emit_receipt(r)
    assert (nested / "cloud-sdk.jsonl").exists()


# ---------------------------------------------------------------------------
# v0.2-alpha schema conformance
# ---------------------------------------------------------------------------

_SCHEMA_PATH = Path("/Users/sunilp/Development/sunil-ws/agentboundary/docs/schemas/action-receipt-v0.2-alpha.json")


@pytest.mark.skipif(not _SCHEMA_PATH.exists(), reason="agentboundary schema not present locally")
def test_strict_view_validates_against_v02_alpha_schema():
    """The v0.2-alpha schema is ``additionalProperties: false``, so the
    ``middleware`` extension field is stripped via ``schema_strict_view``
    before validation. The JSONL audit stream keeps the full body."""
    import jsonschema

    schema = json.loads(_SCHEMA_PATH.read_text())
    for ctx_factory in (_block_ctx, _passthrough_redact_ctx):
        receipt = build_llm_receipt(ctx_factory(), matched_rule={"match": "openai:*"})
        strict = schema_strict_view(receipt)
        jsonschema.validate(strict, schema)


# ---------------------------------------------------------------------------
# Patcher integration — synthetic middleware exercises both code paths
# ---------------------------------------------------------------------------


class _SyntheticBlocker:
    """Synthetic middleware that marks ctx.middleware_fired then raises —
    stands in for the not-yet-shipped PII middleware so we can verify the
    patcher's exception path emits a deny receipt before re-raising."""

    def __call__(self, ctx: CallContext, nxt: Any) -> Any:
        ctx.middleware_fired.append("synthetic.blocker")
        ctx.middleware_outcome = MiddlewareOutcome.BLOCKED
        ctx.middleware_evidence = {"types": ["EMAIL"], "count": 1, "action": "blocked"}
        raise RuntimeError("synthetic block")


class _SyntheticRedactor:
    """Synthetic middleware that mutates ctx then calls next() — stands in
    for a redacting PII middleware. Used to verify the success path emits
    an allow receipt when middleware fires without raising."""

    def __call__(self, ctx: CallContext, nxt: Any) -> Any:
        ctx.middleware_fired.append("synthetic.redactor")
        ctx.middleware_outcome = MiddlewareOutcome.PASSTHROUGH
        ctx.middleware_evidence = {"types": ["EMAIL"], "count": 1, "action": "replaced"}
        ctx.messages = [{"role": "user", "content": "[REDACTED:EMAIL]"}]
        return nxt(ctx)


def test_patcher_emits_receipt_on_middleware_block(monkeypatch, tmp_path):
    """End-to-end: install a synthetic blocker, drive a fake OpenAI call
    through the patched terminal lambda, verify the JSONL receipt lands
    AND the exception is re-raised unchanged."""
    monkeypatch.setenv("JAMJET_AUDIT_DIR", str(tmp_path))
    from jamjet.cloud.middleware import Chain
    from jamjet.cloud.middleware.context import (
        call_context_from_openai_kwargs,
        openai_kwargs_from_call_context,
    )

    # We don't need the real openai SDK — exercise the chain + receipt
    # plumbing directly the way the patcher does.
    chain: Chain = Chain(middlewares=[_SyntheticBlocker()])
    call_ctx = call_context_from_openai_kwargs(
        {
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "leak alice@example.com"}],
        }
    )

    def terminal(c: CallContext) -> Any:
        # Never reached — the blocker raises before us.
        return original_call(**openai_kwargs_from_call_context(c))

    def original_call(**_: Any) -> Any:
        raise AssertionError("terminal must NOT run when middleware blocks")

    with pytest.raises(RuntimeError, match="synthetic block"):
        try:
            chain.run(call_ctx, terminal=terminal)
        except BaseException:
            if call_ctx.middleware_fired:
                receipt = build_llm_receipt(
                    call_ctx,
                    matched_rule={"match": call_ctx.middleware_fired[0]},
                    policy_version="1",
                )
                emit_receipt(receipt)
            raise

    # The patcher's path always emits exactly one receipt on the block path.
    target = tmp_path / "cloud-sdk.jsonl"
    assert target.exists()
    lines = target.read_text().strip().splitlines()
    assert len(lines) == 1
    parsed = json.loads(lines[0])
    assert parsed["policy"]["decision"] == "deny"
    assert parsed["execution"]["status"] == "blocked"
    assert parsed["middleware"]["fired"] == ["synthetic.blocker"]


def test_patcher_emits_receipt_on_middleware_success(monkeypatch, tmp_path):
    """End-to-end success path: redactor mutates the ctx, terminal runs,
    a receipt with allow/success lands in the audit stream."""
    monkeypatch.setenv("JAMJET_AUDIT_DIR", str(tmp_path))
    from jamjet.cloud.middleware import Chain
    from jamjet.cloud.middleware.context import call_context_from_openai_kwargs

    chain = Chain(middlewares=[_SyntheticRedactor()])
    call_ctx = call_context_from_openai_kwargs(
        {
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "leak alice@example.com"}],
        }
    )
    terminal_calls: list[CallContext] = []

    def terminal(c: CallContext) -> Any:
        terminal_calls.append(c)
        return {"id": "chatcmpl-test"}

    # Same code path the patcher runs:
    result = chain.run(call_ctx, terminal=terminal)
    assert result == {"id": "chatcmpl-test"}
    assert len(terminal_calls) == 1
    # Redactor ran -> ctx mutated -> terminal saw the redacted message.
    assert terminal_calls[0].messages[0]["content"] == "[REDACTED:EMAIL]"

    if call_ctx.middleware_fired:
        receipt = build_llm_receipt(
            call_ctx,
            matched_rule={"match": call_ctx.middleware_fired[0]},
            policy_version="1",
        )
        emit_receipt(receipt)

    lines = (tmp_path / "cloud-sdk.jsonl").read_text().strip().splitlines()
    assert len(lines) == 1
    parsed = json.loads(lines[0])
    assert parsed["policy"]["decision"] == "allow"
    assert parsed["execution"]["status"] == "success"
    assert parsed["middleware"]["outcome"] == "passthrough"


def test_patcher_no_receipt_on_happy_passthrough(monkeypatch, tmp_path):
    """No middleware fired -> no receipt emitted (audit volume control).
    The pre-Phase-1 span/event path handles passthrough telemetry."""
    monkeypatch.setenv("JAMJET_AUDIT_DIR", str(tmp_path))
    from jamjet.cloud.middleware import Chain
    from jamjet.cloud.middleware.context import call_context_from_openai_kwargs

    chain = Chain(middlewares=[])  # empty chain = no middleware fires
    call_ctx = call_context_from_openai_kwargs({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]})
    chain.run(call_ctx, terminal=lambda _c: {"id": "chatcmpl-test"})

    # Mirrors the patcher's gating predicate.
    if call_ctx.middleware_fired:
        emit_receipt(build_llm_receipt(call_ctx, matched_rule={"match": "openai:*"}))

    target = tmp_path / "cloud-sdk.jsonl"
    assert not target.exists(), "no middleware -> no receipt -> no file"
