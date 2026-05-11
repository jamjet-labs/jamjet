"""Phase 5+ runtime stubs. Each raises NotImplementedError until that phase ships."""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from jamjet.runtime.types import RuntimeEvent, RuntimeResult, Scope
from jamjet.spec import AgentSpec, WorkflowSpec


class _StubBase:
    name: str = ""
    supported_ir_versions: tuple[str, ...] = ("1.0",)

    async def execute(
        self,
        spec: AgentSpec | WorkflowSpec,
        input: Any,
        *,
        execution_id: str | None = None,
        scope: Scope | None = None,
        on_event: Callable[[RuntimeEvent], None] | None = None,
    ) -> RuntimeResult:
        raise NotImplementedError(
            f"{self.name}Runtime not implemented yet; lands in Phase 5. Use LocalRuntime for now."
        )

    async def resume(
        self,
        spec: AgentSpec | WorkflowSpec,
        execution_id: str,
    ) -> RuntimeResult:
        raise NotImplementedError(f"{self.name}Runtime.resume lands in Phase 5.")


class CloudRuntime(_StubBase):
    name = "Cloud"


class JavaRuntime(_StubBase):
    name = "Java"


class RustRuntime(_StubBase):
    name = "Rust"
