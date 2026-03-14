"""Tests for the ProtocolAdapter ABC, types, and ProtocolRegistry."""

from __future__ import annotations

from collections.abc import AsyncIterator

import pytest

from jamjet.protocols.adapter import (
    ProtocolAdapter,
    RemoteCapabilities,
    RemoteSkill,
    StreamChunk,
    TaskEvent,
    TaskHandle,
    TaskRequest,
    TaskStatus,
    _event_to_chunk,
)
from jamjet.protocols.registry import ProtocolRegistry, get_registry

# ── Fake adapter for testing ────────────────────────────────────────────────


class FakeAdapter(ProtocolAdapter):
    """Minimal concrete adapter for testing."""

    def __init__(self, name: str = "fake") -> None:
        self._name = name

    async def discover(self, url: str) -> RemoteCapabilities:
        return RemoteCapabilities(
            name=self._name,
            description=f"Fake adapter at {url}",
            skills=[RemoteSkill(name="echo")],
            protocols=[self._name],
        )

    async def invoke(self, url: str, task: TaskRequest) -> TaskHandle:
        return TaskHandle(task_id="task-001", remote_url=url)

    async def stream(self, url: str, task: TaskRequest) -> AsyncIterator[TaskEvent]:
        yield TaskEvent.progress_event("working", 0.5)
        yield TaskEvent.completed_event({"result": "done"})

    async def status(self, url: str, task_id: str) -> TaskStatus:
        return TaskStatus.completed({"result": "done"})

    async def cancel(self, url: str, task_id: str) -> None:
        pass


# ── ABC enforcement ─────────────────────────────────────────────────────────


class IncompleteAdapter(ProtocolAdapter):
    """Missing all abstract methods — should fail to instantiate."""

    pass


def test_abc_enforcement():
    with pytest.raises(TypeError):
        IncompleteAdapter()  # type: ignore[abstract]


class MissingOneMethod(ProtocolAdapter):
    """Missing only cancel."""

    async def discover(self, url: str) -> RemoteCapabilities:
        return RemoteCapabilities(name="x")

    async def invoke(self, url: str, task: TaskRequest) -> TaskHandle:
        return TaskHandle(task_id="x", remote_url=url)

    async def stream(self, url: str, task: TaskRequest) -> AsyncIterator[TaskEvent]:
        yield TaskEvent.completed_event(None)

    async def status(self, url: str, task_id: str) -> TaskStatus:
        return TaskStatus.submitted()


def test_abc_enforcement_missing_one():
    with pytest.raises(TypeError):
        MissingOneMethod()  # type: ignore[abstract]


# ── Subclassing works ───────────────────────────────────────────────────────


def test_subclass_instantiation():
    adapter = FakeAdapter("test")
    assert isinstance(adapter, ProtocolAdapter)


# ── Type construction ───────────────────────────────────────────────────────


def test_task_request():
    req = TaskRequest(skill="summarize", input={"text": "hello"}, stream=True)
    d = req.to_dict()
    assert d["skill"] == "summarize"
    assert d["stream"] is True
    assert "timeout_secs" not in d


def test_task_request_with_timeout():
    req = TaskRequest(skill="x", input={}, timeout_secs=30)
    d = req.to_dict()
    assert d["timeout_secs"] == 30


def test_task_handle():
    h = TaskHandle(task_id="abc", remote_url="http://example.com")
    d = h.to_dict()
    assert d == {"task_id": "abc", "remote_url": "http://example.com"}


def test_remote_skill():
    s = RemoteSkill(name="echo", description="Echo input")
    d = s.to_dict()
    assert d["name"] == "echo"
    assert d["description"] == "Echo input"


def test_remote_capabilities():
    caps = RemoteCapabilities(
        name="agent-1",
        skills=[RemoteSkill(name="search")],
        protocols=["mcp", "a2a"],
    )
    d = caps.to_dict()
    assert d["name"] == "agent-1"
    assert len(d["skills"]) == 1
    assert d["protocols"] == ["mcp", "a2a"]


# ── TaskEvent factories ─────────────────────────────────────────────────────


def test_task_event_progress():
    e = TaskEvent.progress_event("step 1", 0.25)
    assert e.type == "progress"
    assert e.message == "step 1"
    assert e.progress == 0.25
    d = e.to_dict()
    assert d["type"] == "progress"
    assert "error" not in d


def test_task_event_completed():
    e = TaskEvent.completed_event({"answer": 42})
    assert e.type == "completed"
    assert e.output == {"answer": 42}


def test_task_event_failed():
    e = TaskEvent.failed_event("timeout")
    assert e.type == "failed"
    assert e.error == "timeout"


def test_task_event_artifact():
    e = TaskEvent.artifact_event("report.pdf", b"bytes")
    assert e.type == "artifact"
    assert e.name == "report.pdf"


def test_task_event_input_required():
    e = TaskEvent.input_required_event("Enter approval code")
    assert e.type == "input_required"
    assert e.prompt == "Enter approval code"


# ── TaskStatus factories ────────────────────────────────────────────────────


def test_task_status_submitted():
    s = TaskStatus.submitted()
    assert s.status == "submitted"
    d = s.to_dict()
    assert d == {"status": "submitted"}


def test_task_status_completed():
    s = TaskStatus.completed({"ok": True})
    assert s.status == "completed"
    assert s.output == {"ok": True}
    d = s.to_dict()
    assert d["output"] == {"ok": True}


def test_task_status_failed():
    s = TaskStatus.failed("boom")
    assert s.status == "failed"
    d = s.to_dict()
    assert d["error"] == "boom"


def test_task_status_cancelled():
    s = TaskStatus.cancelled()
    assert s.status == "cancelled"


# ── StreamChunk factories ───────────────────────────────────────────────────


def test_stream_chunk_text_delta():
    c = StreamChunk.text_delta("hello ")
    assert c.type == "text_delta"
    assert c.delta == "hello "


def test_stream_chunk_tool_call():
    c = StreamChunk.tool_call("search", {"q": "test"})
    assert c.type == "tool_call"
    assert c.name == "search"


def test_stream_chunk_progress():
    c = StreamChunk.progress_chunk("loading", 0.75)
    assert c.type == "progress"
    assert c.fraction == 0.75


def test_stream_chunk_artifact():
    c = StreamChunk.artifact_chunk("file.csv", "data", mime_type="text/csv")
    assert c.type == "artifact"
    assert c.mime_type == "text/csv"


def test_stream_chunk_final():
    c = StreamChunk.final_chunk({"result": "ok"})
    assert c.type == "final"
    assert c.output == {"result": "ok"}


def test_stream_chunk_error():
    c = StreamChunk.error_chunk("something broke")
    assert c.type == "error"
    assert c.message == "something broke"


def test_stream_chunk_to_dict_omits_none():
    c = StreamChunk.text_delta("x")
    d = c.to_dict()
    assert d == {"type": "text_delta", "delta": "x"}
    assert "name" not in d
    assert "output" not in d


# ── _event_to_chunk mapping ─────────────────────────────────────────────────


def test_event_to_chunk_progress():
    c = _event_to_chunk(TaskEvent.progress_event("go", 0.5))
    assert c.type == "progress"
    assert c.message == "go"
    assert c.fraction == 0.5


def test_event_to_chunk_artifact():
    c = _event_to_chunk(TaskEvent.artifact_event("f", {"k": "v"}))
    assert c.type == "artifact"
    assert c.name == "f"


def test_event_to_chunk_completed():
    c = _event_to_chunk(TaskEvent.completed_event({"ok": True}))
    assert c.type == "final"
    assert c.output == {"ok": True}


def test_event_to_chunk_failed():
    c = _event_to_chunk(TaskEvent.failed_event("err"))
    assert c.type == "error"
    assert c.message == "err"


def test_event_to_chunk_input_required():
    c = _event_to_chunk(TaskEvent.input_required_event("approve?"))
    assert c.type == "progress"
    assert "input required" in (c.message or "")


# ── Adapter discover/invoke/status/cancel ────────────────────────────────────


@pytest.mark.asyncio
async def test_adapter_discover():
    adapter = FakeAdapter("myproto")
    caps = await adapter.discover("http://example.com")
    assert caps.name == "myproto"
    assert caps.protocols == ["myproto"]
    assert len(caps.skills) == 1


@pytest.mark.asyncio
async def test_adapter_invoke():
    adapter = FakeAdapter()
    handle = await adapter.invoke("http://x", TaskRequest(skill="echo", input={}))
    assert handle.task_id == "task-001"


@pytest.mark.asyncio
async def test_adapter_stream():
    adapter = FakeAdapter()
    events = []
    async for event in adapter.stream("http://x", TaskRequest(skill="x", input={})):
        events.append(event)
    assert len(events) == 2
    assert events[0].type == "progress"
    assert events[1].type == "completed"


@pytest.mark.asyncio
async def test_adapter_status():
    adapter = FakeAdapter()
    s = await adapter.status("http://x", "task-001")
    assert s.status == "completed"


@pytest.mark.asyncio
async def test_adapter_cancel():
    adapter = FakeAdapter()
    await adapter.cancel("http://x", "task-001")  # should not raise


# ── Default stream_structured ────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_default_stream_structured():
    adapter = FakeAdapter()
    chunks = []
    async for chunk in adapter.stream_structured("http://x", TaskRequest(skill="x", input={})):
        chunks.append(chunk)
    assert len(chunks) == 2
    assert chunks[0].type == "progress"
    assert chunks[1].type == "final"


@pytest.mark.asyncio
async def test_default_stream_with_backpressure():
    adapter = FakeAdapter()
    chunks = []
    async for chunk in adapter.stream_with_backpressure("http://x", TaskRequest(skill="x", input={}), buffer_size=4):
        chunks.append(chunk)
    assert len(chunks) == 2


# ── ProtocolRegistry ─────────────────────────────────────────────────────────


def test_registry_register_and_lookup():
    reg = ProtocolRegistry()
    reg.register("mcp", FakeAdapter("mcp"), url_prefixes=["http://mcp/"])
    assert reg.adapter("mcp") is not None
    assert reg.adapter("a2a") is None


def test_registry_adapter_for_url():
    reg = ProtocolRegistry()
    reg.register("anp", FakeAdapter("anp"), url_prefixes=["did:"])
    reg.register("mcp", FakeAdapter("mcp"), url_prefixes=["http://mcp."])
    assert reg.adapter_for_url("did:web:example.com") is not None
    assert reg.adapter_for_url("http://mcp.example.com/tools") is not None
    assert reg.adapter_for_url("https://unknown.com") is None


def test_registry_longest_prefix_wins():
    reg = ProtocolRegistry()
    generic = FakeAdapter("generic")
    specific = FakeAdapter("specific")
    reg.register("generic-http", generic, url_prefixes=["http://"])
    reg.register("specific-mcp", specific, url_prefixes=["http://mcp.example.com/"])

    matched = reg.adapter_for_url("http://mcp.example.com/v1")
    assert matched is specific


def test_registry_protocols_list():
    reg = ProtocolRegistry()
    reg.register("mcp", FakeAdapter("mcp"))
    reg.register("a2a", FakeAdapter("a2a"))
    protos = sorted(reg.protocols())
    assert protos == ["a2a", "mcp"]


def test_registry_no_url_prefixes():
    reg = ProtocolRegistry()
    reg.register("custom", FakeAdapter("custom"))
    assert reg.adapter("custom") is not None
    assert reg.adapter_for_url("http://anything") is None


def test_registry_repr():
    reg = ProtocolRegistry()
    reg.register("mcp", FakeAdapter("mcp"))
    assert "mcp" in repr(reg)


# ── get_registry singleton ───────────────────────────────────────────────────


def test_get_registry_returns_same_instance():
    r1 = get_registry()
    r2 = get_registry()
    assert r1 is r2


# ── Top-level imports ────────────────────────────────────────────────────────


def test_import_from_jamjet():
    from jamjet import ProtocolAdapter, ProtocolRegistry

    assert ProtocolAdapter is not None
    assert ProtocolRegistry is not None


def test_import_from_jamjet_protocols():
    from jamjet.protocols import (
        ProtocolAdapter,
        ProtocolRegistry,
        RemoteCapabilities,
        RemoteSkill,
        StreamChunk,
        TaskEvent,
        TaskHandle,
        TaskRequest,
        TaskStatus,
        get_registry,
        serve_tools,
    )

    assert all(
        x is not None
        for x in [
            ProtocolAdapter,
            ProtocolRegistry,
            RemoteCapabilities,
            RemoteSkill,
            StreamChunk,
            TaskEvent,
            TaskHandle,
            TaskRequest,
            TaskStatus,
            get_registry,
            serve_tools,
        ]
    )
