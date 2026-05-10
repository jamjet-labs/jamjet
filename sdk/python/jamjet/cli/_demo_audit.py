"""Audit event for jamjet demo commands. Writes to .jamjet-demo/runs/<id>.json."""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass
class DemoAuditEvent:
    run_id: str
    demo: str
    decision: str
    tool: str
    rule: str | None = None
    executed: bool = False
    extra: dict[str, Any] = field(default_factory=dict)
    timestamp: str = field(default_factory=lambda: datetime.now(timezone.utc).isoformat())

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


def write_audit_event(event: DemoAuditEvent, base_dir: Path | None = None) -> Path:
    """Write a demo audit event under .jamjet-demo/runs/<run-id>.json. Returns the path."""
    root = (base_dir or Path.cwd()) / ".jamjet-demo" / "runs"
    root.mkdir(parents=True, exist_ok=True)
    path = root / f"{event.run_id}.json"
    path.write_text(json.dumps(event.to_dict(), indent=2, sort_keys=True))
    return path
