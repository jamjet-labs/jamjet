"""Unit tests for Plan 5 Phase 6.1/6.3 — replay bundle + SDK interceptor."""

from __future__ import annotations

import io
import json
import tarfile
import tempfile
from pathlib import Path
from typing import Any

import pytest

from jamjet.cloud.replay import (
    activate,
    deactivate,
    get_active,
    is_stub_models,
    load_bundle,
    load_bundle_from_bytes,
)
from jamjet.tools.decorators import tool

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_bundle_bytes(
    manifest: dict[str, Any] | None = None,
    events: list[dict[str, Any]] | None = None,
    audit: list[dict[str, Any]] | None = None,
    agents: list[dict[str, Any]] | None = None,
) -> bytes:
    """Build a minimal tar.gz replay bundle for testing."""
    manifest = manifest or {
        "schema_version": "replay-1.0",
        "trace_id": "tr_test",
        "project_id": "00000000-0000-0000-0000-000000000001",
        "event_count": len(events or []),
        "originating_trace_id": None,
    }
    events_jsonl = b"\n".join(json.dumps(e).encode() for e in (events or []))
    audit_jsonl = b"\n".join(json.dumps(a).encode() for a in (audit or []))
    agents_json = json.dumps(agents or []).encode()
    manifest_json = json.dumps(manifest).encode()

    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        for name, data in [
            ("replay-tr_test/manifest.json", manifest_json),
            ("replay-tr_test/events.jsonl", events_jsonl),
            ("replay-tr_test/audit.jsonl", audit_jsonl),
            ("replay-tr_test/agents.json", agents_json),
        ]:
            info = tarfile.TarInfo(name=name)
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
    return buf.getvalue()


# ---------------------------------------------------------------------------
# Bundle parsing
# ---------------------------------------------------------------------------


def test_load_bundle_from_bytes_parses_all_sections() -> None:
    events = [
        {
            "kind": "tool_call",
            "sequence": 0,
            "name": "send_email",
            "payload": {"tool": "send_email", "tool_input": {"to": "a@b.com"}, "tool_output": {"status": "sent"}},
        },
        {
            "kind": "llm_call",
            "sequence": 1,
            "name": "openai.gpt-4o",
            "payload": {"model": "gpt-4o", "response": "Hello from recording"},
        },
    ]
    audit = [{"action": "tool_blocked", "detail": {"tool": "send_email"}}]
    agents = [{"id": "uuid1", "name": "worker"}]

    bundle = load_bundle_from_bytes(_make_bundle_bytes(events=events, audit=audit, agents=agents))

    assert bundle.manifest["trace_id"] == "tr_test"
    assert len(bundle.events) == 2
    assert len(bundle.audit) == 1
    assert bundle.agents[0]["name"] == "worker"


def test_total_cost_sums_event_costs() -> None:
    events = [
        {"kind": "llm_call", "cost_usd": 0.01, "payload": {}},
        {"kind": "llm_call", "cost_usd": 0.02, "payload": {}},
        {"kind": "tool_call", "payload": {}},
    ]
    bundle = load_bundle_from_bytes(_make_bundle_bytes(events=events))
    assert abs(bundle.total_cost_usd - 0.03) < 1e-9


def test_load_bundle_from_directory() -> None:
    events = [
        {
            "kind": "tool_call",
            "sequence": 0,
            "payload": {"tool": "greet", "tool_input": {"name": "world"}, "tool_output": "hi world"},
        }
    ]
    with tempfile.TemporaryDirectory() as tmpdir:
        d = Path(tmpdir)
        (d / "manifest.json").write_text(
            json.dumps({"trace_id": "tr_dir", "schema_version": "replay-1.0", "event_count": 1})
        )
        (d / "events.jsonl").write_text(json.dumps(events[0]))
        (d / "audit.jsonl").write_text("")
        (d / "agents.json").write_text("[]")
        bundle = load_bundle(d)
    assert bundle.trace_id == "tr_dir"
    assert len(bundle.events) == 1


# ---------------------------------------------------------------------------
# Tool output lookup
# ---------------------------------------------------------------------------


def test_lookup_tool_output_returns_recorded_value() -> None:
    events = [{"kind": "tool_call", "payload": {"tool": "add", "tool_input": {"a": 1, "b": 2}, "tool_output": 3}}]
    bundle = load_bundle_from_bytes(_make_bundle_bytes(events=events))
    assert bundle.lookup_tool_output("add", {"a": 1, "b": 2}) == 3


def test_lookup_tool_output_raises_on_missing() -> None:
    bundle = load_bundle_from_bytes(_make_bundle_bytes(events=[]))
    with pytest.raises(KeyError, match="add"):
        bundle.lookup_tool_output("add", {"x": 1})


def test_lookup_tool_output_first_occurrence_wins() -> None:
    events = [
        {"kind": "tool_call", "payload": {"tool": "fn", "tool_input": {"k": "v"}, "tool_output": "first"}},
        {"kind": "tool_call", "payload": {"tool": "fn", "tool_input": {"k": "v"}, "tool_output": "second"}},
    ]
    bundle = load_bundle_from_bytes(_make_bundle_bytes(events=events))
    assert bundle.lookup_tool_output("fn", {"k": "v"}) == "first"


# ---------------------------------------------------------------------------
# LLM stub
# ---------------------------------------------------------------------------


def test_next_llm_stub_returns_recorded_responses_in_order() -> None:
    events = [
        {"kind": "llm_call", "payload": {"model": "gpt-4o", "response": "first"}},
        {"kind": "llm_call", "payload": {"model": "gpt-4o", "response": "second"}},
    ]
    bundle = load_bundle_from_bytes(_make_bundle_bytes(events=events))
    assert bundle.next_llm_stub("gpt-4o") == "first"
    assert bundle.next_llm_stub("gpt-4o") == "second"


def test_next_llm_stub_falls_back_when_exhausted() -> None:
    bundle = load_bundle_from_bytes(_make_bundle_bytes(events=[]))
    result = bundle.next_llm_stub("gpt-4o")
    assert "replay stub" in result


# ---------------------------------------------------------------------------
# Active session + auto-activation
# ---------------------------------------------------------------------------


def test_activate_deactivate_lifecycle() -> None:
    bundle = load_bundle_from_bytes(_make_bundle_bytes())
    activate(bundle, stub_models=True)
    assert get_active() is bundle
    assert is_stub_models() is True
    deactivate()
    assert get_active() is None
    assert is_stub_models() is False


def test_auto_activate_from_env_var(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    events = [{"kind": "tool_call", "payload": {"tool": "ping", "tool_input": {}, "tool_output": "pong"}}]
    # Write extracted dir
    (tmp_path / "manifest.json").write_text(
        json.dumps({"trace_id": "tr_env", "schema_version": "replay-1.0", "event_count": 1})
    )
    (tmp_path / "events.jsonl").write_text(json.dumps(events[0]))
    (tmp_path / "audit.jsonl").write_text("")
    (tmp_path / "agents.json").write_text("[]")

    monkeypatch.setenv("JAMJET_REPLAY_BUNDLE", str(tmp_path))
    monkeypatch.setenv("JAMJET_STUB_MODELS", "1")

    # Re-run auto-activation manually (module was already imported)
    import jamjet.cloud.replay as _replay_mod

    _replay_mod.deactivate()
    _replay_mod._auto_activate()

    assert _replay_mod.get_active() is not None
    assert _replay_mod.is_stub_models() is True

    _replay_mod.deactivate()


# ---------------------------------------------------------------------------
# @tool replay intercept integration
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_tool_decorator_uses_replay_when_bundle_active() -> None:
    call_count = 0

    @tool
    async def double(x: int) -> int:
        nonlocal call_count
        call_count += 1
        return x * 2

    events = [{"kind": "tool_call", "payload": {"tool": "double", "tool_input": {"x": 5}, "tool_output": 99}}]
    bundle = load_bundle_from_bytes(_make_bundle_bytes(events=events))
    activate(bundle)
    try:
        result = await double(x=5)
        assert result == 99
        assert call_count == 0  # real fn never called
    finally:
        deactivate()


@pytest.mark.asyncio
async def test_tool_decorator_falls_through_when_no_recording() -> None:
    @tool
    async def triple(x: int) -> int:
        return x * 3

    # Bundle is active but has no recording for 'triple'
    bundle = load_bundle_from_bytes(_make_bundle_bytes(events=[]))
    activate(bundle)
    try:
        result = await triple(x=4)
        assert result == 12  # real fn called
    finally:
        deactivate()


@pytest.mark.asyncio
async def test_tool_decorator_calls_real_fn_when_no_bundle() -> None:
    deactivate()

    @tool
    async def add_one(n: int) -> int:
        return n + 1

    assert await add_one(n=10) == 11


# ---------------------------------------------------------------------------
# Local capture (capture_io=True)
# ---------------------------------------------------------------------------


def test_local_capture_writes_jsonl_when_enabled(tmp_path: Path) -> None:
    from jamjet.cloud.config import set_config
    from jamjet.cloud.events import emit, set_capture_path

    capture_file = tmp_path / "replay.jsonl"
    set_capture_path(str(capture_file))
    set_config(capture_io=True)
    try:
        emit({"kind": "tool_call", "name": "test_tool", "trace_id": "tr_x"})
        emit({"kind": "llm_call", "name": "gpt-4o", "trace_id": "tr_x"})
        lines = capture_file.read_text().strip().splitlines()
        assert len(lines) == 2
        assert json.loads(lines[0])["kind"] == "tool_call"
        assert json.loads(lines[1])["kind"] == "llm_call"
    finally:
        set_config(capture_io=False)
        set_capture_path(".jamjet-replay.jsonl")


def test_local_capture_no_op_when_disabled(tmp_path: Path) -> None:
    from jamjet.cloud.config import set_config
    from jamjet.cloud.events import emit, set_capture_path

    capture_file = tmp_path / "replay.jsonl"
    set_capture_path(str(capture_file))
    set_config(capture_io=False)
    emit({"kind": "tool_call", "name": "x"})
    assert not capture_file.exists()
