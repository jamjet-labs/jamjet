"""Args redaction for events leaving the host via Cloud Sync (R9).

Distinct from ``jamjet.cloud.redaction`` (which handles Presidio-style PII
masking for span IO content). This module applies to audit-event-v1 ``args``
specifically and mirrors the TS implementation at
``jamjet-policy/packages/cli/src/sync/redaction.ts``:

  full → args = {"redacted": True}
       (default; no tool-call content leaves the host)
  hash → args = {"redacted": True, "sha256": "<hex>"}
       (stable hash; lets ops correlate identical calls without leaking)
  none → args passed through verbatim
       (operator must opt in explicitly)

Adapters call ``apply_args_redaction(event_dict, mode)`` before pushing to
Cloud. The local JSONL keeps the original args — only the Cloud-bound copy
is redacted.
"""

from __future__ import annotations

import hashlib
import json
import os
from typing import Any, Literal

ArgsRedactionMode = Literal["full", "hash", "none"]


def _stable_stringify(value: Any) -> str:
    """JSON serialization with sorted keys — same content → same hash."""
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def apply_args_redaction(
    event: dict[str, Any], mode: ArgsRedactionMode
) -> dict[str, Any]:
    """Return a new event dict with args replaced according to mode.

    Adds an ``args_redaction`` field so Cloud + dashboards can tell apart
    events that left with content from those that didn't.
    """
    out = dict(event)
    if mode == "none":
        out["args_redaction"] = "none"
        return out
    if mode == "full":
        out["args"] = {"redacted": True}
        out["args_redaction"] = "full"
        return out
    if mode == "hash":
        stable = _stable_stringify(event.get("args") or {})
        digest = hashlib.sha256(stable.encode("utf-8")).hexdigest()
        out["args"] = {"redacted": True, "sha256": digest}
        out["args_redaction"] = "hash"
        return out
    raise ValueError(f"unknown redaction mode: {mode}")


def resolve_args_redaction_mode(
    default: ArgsRedactionMode = "full",
) -> ArgsRedactionMode:
    """Resolve mode from JAMJET_ARGS_REDACTION env var."""
    val = (os.environ.get("JAMJET_ARGS_REDACTION") or default).lower()
    if val in ("full", "hash", "none"):
        return val  # type: ignore[return-value]
    return default
