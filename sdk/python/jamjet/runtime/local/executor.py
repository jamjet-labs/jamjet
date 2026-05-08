"""LocalRuntime executor — dispatches AgentSpec / DurableAgentSpec / WorkflowSpec."""
from __future__ import annotations

import importlib
import json
import time
import uuid
from collections.abc import Callable
from pathlib import Path
from typing import Any

from jamjet.runtime.local.checkpoint import CheckpointStore
from jamjet.runtime.local.injector import inject_runtime_attributes
from jamjet.runtime.local.llm_adapters import get_adapter
from jamjet.runtime.local.replay import compute_input_hash, derive_step_id
from jamjet.runtime.local.strategies import get_strategy_runner
from jamjet.runtime.types import (
    LLMCallRecord,
    RuntimeEvent,
    RuntimeResult,
    Scope,
    StepRecord,
    ToolCallRecord,
)
from jamjet.spec import AgentSpec, DurableAgentSpec, ToolSpec, WorkflowSpec


class LocalRuntime:
    name = "local"
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
        eid = execution_id or str(uuid.uuid4())
        t0 = time.perf_counter()

        if isinstance(spec, DurableAgentSpec):
            output, steps, tool_calls, llm_calls = await self._run_durable_agent(
                spec, input, eid, scope, on_event,
            )
        elif isinstance(spec, AgentSpec):
            output, steps, tool_calls, llm_calls = await self._run_agent(
                spec, input, eid, on_event,
            )
        elif isinstance(spec, WorkflowSpec):
            output, steps, tool_calls, llm_calls = await self._run_workflow(
                spec, input, eid, on_event,
            )
        else:
            raise TypeError(f"Unsupported spec type: {type(spec).__name__}")

        return RuntimeResult(
            output=output,
            execution_id=eid,
            duration_ms=(time.perf_counter() - t0) * 1000,
            steps=steps,
            tool_calls=tool_calls,
            llm_calls=llm_calls,
        )

    async def resume(
        self, spec: AgentSpec | WorkflowSpec, execution_id: str,
    ) -> RuntimeResult:
        return await self.execute(spec, input=None, execution_id=execution_id)

    async def _run_agent(
        self, spec: AgentSpec, input: Any, eid: str, on_event: Any,
    ) -> tuple[Any, list[StepRecord], list[ToolCallRecord], list[LLMCallRecord]]:
        adapter = get_adapter(spec.llm)
        runner = get_strategy_runner(spec.strategy.name)
        tool_calls_log: list[dict[str, Any]] = []
        openai_tools = [self._tool_to_openai_schema(t) for t in spec.tools]
        prompt = input if isinstance(input, str) else json.dumps(input)
        output = await runner(
            adapter=adapter, spec=spec, prompt=prompt,
            tools=openai_tools, tool_calls_log=tool_calls_log,
        )
        tool_calls = [
            ToolCallRecord(
                tool=t["tool"], input=t["input"], output=t["output"],
                duration_us=t["duration_us"],
            )
            for t in tool_calls_log
        ]
        return output, [], tool_calls, []

    async def _run_durable_agent(
        self, spec: DurableAgentSpec, input: Any, eid: str, scope: Scope | None, on_event: Any,
    ) -> tuple[Any, list[StepRecord], list[ToolCallRecord], list[LLMCallRecord]]:
        module_path, cls_name = spec.class_ref.split(":", 1)
        module = importlib.import_module(module_path)
        cls: Any = module
        for part in cls_name.split("."):
            cls = getattr(cls, part)

        if spec.durability.db_path:
            db_path = Path(spec.durability.db_path)
        else:
            db_path = Path.home() / ".jamjet" / "durable" / f"{eid}.db"
        store = CheckpointStore(db_path, ir_version=spec.ir_version, spec_hash="todo")
        await store.init()

        instance = cls()
        from engram import Scope as EngramScope
        engram_scope: EngramScope | None = None
        if scope is not None:
            engram_scope = EngramScope(user_id=scope.user_id, org_id=scope.org_id)
        await inject_runtime_attributes(instance, spec=spec, execution_id=eid, scope=engram_scope)

        entry = next((m.name for m in spec.methods if m.is_entrypoint), None)
        if entry is None:
            raise RuntimeError(f"No entrypoint defined on {spec.name}")
        method = getattr(instance, entry)

        step_id = derive_step_id(parent_step_id=None, call_site=f"{spec.name}.{entry}", invocation_index=0)

        # On resume (input is None), check for an already-completed step regardless of input hash.
        if input is None:
            existing = await store.get_step(step_id)
            if existing is not None and existing.status == "completed" and existing.output_json is not None:
                output = json.loads(existing.output_json)
                engram = getattr(instance, "_jamjet_engram", None)
                if engram is not None:
                    await engram.close()
                return output, [existing], [], []

        input_hash = compute_input_hash({"input": input})
        existing = await store.get_step_if_match(step_id, input_hash=input_hash)
        if existing is not None and existing.output_json is not None:
            output = json.loads(existing.output_json)
            engram = getattr(instance, "_jamjet_engram", None)
            if engram is not None:
                await engram.close()
            return output, [existing], [], []

        await store.start_step(step_id, input_hash=input_hash, input_json=json.dumps({"input": input}, default=str))
        t0 = time.perf_counter()
        try:
            output = await method(input) if input is not None else await method()
        except Exception as exc:
            await store.fail_step(step_id, error=str(exc))
            engram = getattr(instance, "_jamjet_engram", None)
            if engram is not None:
                await engram.close()
            raise
        duration_ms = (time.perf_counter() - t0) * 1000
        await store.complete_step(
            step_id,
            output_json=json.dumps(output, default=str),
            duration_ms=duration_ms,
        )

        engram = getattr(instance, "_jamjet_engram", None)
        if engram is not None:
            await engram.close()

        record = StepRecord(
            step_id=step_id, input_hash=input_hash,
            status="completed", output_json=json.dumps(output, default=str),
            duration_ms=duration_ms,
        )
        return output, [record], [], []

    async def _run_workflow(
        self, spec: WorkflowSpec, input: Any, eid: str, on_event: Any,
    ) -> tuple[Any, list[StepRecord], list[ToolCallRecord], list[LLMCallRecord]]:
        if len(spec.nodes) == 1:
            node = spec.nodes[0]
            module_path, fn_name = node.handler_ref.split(":", 1)
            module = importlib.import_module(module_path)
            fn: Any = module
            for part in fn_name.split("."):
                fn = getattr(fn, part)
            output = await fn(input) if input is not None else await fn()
            return output, [], [], []
        raise NotImplementedError(
            "Multi-node WorkflowSpec execution stays in jamjet.workflow.executor; "
            "this LocalRuntime path handles single-node @workflow only."
        )

    @staticmethod
    def _tool_to_openai_schema(t: ToolSpec) -> dict[str, Any]:
        return {
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            },
        }
