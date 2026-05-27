"""Pre-call policy middleware for jamjet.cloud — extends the existing patcher
with a fixed-order chain that gates LLM calls through policy.yaml rules.

Phase 1 ships the substrate plus the PII pre-LLM middleware. Phases 2 (cache)
and 3 (fallback) extend the same Protocol; the chain order is fixed at
PII -> cache -> fallback.
"""
from __future__ import annotations
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Callable, Protocol


class MiddlewareOutcome(str, Enum):
    """The terminal disposition of a single LLM-call attempt. Phase 1 emits
    PASSTHROUGH / BLOCKED / DETECTOR_ERROR. CACHE_HIT / CACHE_UNAVAILABLE
    are reserved for Phase 2; FALLBACK is reserved for Phase 3 — they're
    declared here so the SDK can adopt them without an enum change."""
    PASSTHROUGH = "passthrough"            # no middleware short-circuited; original() ran
    BLOCKED = "blocked"                    # a middleware raised before original()
    DETECTOR_ERROR = "detector_error"      # detector threw; fail-open path took the call
    CACHE_HIT = "cache_hit"                # Phase 2
    CACHE_UNAVAILABLE = "cache_unavailable"  # Phase 2
    FALLBACK = "fallback"                  # Phase 3


@dataclass
class CallContext:
    """Provider-agnostic view of an LLM call. The patcher builds one of these
    from vendor kwargs before invoking the chain; each middleware may mutate
    it (e.g. PII redact replaces messages); the terminal call converts it
    back to vendor kwargs.

    Two telemetry fields (middleware_fired, middleware_outcome) accumulate
    as the chain runs and are read by the patcher's post-response code to
    populate the `middleware` extension on the AgentBoundary receipt."""
    provider: str                                    # "openai" | "anthropic"
    model: str                                       # "gpt-4o", "claude-haiku-4-5", ...
    messages: list[dict[str, Any]] = field(default_factory=list)
    tools: list[dict[str, Any]] = field(default_factory=list)
    system: str | None = None                        # extracted, not mutated by PII middleware
    extra_kwargs: dict[str, Any] = field(default_factory=dict)
    middleware_fired: list[str] = field(default_factory=list)
    middleware_outcome: MiddlewareOutcome | None = None
    middleware_evidence: dict[str, Any] | None = None  # sanitized; see PIIMiddleware

    @property
    def identifier(self) -> str:
        """The match string used by the policy evaluator on the LLM-call path."""
        return f"{self.provider}:{self.model}"


# Vendor response shape is opaque at the substrate level — Phase 1 returns it
# as-is from the terminal call. Phase 3 introduces JamjetUnifiedResponse.
Response = Any


class PreCallMiddleware(Protocol):
    """Implementations live in pii.py / cache.py / fallback.py. The chain
    calls each in fixed order with a `next` continuation."""

    def __call__(
        self,
        ctx: CallContext,
        next: Callable[[CallContext], Response],
    ) -> Response: ...


__all__ = [
    "CallContext",
    "MiddlewareOutcome",
    "PreCallMiddleware",
    "Response",
]
