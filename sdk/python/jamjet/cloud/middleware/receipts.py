"""LLM-call receipt emission for jamjet.cloud middleware.

Emits AgentBoundary v0.2-alpha receipts ONLY when middleware fires —
the happy passthrough path is unchanged from the pre-Phase-1 patcher.

Receipt shape uses synthetic tool semantics to fit the v0.2-alpha schema:
- ``tool.capability`` = ``"llm.text_generation"``
- ``target.system`` = ``"api.openai.com"`` | ``"api.anthropic.com"``
- ``arguments_hash`` covers the full LLM-call request envelope

The locked receipt shape carries a ``middleware`` extension field with
``{fired, outcome, evidence?}``. The v0.2-alpha schema is
``additionalProperties: false`` at the top level so the extension cannot
travel inside a strict-validated body. ``schema_strict_view()`` strips the
extension for schema validation while the JSONL audit stream keeps it; the
``receipt_hash`` is computed over the full body (extension included) so
tampering with telemetry breaks the chain.
"""

from __future__ import annotations

import hashlib
import json
import os
import uuid
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from jamjet.cloud.middleware import CallContext, MiddlewareOutcome

_PROVIDER_TO_TOOL_NAME = {
    "openai": "openai-chat",
    "anthropic": "anthropic-messages",
}
_PROVIDER_TO_TARGET_SYSTEM = {
    "openai": "api.openai.com",
    "anthropic": "api.anthropic.com",
}
_ALLOWED_ENVS = {"prod", "staging", "dev"}


def _canonical_json(value: Any) -> str:
    """Canonical JSON for hashing: sorted keys, no whitespace, UTF-8."""
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def _sha256(s: str) -> str:
    return hashlib.sha256(s.encode("utf-8")).hexdigest()


def _resolve_environment() -> str:
    env = os.environ.get("JAMJET_ENVIRONMENT", "prod")
    return env if env in _ALLOWED_ENVS else "prod"


def _framework_version() -> str:
    """Best-effort SDK version string. Prefers ``jamjet.cloud.VERSION``
    (not yet defined as of 0.8.x), falls back to top-level ``__version__``,
    then to a literal ``"unknown"``."""
    try:
        from jamjet.cloud import VERSION  # type: ignore[attr-defined]

        return str(VERSION)
    except Exception:
        pass
    try:
        from jamjet import __version__

        return str(__version__)
    except Exception:
        return "unknown"


def _rfc3339_utc(dt: datetime) -> str:
    """RFC 3339 UTC timestamp (no sub-second precision)."""
    return dt.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def build_llm_receipt(
    ctx: CallContext,
    *,
    matched_rule: dict[str, Any],
    policy_version: str = "1",
) -> dict[str, Any]:
    """Build an AgentBoundary v0.2-alpha receipt for an LLM call that
    triggered middleware. Called from the patcher only when
    ``ctx.middleware_fired`` is non-empty or a ``JamJetPolicyBlocked`` has
    been caught.

    The returned dict carries a non-schema ``middleware`` extension field;
    use :func:`schema_strict_view` to obtain a body that conforms to the
    v0.2-alpha JSON Schema.
    """
    decision = "deny" if ctx.middleware_outcome == MiddlewareOutcome.BLOCKED else "allow"
    exec_status = "blocked" if ctx.middleware_outcome == MiddlewareOutcome.BLOCKED else "success"

    args_payload = {
        "model": ctx.model,
        "messages": ctx.messages,
        "tools": ctx.tools,
        "system": ctx.system,
        "extra_kwargs": ctx.extra_kwargs,
    }

    now = _rfc3339_utc(datetime.now(UTC))

    body: dict[str, Any] = {
        "version": "agentboundary/v0.2-alpha",
        "receipt_id": str(uuid.uuid4()),
        "issued_at": now,
        "actor": {
            "type": "agent",
            "id": f"agent:jamjet-cloud:pid:{os.getpid()}",
            "display_name": "jamjet.cloud SDK",
        },
        "agent": {
            "framework": "jamjet-cloud",
            "framework_version": _framework_version(),
            "model": ctx.model or "unknown",
        },
        "tool": {
            "name": _PROVIDER_TO_TOOL_NAME.get(ctx.provider, ctx.provider or "unknown"),
            "capability": "llm.text_generation",
        },
        "target": {
            "system": _PROVIDER_TO_TARGET_SYSTEM.get(ctx.provider, ctx.provider or "unknown"),
            "environment": _resolve_environment(),
        },
        "arguments_hash": _sha256(_canonical_json(args_payload)),
        "policy": {
            "name": matched_rule.get("match", "unknown"),
            "version": str(policy_version),
            "decision": decision,
        },
        "execution": {
            "status": exec_status,
            # The v0.2-alpha schema requires `completed_at`; for Phase 1
            # the pre-call middleware fires synchronously around the LLM
            # call so completion ~ issuance is a defensible approximation.
            "completed_at": now,
        },
        "middleware": {
            "fired": list(ctx.middleware_fired),
            "outcome": (ctx.middleware_outcome.value if ctx.middleware_outcome else "passthrough"),
        },
    }
    if ctx.middleware_evidence:
        body["middleware"]["evidence"] = ctx.middleware_evidence

    # ``receipt_hash`` covers the full body (including the ``middleware``
    # extension) so tampering with telemetry breaks the chain. Hash MUST
    # exclude itself.
    body["receipt_hash"] = _sha256(_canonical_json(body))
    return body


def schema_strict_view(receipt: dict[str, Any]) -> dict[str, Any]:
    """Return a copy of ``receipt`` with non-schema extension fields removed
    so it conforms to ``additionalProperties: false`` on the v0.2-alpha
    schema. Phase 1 only adds the ``middleware`` extension."""
    strict = dict(receipt)
    strict.pop("middleware", None)
    return strict


def emit_receipt(receipt: dict[str, Any]) -> None:
    """Append one JSON line to ``~/.jamjet/audit/cloud-sdk.jsonl``. Creates
    the directory if missing. Override the destination via the
    ``JAMJET_AUDIT_DIR`` env var (used by tests + alternative deployments).

    Each call performs one ``open(..., "a")`` + one ``write`` so on POSIX
    systems writes shorter than ``PIPE_BUF`` (typically 4096 bytes) are
    atomic — concurrent emitters won't interleave lines."""
    audit_dir = Path(
        os.environ.get(
            "JAMJET_AUDIT_DIR",
            str(Path.home() / ".jamjet" / "audit"),
        )
    )
    audit_dir.mkdir(parents=True, exist_ok=True)
    target = audit_dir / "cloud-sdk.jsonl"
    with target.open("a", encoding="utf-8") as f:
        f.write(json.dumps(receipt) + "\n")
