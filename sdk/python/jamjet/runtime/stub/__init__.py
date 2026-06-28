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
        *,
        governance: Any | None = None,
    ) -> RuntimeResult:
        raise NotImplementedError(f"{self.name}Runtime.resume lands in Phase 5.")


class CloudRuntime(_StubBase):
    """There is no in-process "cloud" runtime: JamJet Cloud is the GOVERNANCE
    plane (a span/observability API), independent of the execution engine. To run
    on a hosted engine, ship the IR with :meth:`jamjet.Agent.deploy` instead. Both
    methods raise an honest, actionable error rather than a "coming soon" stub.
    """

    name = "Cloud"

    _GUIDANCE = (
        "There is no in-process 'cloud' runtime: JamJet Cloud is the governance "
        "plane, not an execution engine. To run on a hosted engine, use "
        "Agent.deploy(runtime='cloud') (ships the compiled IR to your hosted "
        "jamjet-server at JAMJET_CLOUD_RUNTIME_URL, with JamJet Cloud governance "
        "layered on) or Agent.run_durable(runtime_url=...). For in-process runs "
        "use LocalRuntime."
    )

    async def execute(self, *args: Any, **kwargs: Any) -> RuntimeResult:
        raise NotImplementedError(self._GUIDANCE)

    async def resume(self, *args: Any, **kwargs: Any) -> RuntimeResult:
        raise NotImplementedError(self._GUIDANCE)


class JavaRuntime(_StubBase):
    name = "Java"


class RustRuntime(_StubBase):
    name = "Rust"
