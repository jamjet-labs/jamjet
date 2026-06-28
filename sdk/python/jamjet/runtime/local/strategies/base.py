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
        initial_messages: list[dict[str, Any]] | None = None,
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
        tool_calls_log.append(
            {
                "tool": name,
                "input": args,
                "output": result_str,
                "duration_us": duration_us,
            }
        )
        results.append({"role": "tool", "tool_call_id": tc.id, "content": result_str})
    return results


def _last_assistant_content(messages: list[Any]) -> str:
    for m in reversed(messages):
        content = getattr(m, "content", None) or (m.get("content") if isinstance(m, dict) else None)
        role = getattr(m, "role", None) or (m.get("role") if isinstance(m, dict) else None)
        if role == "assistant" and content:
            return str(content)
    return ""


def seed_history(
    initial_messages: list[dict[str, Any]] | None,
    system: str,
) -> list[dict[str, Any]]:
    """Leading messages a strategy's GENERATIVE phase should build on.

    Session / memory continuity (C1).  When *initial_messages* is provided it is
    the carried thread built by ``agents.session.seed_messages_for_run`` (and,
    when memory is on, ``Agent._inject_memory_block``)::

        [{"role": "system",    "content": <agent instructions>},
         {"role": "system",    "content": "Relevant memory ..."},   # optional
         {"role": "user",      "content": "<prior turn 1>"},
         {"role": "assistant", "content": "<prior reply 1>"},
         ...
         {"role": "user",      "content": "<new prompt>"}]          # trailing

    This returns that thread with two adjustments so a multi-phase strategy can
    layer its own phase prompt on top while the model still sees the conversation
    so far + any retrieved memory (matching ``react``'s behaviour):

    1. The **trailing new-user prompt is dropped** — the strategy re-frames it
       into its own phase prompt and appends that itself.
    2. The **leading system block is swapped for *system*** — so a phase's role
       augmentation ("You are the proposer", "agent N of M") is preserved, while
       any injected memory block and every prior user/assistant turn are kept.

    When *initial_messages* is ``None`` (a fresh, sessionless run) it returns the
    single default system block ``[{"role": "system", "content": system}]`` — the
    prior per-strategy behaviour, unchanged.

    NOTE: meta phases that intentionally use a DIFFERENT system persona (critic,
    judge, devil's-advocate, self-evaluator) do NOT call this — they evaluate the
    draft text a generative phase already produced, so they keep their own system
    and need no history.
    """
    if not initial_messages:
        return [{"role": "system", "content": system}]
    # Copy each carried message so the strategy can append/extend freely without
    # mutating the caller's seed list.
    prefix = [dict(m) for m in initial_messages[:-1]]
    if prefix and prefix[0].get("role") == "system":
        prefix[0] = {"role": "system", "content": system}
    else:
        prefix.insert(0, {"role": "system", "content": system})
    return prefix
