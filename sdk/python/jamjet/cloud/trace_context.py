"""W3C trace-context reader for Path B trace_id propagation.

Mirrors @jamjet/cloud's TypeScript trace-context. Adapters call
read_traceparent(headers=...) with whatever they have; the result feeds
audit-event-v1's trace_id field. PRD 002's Traces page joins audit
decisions to trace timelines via this id.

Sources, in order:
  1. traceparent header on the incoming HTTP request (Vercel handler, Flask,
     FastAPI dependency, etc.).
  2. The active OpenTelemetry span context (if opentelemetry is installed).
     This catches OTel-instrumented hosts where the trace was started before
     the adapter ran.
  3. OTEL_TRACE_ID env var (some bridges populate this at process start).
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from typing import Mapping

_TRACEPARENT_RE = re.compile(
    r"^([0-9a-f]{2})-([0-9a-f]{32})-([0-9a-f]{16})-([0-9a-f]{2})$",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class Traceparent:
    version: str
    trace_id: str
    parent_id: str
    flags: str


def parse_traceparent(s: object) -> Traceparent | None:
    """Strict W3C parse. Returns None for anything malformed."""
    if not isinstance(s, str):
        return None
    m = _TRACEPARENT_RE.match(s.strip())
    if not m:
        return None
    version, trace_id, parent_id, flags = m.groups()
    if version == "ff":  # reserved
        return None
    if set(trace_id) == {"0"}:  # W3C: all-zero is invalid
        return None
    if set(parent_id) == {"0"}:
        return None
    return Traceparent(
        version=version,
        trace_id=trace_id.lower(),
        parent_id=parent_id.lower(),
        flags=flags,
    )


def _pick_header(headers: Mapping[str, object], name: str) -> str | None:
    """Case-insensitive header lookup that handles list-valued headers."""
    lower = name.lower()
    for key, val in headers.items():
        if str(key).lower() != lower:
            continue
        if isinstance(val, (list, tuple)):
            return str(val[0]) if val else None
        return str(val) if val is not None else None
    return None


def read_traceparent(
    headers: Mapping[str, object] | None = None,
) -> Traceparent | None:
    """Pick a traceparent from the available sources, in priority order."""

    if headers:
        raw = _pick_header(headers, "traceparent")
        parsed = parse_traceparent(raw)
        if parsed is not None:
            return parsed

    # OpenTelemetry current span (optional dep).
    try:
        from opentelemetry import trace as _ot_trace  # type: ignore[import-not-found]

        span = _ot_trace.get_current_span()
        ctx = span.get_span_context() if span is not None else None
        if ctx is not None and ctx.is_valid:
            sampled = bool(getattr(ctx.trace_flags, "sampled", False))
            return Traceparent(
                version="00",
                trace_id=f"{ctx.trace_id:032x}",
                parent_id=f"{ctx.span_id:016x}",
                flags="01" if sampled else "00",
            )
    except ImportError:
        pass

    env_id = os.environ.get("OTEL_TRACE_ID")
    if env_id and re.fullmatch(r"[0-9a-f]{32}", env_id, re.IGNORECASE):
        return Traceparent(
            version="00",
            trace_id=env_id.lower(),
            parent_id="0" * 16,
            flags="00",
        )

    return None
