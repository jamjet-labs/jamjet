"""Runtime protocol + result/event types. Every backend (Local, Cloud, Java, Rust) implements Runtime."""

from __future__ import annotations

from collections.abc import Callable
from datetime import datetime
from typing import Any, Literal, Protocol, runtime_checkable

from pydantic import BaseModel, ConfigDict, Field

from jamjet.spec import AgentSpec, WorkflowSpec


class StepRecord(BaseModel):
    model_config = ConfigDict(extra="forbid")
    step_id: str
    input_hash: str
    status: Literal["running", "completed", "failed"]
    output_json: str | None = None
    error: str | None = None
    duration_ms: float | None = None


class ToolCallRecord(BaseModel):
    model_config = ConfigDict(extra="forbid")
    tool: str
    input: dict[str, Any]
    output: str
    duration_us: float


class LLMCallRecord(BaseModel):
    model_config = ConfigDict(extra="forbid")
    provider: str
    model: str
    prompt_tokens: int
    completion_tokens: int
    duration_ms: float


class RuntimeEvent(BaseModel):
    model_config = ConfigDict(extra="forbid")
    kind: Literal["step_start", "step_end", "checkpoint", "tool_call", "llm_call", "error"]
    workflow_id: str
    step_id: str | None
    timestamp: datetime
    payload: dict[str, Any] = Field(default_factory=dict)


class RuntimeResult(BaseModel):
    model_config = ConfigDict(extra="forbid", arbitrary_types_allowed=True)
    output: Any
    execution_id: str
    duration_ms: float
    steps: list[StepRecord]
    tool_calls: list[ToolCallRecord]
    llm_calls: list[LLMCallRecord]


class Scope(BaseModel):
    """Scope for memory / agent invocation. Mirrors engram.Scope shape so it bridges directly."""

    model_config = ConfigDict(frozen=True, extra="forbid")
    user_id: str = "default"
    org_id: str = "default"


@runtime_checkable
class Runtime(Protocol):
    name: str
    supported_ir_versions: tuple[str, ...]

    async def execute(
        self,
        spec: AgentSpec | WorkflowSpec,
        input: Any,
        *,
        execution_id: str | None = None,
        scope: Scope | None = None,
        on_event: Callable[[RuntimeEvent], None] | None = None,
    ) -> RuntimeResult: ...

    async def resume(
        self,
        spec: AgentSpec | WorkflowSpec,
        execution_id: str,
        *,
        governance: Any | None = None,
    ) -> RuntimeResult: ...
