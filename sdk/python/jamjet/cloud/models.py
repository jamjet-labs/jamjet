from __future__ import annotations

import time
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
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
    timestamp: datetime = field(default_factory=lambda: datetime.now(timezone.utc))
    duration_ms: float | None = None
    model: str | None = None
    input_tokens: int | None = None
    output_tokens: int | None = None
    cost_usd: float | None = None
    status: str = "pending"
    payload: dict[str, Any] = field(default_factory=dict)
    _start_time: float = field(default_factory=time.monotonic, repr=False)

    def finish(self, status: str = "ok", duration_ms: float | None = None) -> None:
        """Mark span as finished with a status and computed duration."""
        self.status = status
        if duration_ms is not None:
            self.duration_ms = duration_ms
        else:
            self.duration_ms = (time.monotonic() - self._start_time) * 1000

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
        return d


@dataclass
class PolicyDecision:
    """Result of evaluating a tool name against the policy set."""

    blocked: bool
    policy_kind: str
    pattern: str | None = None
    tool_name: str | None = None
