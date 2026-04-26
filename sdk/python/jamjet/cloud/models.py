from __future__ import annotations

import time
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any


@dataclass
class Span:
    """A single span in a trace, representing one LLM call or decorated function."""

    trace_id: str
    span_id: str
    kind: str
    name: str
    parent_span_id: str | None = None
    sequence: int = 0
    timestamp: datetime = field(default_factory=lambda: datetime.now(UTC))
    duration_ms: float | None = None
    model: str | None = None
    input_tokens: int | None = None
    output_tokens: int | None = None
    cost_usd: float | None = None
    status: str = "pending"
    payload: dict[str, Any] = field(default_factory=dict)
    # Multi-agent attribution (Plan 5 Phase 1). agent_name is the human-readable
    # identifier; the cloud API resolves it to a UUID on ingest. agent_card_uri,
    # when present, lets the dashboard link out to a public Agent Card spec.
    agent_name: str | None = None
    agent_card_uri: str | None = None
    # Cross-trace lineage (Plan 5 Phase 2). When this trace was started by an
    # incoming request from another agent, these point to the upstream span.
    # The dashboard uses them to render this trace as a child sub-tree.
    originating_trace_id: str | None = None
    originating_span_id: str | None = None
    originating_agent_name: str | None = None
    # Session + end-user + environment attribution (Phase 2 + 0009 schema).
    # All opt-in; the SDK never auto-sniffs. end_user_email is PII and lives
    # in a separate cloud-side table; the SDK passes it through opportunistically.
    session_id: str | None = None
    environment: str | None = None
    release_version: str | None = None
    end_user_id: str | None = None
    end_user_email: str | None = None
    tags: tuple[str, ...] = ()
    # Typed failure category (Plan 5 Phase 4). Set when the span ends with an
    # error/blocked status. The dashboard renders these as a pie chart and
    # uses them for per-agent error-rate analytics.
    failure_mode: str | None = None
    _start_time: float = field(default_factory=time.monotonic, repr=False)

    def finish(self, status: str = "ok", duration_ms: float | None = None) -> None:
        """Mark span as finished with a status and computed duration."""
        self.status = status
        if duration_ms is not None:
            self.duration_ms = duration_ms
        else:
            self.duration_ms = (time.monotonic() - self._start_time) * 1000

    def fail(self, mode: str, *, status: str = "error") -> None:
        """Mark span as failed with a typed failure mode (Plan 5 Phase 4).

        Valid modes (CHECK constraint server-side): model_timeout,
        model_refusal, model_rate_limited, model_invalid_request, tool_error,
        policy_block, budget_exceeded, approval_rejected, downstream_failure,
        network_error, validation_error, custom.
        """
        self.failure_mode = mode
        self.finish(status=status)

    def to_event_dict(self) -> dict[str, Any]:
        """Convert span to a dict suitable for the event ingest API."""
        d: dict[str, Any] = {
            "type": "span",
            "trace_id": self.trace_id,
            "span_id": self.span_id,
            "kind": self.kind,
            "name": self.name,
            "sequence": self.sequence,
            "timestamp": self.timestamp.isoformat(),
            "status": self.status,
        }
        if self.parent_span_id is not None:
            d["parent_span_id"] = self.parent_span_id
        if self.duration_ms is not None:
            d["duration_ms"] = int(self.duration_ms)
        if self.model is not None:
            d["model"] = self.model
        if self.input_tokens is not None:
            d["input_tokens"] = self.input_tokens
        if self.output_tokens is not None:
            d["output_tokens"] = self.output_tokens
        if self.cost_usd is not None:
            d["cost_usd"] = self.cost_usd
        if self.payload:
            d["payload"] = self.payload
        if self.agent_name is not None:
            d["agent_name"] = self.agent_name
        if self.agent_card_uri is not None:
            d["agent_card_uri"] = self.agent_card_uri
        if self.originating_trace_id is not None:
            d["originating_trace_id"] = self.originating_trace_id
        if self.originating_span_id is not None:
            d["originating_span_id"] = self.originating_span_id
        if self.originating_agent_name is not None:
            d["originating_agent_name"] = self.originating_agent_name
        if self.session_id is not None:
            d["session_id"] = self.session_id
        if self.environment is not None:
            d["environment"] = self.environment
        if self.release_version is not None:
            d["release_version"] = self.release_version
        if self.end_user_id is not None:
            d["end_user_id"] = self.end_user_id
        if self.end_user_email is not None:
            d["end_user_email"] = self.end_user_email
        if self.tags:
            # Tags ride in the payload jsonb under the reserved 'tags' key —
            # avoids a column for low-cardinality free-form labels.
            d.setdefault("payload", {})["tags"] = list(self.tags)
        if self.failure_mode is not None:
            d["failure_mode"] = self.failure_mode
        return d


@dataclass
class PolicyDecision:
    """Result of evaluating a tool name against the policy set."""

    blocked: bool
    policy_kind: str
    pattern: str | None = None
    tool_name: str | None = None
