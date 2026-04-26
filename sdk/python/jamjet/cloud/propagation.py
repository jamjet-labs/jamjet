"""Cross-agent trace propagation (Plan 5 Phase 2).

Defines the W3C-traceparent + JamJet-tracestate header format and provides
``inject_headers`` / ``extract_headers`` helpers. Most users never call these
directly — the auto-patched transports (httpx, A2A, MCP) wire it up
transparently. Helpers exist for custom transports.

Header spec lives in `docs/propagation.md` (jamjet-cloud private repo,
mirrored at https://docs.jamjet.dev/en/docs/cloud-quickstart#propagation
once it's published).
"""

from __future__ import annotations

import re
import threading
from contextvars import ContextVar
from dataclasses import dataclass
from typing import Mapping, MutableMapping
from urllib.parse import quote, unquote

# ---------------------------------------------------------------------------
# Header constants
# ---------------------------------------------------------------------------

TRACEPARENT_HEADER = "traceparent"
TRACESTATE_HEADER = "tracestate"
JJ_TRACESTATE_KEY = "jj"

# ``00-<32hex>-<16hex>-<2hex>``
_TRACEPARENT_RE = re.compile(r"^00-(?P<trace>[0-9a-f]{32})-(?P<parent>[0-9a-f]{16})-(?P<flags>[0-9a-f]{2})$")


# ---------------------------------------------------------------------------
# Originating context (what the receiver picks up)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class OriginatingContext:
    """Captured upstream context. Set by ``extract_headers`` on the receiver.

    Subsequent spans in this scope are tagged with these fields so the cloud
    can render the receiver's trace as a child of the caller's span.
    """

    trace_id: str  # full prefixed form, e.g. "tr_d6d12272..."
    span_id: str  # full prefixed form, e.g. "sp_d6d12272..."
    agent_id: str | None = None  # UUID without dashes, if upstream sent one
    agent_name: str | None = None


_originating_var: ContextVar[OriginatingContext | None] = ContextVar("jamjet_originating", default=None)


def get_originating() -> OriginatingContext | None:
    """Return the upstream context for this request, or None."""
    return _originating_var.get()


def set_originating(ctx: OriginatingContext | None) -> None:
    _originating_var.set(ctx)


# ---------------------------------------------------------------------------
# Inject (caller side)
# ---------------------------------------------------------------------------


def _trace_id_to_hex(trace_id: str) -> str:
    """Strip the ``tr_`` prefix to get a 32-char hex W3C trace-id.

    The SDK generates trace_ids as ``tr_`` + uuid4().hex (32 hex chars), so
    the W3C portion is just ``trace_id[3:]`` when length matches. We tolerate
    other shapes by hashing as a fallback.
    """
    if trace_id.startswith("tr_") and len(trace_id) == 35:
        return trace_id[3:]
    # Fallback: deterministic hash if the format is unexpected (shouldn't
    # happen with current SDK, but defensive — never raise just because we
    # can't propagate cleanly).
    import hashlib

    return hashlib.sha256(trace_id.encode("utf-8")).hexdigest()[:32]


def _span_id_to_hex(span_id: str) -> str:
    """Take the first 16 hex chars after ``sp_`` for the W3C parent-id.

    SDK span_ids are ``sp_`` + uuid4().hex (32 hex). W3C wants 16. Taking
    the first 16 is deterministic and good enough for uniqueness within a
    trace.
    """
    if span_id.startswith("sp_") and len(span_id) >= 19:
        return span_id[3:19]
    import hashlib

    return hashlib.sha256(span_id.encode("utf-8")).hexdigest()[:16]


def _build_traceparent(trace_id: str, span_id: str, sampled: bool = True) -> str:
    flags = "01" if sampled else "00"
    return f"00-{_trace_id_to_hex(trace_id)}-{_span_id_to_hex(span_id)}-{flags}"


def _build_tracestate(
    agent_id: str | None,
    agent_name: str | None,
    session_id: str | None = None,
    end_user_id: str | None = None,
) -> str | None:
    parts: list[str] = []
    if agent_id:
        parts.append(f"ag:{agent_id.replace('-', '')}")
    if agent_name:
        # Names can contain spaces / hyphens / dots; URL-encode the value
        # so the comma/equals separators in tracestate stay unambiguous.
        parts.append(f"an:{quote(agent_name, safe='')}")
    if session_id:
        parts.append(f"si:{quote(session_id, safe='')}")
    if end_user_id:
        # Opaque only — never email or PII via this header.
        parts.append(f"eu:{quote(end_user_id, safe='')}")
    if not parts:
        return None
    return f"{JJ_TRACESTATE_KEY}={';'.join(parts)}"


def inject_headers(
    headers: MutableMapping[str, str],
    *,
    trace_id: str | None = None,
    span_id: str | None = None,
    agent_id: str | None = None,
    agent_name: str | None = None,
) -> MutableMapping[str, str]:
    """Write traceparent + tracestate into ``headers``.

    By default, pulls trace_id / span_id from the current trace context and
    agent info from the current agent context. Pass overrides only for
    custom call paths.

    Returns the same mapping for chaining. The mapping is mutated in place
    so this works with httpx.Headers, dict, requests' CaseInsensitiveDict,
    and friends.
    """
    # Lazy imports to avoid circular deps with trace.py / agent.py at
    # module-load time.
    from .agent import get_current_agent
    from .trace import get_context

    if trace_id is None or span_id is None:
        ctx = get_context()
        trace_id = trace_id or ctx.trace_id
        # If the caller didn't provide a span_id, generate one for the
        # outbound call. The receiver will create its own trace anyway, but
        # parent_id should be a real (parent-side) span.
        if span_id is None:
            # Reuse trace context's id without bumping sequence — the actual
            # outbound span already exists if the caller is inside @trace
            # or under an auto-patched HTTP wrapper. For raw inject_headers,
            # we synthesize a stable id from trace_id so retries don't
            # explode the parent-id space.
            span_id = "sp_" + trace_id[3:35] if trace_id.startswith("tr_") else "sp_" + trace_id[:32]

    if agent_id is None and agent_name is None:
        current = get_current_agent()
        if current is not None:
            agent_name = current.name

    # Pull session/end-user from current context — they propagate across
    # agents so a multi-agent conversation stays joinable in the dashboard.
    from .user_context import get_user_context

    user_ctx = get_user_context()
    session_id = user_ctx.session_id if user_ctx else None
    end_user_id = user_ctx.end_user_id if user_ctx else None

    headers[TRACEPARENT_HEADER] = _build_traceparent(trace_id, span_id)
    tracestate = _build_tracestate(agent_id, agent_name, session_id, end_user_id)
    if tracestate:
        # If a tracestate already exists on the request, prepend ours.
        # W3C says the most recent vendor goes first, comma-separated.
        existing = headers.get(TRACESTATE_HEADER)
        if existing and JJ_TRACESTATE_KEY + "=" not in existing:
            headers[TRACESTATE_HEADER] = f"{tracestate},{existing}"
        elif not existing:
            headers[TRACESTATE_HEADER] = tracestate
        # If existing already has jj=…, leave the more-recent existing entry
        # alone (we trust the caller knows what they're doing).
    return headers


# ---------------------------------------------------------------------------
# Extract (receiver side)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class _ParsedTraceState:
    agent_id: str | None = None
    agent_name: str | None = None
    session_id: str | None = None
    end_user_id: str | None = None


def _parse_jj_tracestate(tracestate: str) -> _ParsedTraceState:
    """Pull our vendor entry (jj=…) into a structured view."""
    for entry in tracestate.split(","):
        entry = entry.strip()
        if not entry.startswith(JJ_TRACESTATE_KEY + "="):
            continue
        body = entry[len(JJ_TRACESTATE_KEY) + 1 :]
        fields: dict[str, str] = {}
        for kv in body.split(";"):
            if ":" not in kv:
                continue
            k, v = kv.split(":", 1)
            fields[k.strip()] = v.strip()
        return _ParsedTraceState(
            agent_id=fields.get("ag") or None,
            agent_name=unquote(fields["an"]) if "an" in fields else None,
            session_id=unquote(fields["si"]) if "si" in fields else None,
            end_user_id=unquote(fields["eu"]) if "eu" in fields else None,
        )
    return _ParsedTraceState()


def extract_headers(headers: Mapping[str, str]) -> OriginatingContext | None:
    """Read traceparent + tracestate; record originating context for this scope.

    Subsequent spans (from auto-patched LLM calls, @trace decorators, or
    explicit Span creation) will be tagged with these originating_* fields,
    which the API joins to render the receiver's trace as a child sub-tree
    of the caller's span.

    Returns the parsed context (or None if no propagation header found).
    Header lookups are case-insensitive: works with raw dicts, httpx.Headers,
    Werkzeug's EnvironHeaders, etc.
    """
    # Case-insensitive header lookup
    tp = _get_ci(headers, TRACEPARENT_HEADER)
    if not tp:
        return None
    m = _TRACEPARENT_RE.match(tp.strip())
    if not m:
        return None

    trace_hex = m.group("trace")
    parent_hex = m.group("parent")
    # Re-prefix to match the SDK's internal id format. We pad parent_hex to
    # 32 chars so receivers see ``sp_<16hex><16 zeros>``; this keeps the
    # column lookups consistent with locally-generated span_ids that are
    # always ``sp_<32hex>``. The cloud's index does prefix matching anyway.
    trace_id = "tr_" + trace_hex
    span_id = "sp_" + parent_hex + ("0" * 16)

    parsed = _ParsedTraceState()
    ts = _get_ci(headers, TRACESTATE_HEADER)
    if ts:
        parsed = _parse_jj_tracestate(ts)

    ctx = OriginatingContext(
        trace_id=trace_id,
        span_id=span_id,
        agent_id=parsed.agent_id,
        agent_name=parsed.agent_name,
    )
    set_originating(ctx)

    # Session_id and end_user_id propagate as user-context. Receiver auto-
    # adopts them so subsequent spans are tagged with the same session — no
    # manual plumbing required for multi-agent conversations.
    if parsed.session_id or parsed.end_user_id:
        from .user_context import UserContext, _user_var

        existing = _user_var.get()
        # Don't clobber an explicit set_user_context() the receiver may have
        # already done — only fill the gaps.
        merged = UserContext(
            session_id=(existing.session_id if existing and existing.session_id else parsed.session_id),
            end_user_id=(existing.end_user_id if existing and existing.end_user_id else parsed.end_user_id),
            end_user_email=(existing.end_user_email if existing else None),
            tags=(existing.tags if existing else ()),
        )
        _user_var.set(merged)

    return ctx


def _get_ci(headers: Mapping[str, str], key: str) -> str | None:
    """Case-insensitive header lookup that handles Mapping-like types."""
    # Many HTTP libs already normalize to lower or have case-insensitive
    # lookup; fall through generically.
    if key in headers:
        return headers[key]
    lower_key = key.lower()
    if lower_key in headers:
        return headers[lower_key]
    # Linear scan — small dicts.
    for k, v in headers.items():
        if k.lower() == lower_key:
            return v
    return None


# ---------------------------------------------------------------------------
# Module state
# ---------------------------------------------------------------------------

_lock = threading.Lock()
