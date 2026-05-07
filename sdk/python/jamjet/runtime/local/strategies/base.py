"""Strategy runner Protocol + shared helpers (tool resolution, tool-call execution)."""
from __future__ import annotations

import importlib
import inspect
import json
import time
from typing import Any, Protocol, runtime_checkable

from jamjet.runtime.local.llm_adapters.base import LLMAdapter
from jamjet.spec import AgentSpec, ToolSpec


@runtime_checkable
class StrategyRunner(Protocol):
    async def __call__(
        self,
        *,
        adapter: LLMAdapter,
        spec: AgentSpec,
        prompt: str,
        tools: list[dict[str, Any]],
        tool_calls_log: list[dict[str, Any]],
    ) -> str: ...


def _resolve_handler(handler_ref: str) -> Any:
    module_path, fn_name = handler_ref.split(":", 1)
    module = importlib.import_module(module_path)
    obj: Any = module
    for part in fn_name.split("."):
        obj = getattr(obj, part)
    return obj


def resolve_tool_map(tools: list[ToolSpec]) -> dict[str, Any]:
    """Map tool name -> callable handler. Resolves handler_ref via importlib."""
    return {t.name: _resolve_handler(t.handler_ref) for t in tools}


async def execute_tool_calls(
    msg: Any,
    tool_map: dict[str, Any],
    tool_calls_log: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    """Execute tool_calls on a model message; return tool result messages."""
    results: list[dict[str, Any]] = []
    for tc in getattr(msg, "tool_calls", None) or []:
        name = tc.function.name
        fn = tool_map.get(name)
        if fn is None:
            result_str = f"Error: unknown tool {name!r}"
            results.append({"role": "tool", "tool_call_id": tc.id, "content": result_str})
            continue
        t0 = time.perf_counter_ns()
        args = json.loads(tc.function.arguments or "{}")
        result = fn(**args)
        if inspect.isawaitable(result):
            result = await result
        duration_us = (time.perf_counter_ns() - t0) / 1000
        result_str = str(result)
        tool_calls_log.append({
            "tool": name,
            "input": args,
            "output": result_str,
            "duration_us": duration_us,
        })
        results.append({"role": "tool", "tool_call_id": tc.id, "content": result_str})
    return results


def _last_assistant_content(messages: list[Any]) -> str:
    for m in reversed(messages):
        content = getattr(m, "content", None) or (m.get("content") if isinstance(m, dict) else None)
        role = getattr(m, "role", None) or (m.get("role") if isinstance(m, dict) else None)
        if role == "assistant" and content:
            return content
    return ""
