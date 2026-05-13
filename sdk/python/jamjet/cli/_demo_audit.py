"""Audit event for jamjet demo commands. Writes to .jamjet-demo/runs/<id>.json.

Emits the v1 portable audit schema (adapter/host/ts/schema_version/...) so
output from `jamjet demo` matches @jamjet/claude-code-hook, @jamjet/mcp-shim,
and @jamjet/cloud. See jamjet-policy/conformance/audit-event-shape.json.
"""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Literal

RuleKind = Literal["allow", "block", "require_approval", "audit"]


@dataclass
class DemoAuditEvent:
    run_id: str
    demo: str
    decision: str
    tool: str
    rule: str | None = None
    executed: bool = False
    trace_id: str | None = None
    decision_id: str | None = None
    rule_kind: RuleKind | None = None
    args: dict[str, Any] = field(default_factory=dict)
    extra: dict[str, Any] = field(default_factory=dict)
    ts: str = field(default_factory=lambda: datetime.now(UTC).isoformat())
    # v1 audit-schema discriminators
    adapter: str = "python-sdk"
    host: str = "python"
    server: str | None = None
    policy_version: str = "1"
    schema_version: int = 1

    def to_dict(self) -> dict[str, Any]:
        d = asdict(self)
        # Backward-compat alias for jamjet 0.8.1 consumers that read `timestamp`.
        d["timestamp"] = d["ts"]
        return d


def write_audit_event(event: DemoAuditEvent, base_dir: Path | None = None) -> Path:
    """Write a demo audit event under .jamjet-demo/runs/<run-id>.json. Returns the path."""
    root = (base_dir or Path.cwd()) / ".jamjet-demo" / "runs"
    root.mkdir(parents=True, exist_ok=True)
    path = root / f"{event.run_id}.json"
    path.write_text(json.dumps(event.to_dict(), indent=2, sort_keys=True))
    return path
