"""Runtime helper for the durable agent loop — dispatch a model turn's tool calls.

This is the function the ``__tools_<turn>__`` PythonFn nodes emitted by
:func:`jamjet.compiler.agent_ir.compile_agent_to_ir` point at. On the durable
engine it runs on the ``python_tool`` worker (Track 2d): the worker resolves
``payload["module"]`` / ``payload["function"]`` to this coroutine and calls it
with ``payload["input"]`` — the **full accumulated workflow state** (the
scheduler passes ``progress.final_state`` as the PythonFn input; PythonFn nodes
have no per-node input mapping).

Message accumulation lives ENTIRELY here so the generic Model executor stays
generic: we append the assistant message that requested the tools plus one
``role: tool`` message per call to the running ``messages`` list, and return the
**full updated list**. The engine replaces ``state["messages"]`` with it
(merge-patch, top-level key replace).

Input keys (read with fallbacks so this is unit-testable without the engine):

* ``messages``              — the running message list (default ``[]``)
* ``tool_calls`` | ``last_model_tool_calls`` — ``[{id, name, arguments}, ...]``
  (the Model executor / Track 2j-2 writes ``last_model_tool_calls`` to state)
* ``assistant_content`` | ``last_model_output`` — the assistant content text
* ``tools``                 — ``{name: "module:function"}`` resolver map

Each tool is resolved through the ``module:function`` map (imported on demand,
mirroring the worker's own handler resolution) so the dispatch worker does not
need the user's tool module pre-imported; the in-process ``@tool`` registry is a
secondary fallback.
"""

from __future__ import annotations

import importlib
import inspect
import json
from collections.abc import Callable
from typing import Any


async def dispatch_tool_calls(input: dict[str, Any]) -> dict[str, Any]:  # noqa: A002 - matches worker payload key
    """Run the tool calls from one model turn and return the updated messages.

    Returns ``{"messages": <full updated list>}`` which replaces
    ``state["messages"]`` for the next turn's model node.
    """
    messages: list[dict[str, Any]] = list(input.get("messages") or [])

    tool_calls = input.get("tool_calls")
    if tool_calls is None:
        tool_calls = input.get("last_model_tool_calls") or []

    assistant_content = input.get("assistant_content")
    if assistant_content is None:
        assistant_content = input.get("last_model_output")

    tools: dict[str, str] = input.get("tools") or {}

    # 1. The assistant message that requested these tool calls (OpenAI shape so
    #    the message history replays cleanly into the next model turn).
    messages.append(
        {
            "role": "assistant",
            "content": assistant_content or "",
            "tool_calls": [_assistant_tool_call(call) for call in tool_calls],
        }
    )

    # 2. Execute each requested tool, append its result as a tool message.
    for call in tool_calls:
        name = call.get("name")
        call_id = call.get("id")
        arguments = _coerce_arguments(call.get("arguments"))

        fn = _resolve_tool(name, tools)
        result = fn(**arguments) if isinstance(arguments, dict) else fn(arguments)
        if inspect.isawaitable(result):
            result = await result

        messages.append(
            {
                "role": "tool",
                "tool_call_id": call_id,
                "name": name,
                "content": _stringify(result),
            }
        )

    # 3. Return the FULL updated list — replaces state["messages"].
    return {"messages": messages}


# ── Helpers ──────────────────────────────────────────────────────────────────


def _assistant_tool_call(call: dict[str, Any]) -> dict[str, Any]:
    """Render a ``{id, name, arguments}`` call into the OpenAI assistant shape."""
    arguments = call.get("arguments")
    if not isinstance(arguments, str):
        arguments = json.dumps(arguments if arguments is not None else {})
    return {
        "id": call.get("id"),
        "type": "function",
        "function": {"name": call.get("name"), "arguments": arguments},
    }


def _coerce_arguments(arguments: Any) -> Any:
    """Tool arguments arrive as a dict or a JSON string — normalise to a dict."""
    if isinstance(arguments, str):
        if not arguments.strip():
            return {}
        try:
            return json.loads(arguments)
        except json.JSONDecodeError:
            return {}
    return arguments if arguments is not None else {}


def _resolve_tool(name: str | None, tools: dict[str, str]) -> Callable[..., Any]:
    """Resolve a tool name to a callable via the ``module:function`` map.

    Falls back to the in-process ``@tool`` registry both when the name is absent
    from the map AND when a present-but-stale ref fails to import/resolve (useful
    for in-process tests and dev, and resilient to a renamed/moved tool module).
    """
    ref = tools.get(name) if (name and isinstance(tools, dict)) else None
    if ref:
        try:
            module_path, _, fn_path = ref.partition(":")
            obj: Any = importlib.import_module(module_path)
            for part in fn_path.split("."):
                obj = getattr(obj, part)
            return obj
        except (ImportError, AttributeError):
            # A stale/non-importable map ref must not mask the @tool registry
            # fallback below — fall through and try the registry before raising.
            pass

    from jamjet.tools.decorators import get_tool  # noqa: PLC0415 - avoid import cycle

    defn = get_tool(name) if name else None
    if defn is None:
        raise KeyError(f"tool {name!r} not found in tools map or @tool registry")
    return defn.fn


def _stringify(result: Any) -> str:
    """Coerce a tool result to the string content a ``role: tool`` message needs."""
    if isinstance(result, str):
        return result
    model_dump_json = getattr(result, "model_dump_json", None)  # pydantic BaseModel
    if callable(model_dump_json):
        return model_dump_json()
    try:
        return json.dumps(result)
    except (TypeError, ValueError):
        return str(result)
