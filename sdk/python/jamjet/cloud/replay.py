"""Plan 5 Phase 6.1/6.3 — replay bundle reader and replay-mode interceptor.

Flow:
  1. `jamjet-cloud replay <trace_id>` downloads the bundle and extracts it.
  2. The user re-runs their agent with JAMJET_REPLAY_BUNDLE=<dir>.
  3. On import, this module auto-activates if that env var is set.
  4. @tool-decorated functions look up recorded tool_outputs instead of
     executing; LLM patchers return stubs when JAMJET_STUB_MODELS=1.

Local capture (6.1):
  When capture_io=True in CloudConfig, EventQueue also appends each emitted
  event to .jamjet-replay.jsonl in the current working directory, producing
  a cassette that can be used offline without downloading from the cloud.
"""

from __future__ import annotations

import hashlib
import io
import json
import os
import tarfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# ---------------------------------------------------------------------------
# Bundle data model
# ---------------------------------------------------------------------------


@dataclass
class ReplayBundle:
    """Parsed replay bundle (replay-1.0 schema)."""

    manifest: dict[str, Any]
    events: list[dict[str, Any]]
    audit: list[dict[str, Any]]
    agents: list[dict[str, Any]]

    # Pre-built lookup: (tool_name, input_hash) → tool_output
    _tool_index: dict[tuple[str, str], Any] = field(default_factory=dict, repr=False)
    # Ordered list of recorded LLM responses (consumed sequentially for stubs)
    _llm_responses: list[str] = field(default_factory=list, repr=False)
    _llm_cursor: int = field(default=0, repr=False)

    def __post_init__(self) -> None:
        for ev in self.events:
            if ev.get("kind") == "tool_call":
                payload = ev.get("payload") or {}
                tool_name = payload.get("tool") or ev.get("name", "")
                tool_input = payload.get("tool_input") or {}
                tool_output = payload.get("tool_output")
                if tool_name and tool_output is not None:
                    key = (tool_name, _input_hash(tool_input))
                    # First occurrence wins — matches recorded order.
                    self._tool_index.setdefault(key, tool_output)
            elif ev.get("kind") == "llm_call":
                payload = ev.get("payload") or {}
                if "response" in payload:
                    self._llm_responses.append(str(payload["response"]))

    def lookup_tool_output(self, tool_name: str, tool_input: dict[str, Any]) -> Any:
        """Return the recorded output for (tool_name, tool_input).

        Raises KeyError when no matching recording exists — callers should
        fall through to real execution in that case.
        """
        key = (tool_name, _input_hash(tool_input))
        if key not in self._tool_index:
            raise KeyError(f"No replay recording for tool '{tool_name}' with given inputs")
        return self._tool_index[key]

    def next_llm_stub(self, model: str) -> str:
        """Return the next recorded LLM response (stub mode).

        Falls back to a canned placeholder when the recording is exhausted.
        """
        if self._llm_cursor < len(self._llm_responses):
            resp = self._llm_responses[self._llm_cursor]
            self._llm_cursor += 1
            return resp
        return f"[replay stub — no more recorded responses for {model}]"

    @property
    def trace_id(self) -> str:
        return str(self.manifest.get("trace_id", ""))

    @property
    def event_count(self) -> int:
        return len(self.events)

    @property
    def total_cost_usd(self) -> float:
        return sum(float(e.get("cost_usd") or 0) for e in self.events)


def _input_hash(tool_input: dict[str, Any]) -> str:
    """Stable hash of tool input for lookup keying."""
    canonical = json.dumps(tool_input, sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(canonical.encode()).hexdigest()[:16]


# ---------------------------------------------------------------------------
# Bundle loading
# ---------------------------------------------------------------------------


def load_bundle(path: str | Path) -> ReplayBundle:
    """Load a replay bundle from a tar.gz file or extracted directory."""
    p = Path(path)
    if p.is_dir():
        return _load_from_dir(p)
    with open(p, "rb") as fh:
        return load_bundle_from_bytes(fh.read())


def load_bundle_from_bytes(data: bytes) -> ReplayBundle:
    """Parse a tar.gz replay bundle from raw bytes."""
    out: dict[str, Any] = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
        for member in tar.getmembers():
            f = tar.extractfile(member)
            if f is None:
                continue
            raw = f.read()
            # Strip the top-level `replay-{trace_id}/` prefix
            name = member.name.split("/", 1)[-1]
            if name == "manifest.json":
                out["manifest"] = json.loads(raw)
            elif name == "events.jsonl":
                out["events"] = [json.loads(line) for line in raw.splitlines() if line]
            elif name == "audit.jsonl":
                out["audit"] = [json.loads(line) for line in raw.splitlines() if line]
            elif name == "agents.json":
                out["agents"] = json.loads(raw)
    return ReplayBundle(
        manifest=out.get("manifest", {}),
        events=out.get("events", []),
        audit=out.get("audit", []),
        agents=out.get("agents", []),
    )


def _load_from_dir(directory: Path) -> ReplayBundle:
    manifest = json.loads((directory / "manifest.json").read_text())
    events_raw = (directory / "events.jsonl").read_text()
    events = [json.loads(line) for line in events_raw.splitlines() if line]
    audit_raw = (directory / "audit.jsonl").read_text()
    audit = [json.loads(line) for line in audit_raw.splitlines() if line]
    agents = json.loads((directory / "agents.json").read_text())
    return ReplayBundle(manifest=manifest, events=events, audit=audit, agents=agents)


# ---------------------------------------------------------------------------
# Active replay session (process-global)
# ---------------------------------------------------------------------------

_active_bundle: ReplayBundle | None = None
_stub_models: bool = False


def activate(bundle: ReplayBundle, *, stub_models: bool = False) -> None:
    global _active_bundle, _stub_models
    _active_bundle = bundle
    _stub_models = stub_models


def deactivate() -> None:
    global _active_bundle, _stub_models
    _active_bundle = None
    _stub_models = False


def get_active() -> ReplayBundle | None:
    return _active_bundle


def is_stub_models() -> bool:
    return _stub_models


# ---------------------------------------------------------------------------
# Auto-activation from env vars (Phase 6.1)
# ---------------------------------------------------------------------------


def _auto_activate() -> None:
    """Called at module import. Activates replay if JAMJET_REPLAY_BUNDLE is set."""
    bundle_path = os.environ.get("JAMJET_REPLAY_BUNDLE")
    if not bundle_path:
        return
    try:
        bundle = load_bundle(bundle_path)
        stub = os.environ.get("JAMJET_STUB_MODELS", "").lower() in ("1", "true", "yes")
        activate(bundle, stub_models=stub)
    except Exception as exc:  # noqa: BLE001
        import warnings

        warnings.warn(
            f"JAMJET_REPLAY_BUNDLE={bundle_path!r} could not be loaded: {exc}",
            stacklevel=2,
        )


_auto_activate()
