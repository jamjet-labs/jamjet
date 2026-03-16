"""
Protocol adapter base class and types.

Mirrors the Rust ``ProtocolAdapter`` trait in ``runtime/protocols/src/lib.rs``.
Subclass ``ProtocolAdapter`` and implement the five abstract methods to add a
new protocol to JamJet (e.g. LDP, FlowScript, custom gRPC services).
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from typing import Any

# ── Data types ───────────────────────────────────────────────────────────────


@dataclass
class TaskRequest:
    """A task request to a remote agent or tool provider."""

    skill: str
    input: Any
    timeout_secs: int | None = None
    stream: bool = False
    metadata: Any = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"skill": self.skill, "input": self.input, "stream": self.stream}
        if self.timeout_secs is not None:
            d["timeout_secs"] = self.timeout_secs
        if self.metadata:
            d["metadata"] = self.metadata
        return d


@dataclass
class TaskHandle:
    """A handle to a submitted task."""

    task_id: str
    remote_url: str

    def to_dict(self) -> dict[str, Any]:
        return {"task_id": self.task_id, "remote_url": self.remote_url}


@dataclass
class RemoteSkill:
    """A skill offered by a remote agent or tool provider."""

    name: str
    description: str | None = None
    input_schema: Any | None = None
    output_schema: Any | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"name": self.name}
        if self.description is not None:
            d["description"] = self.description
        if self.input_schema is not None:
            d["input_schema"] = self.input_schema
        if self.output_schema is not None:
            d["output_schema"] = self.output_schema
        return d


@dataclass
class RemoteCapabilities:
    """Capabilities discovered from a remote agent or tool provider."""

    name: str
    description: str | None = None
    skills: list[RemoteSkill] = field(default_factory=list)
    protocols: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "description": self.description,
            "skills": [s.to_dict() for s in self.skills],
            "protocols": list(self.protocols),
        }


@dataclass
class TaskEvent:
    """An event streamed from a running task.

    Discriminated by ``type``.  Use the factory class methods to create
    well-formed events.
    """

    type: str  # progress | artifact | input_required | completed | failed
    message: str | None = None
    progress: float | None = None
    name: str | None = None
    data: Any | None = None
    prompt: str | None = None
    output: Any | None = None
    error: str | None = None

    # ── Factories ──

    @classmethod
    def progress_event(cls, message: str, progress: float | None = None) -> TaskEvent:
        return cls(type="progress", message=message, progress=progress)

    @classmethod
    def artifact_event(cls, name: str, data: Any) -> TaskEvent:
        return cls(type="artifact", name=name, data=data)

    @classmethod
    def input_required_event(cls, prompt: str) -> TaskEvent:
        return cls(type="input_required", prompt=prompt)

    @classmethod
    def completed_event(cls, output: Any) -> TaskEvent:
        return cls(type="completed", output=output)

    @classmethod
    def failed_event(cls, error: str) -> TaskEvent:
        return cls(type="failed", error=error)

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"type": self.type}
        for k in ("message", "progress", "name", "data", "prompt", "output", "error"):
            v = getattr(self, k)
            if v is not None:
                d[k] = v
        return d


@dataclass
class TaskStatus:
    """Current status of a remote task.

    Discriminated by ``status``.  Use the factory class methods for convenience.
    """

    status: str  # submitted | working | input_required | completed | failed | cancelled
    output: Any | None = None
    error: str | None = None

    # ── Factories ──

    @classmethod
    def submitted(cls) -> TaskStatus:
        return cls(status="submitted")

    @classmethod
    def working(cls) -> TaskStatus:
        return cls(status="working")

    @classmethod
    def completed(cls, output: Any) -> TaskStatus:
        return cls(status="completed", output=output)

    @classmethod
    def failed(cls, error: str) -> TaskStatus:
        return cls(status="failed", error=error)

    @classmethod
    def cancelled(cls) -> TaskStatus:
        return cls(status="cancelled")

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"status": self.status}
        if self.output is not None:
            d["output"] = self.output
        if self.error is not None:
            d["error"] = self.error
        return d


@dataclass
class StreamChunk:
    """A typed stream chunk for structured streaming.

    Follows the OTel GenAI structured streaming convention.
    Discriminated by ``type``.
    """

    type: str  # text_delta | tool_call | progress | artifact | final | error
    delta: str | None = None
    name: str | None = None
    arguments: Any | None = None
    message: str | None = None
    fraction: float | None = None
    data: Any | None = None
    mime_type: str | None = None
    output: Any | None = None

    # ── Factories ──

    @classmethod
    def text_delta(cls, delta: str) -> StreamChunk:
        return cls(type="text_delta", delta=delta)

    @classmethod
    def tool_call(cls, name: str, arguments: Any) -> StreamChunk:
        return cls(type="tool_call", name=name, arguments=arguments)

    @classmethod
    def progress_chunk(cls, message: str, fraction: float | None = None) -> StreamChunk:
        return cls(type="progress", message=message, fraction=fraction)

    @classmethod
    def artifact_chunk(cls, name: str, data: Any, mime_type: str | None = None) -> StreamChunk:
        return cls(type="artifact", name=name, data=data, mime_type=mime_type)

    @classmethod
    def final_chunk(cls, output: Any) -> StreamChunk:
        return cls(type="final", output=output)

    @classmethod
    def error_chunk(cls, message: str) -> StreamChunk:
        return cls(type="error", message=message)

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"type": self.type}
        for k in (
            "delta",
            "name",
            "arguments",
            "message",
            "fraction",
            "data",
            "mime_type",
            "output",
        ):
            v = getattr(self, k)
            if v is not None:
                d[k] = v
        return d


# ── Helpers ──────────────────────────────────────────────────────────────────


def _event_to_chunk(event: TaskEvent) -> StreamChunk:
    """Map a ``TaskEvent`` to a ``StreamChunk`` (mirrors the Rust default)."""
    t = event.type
    if t == "progress":
        return StreamChunk.progress_chunk(event.message or "", event.progress)
    if t == "artifact":
        return StreamChunk.artifact_chunk(event.name or "", event.data)
    if t == "completed":
        return StreamChunk.final_chunk(event.output)
    if t == "failed":
        return StreamChunk.error_chunk(event.error or "unknown error")
    if t == "input_required":
        return StreamChunk.progress_chunk(f"input required: {event.prompt or ''}")
    return StreamChunk.progress_chunk(f"unknown event: {t}")


# ── Abstract base class ─────────────────────────────────────────────────────


class ProtocolAdapter(ABC):
    """Base class for protocol adapters.

    Implement all five abstract methods to add a new protocol to JamJet.

    Example::

        class LdpAdapter(ProtocolAdapter):
            async def discover(self, url: str) -> RemoteCapabilities: ...
            async def invoke(self, url: str, task: TaskRequest) -> TaskHandle: ...
            async def stream(self, url: str, task: TaskRequest) -> AsyncIterator[TaskEvent]: ...
            async def status(self, url: str, task_id: str) -> TaskStatus: ...
            async def cancel(self, url: str, task_id: str) -> None: ...
    """

    @abstractmethod
    async def discover(self, url: str) -> RemoteCapabilities:
        """Discover remote capabilities (fetch Agent Card or equivalent)."""

    @abstractmethod
    async def invoke(self, url: str, task: TaskRequest) -> TaskHandle:
        """Submit a task/request to the remote."""

    @abstractmethod
    async def stream(self, url: str, task: TaskRequest) -> AsyncIterator[TaskEvent]:
        """Stream task progress events."""

    @abstractmethod
    async def status(self, url: str, task_id: str) -> TaskStatus:
        """Poll task status by task_id."""

    @abstractmethod
    async def cancel(self, url: str, task_id: str) -> None:
        """Cancel a running task."""

    async def stream_structured(self, url: str, task: TaskRequest) -> AsyncIterator[StreamChunk]:
        """Stream with typed chunks.  Default: wraps ``stream()``."""
        async for event in await self.stream(url, task):
            yield _event_to_chunk(event)

    async def stream_with_backpressure(
        self,
        url: str,
        task: TaskRequest,
        buffer_size: int = 16,
    ) -> AsyncIterator[StreamChunk]:
        """Stream with bounded buffer.  Default: wraps ``stream_structured()``."""
        async for chunk in self.stream_structured(url, task):
            yield chunk
