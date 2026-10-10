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
* ``tools``                 — ``{name: "module:qualname"}`` map the compiler writes

A call resolves to the ``@tool`` registered under its name. A ``tools`` entry is
used only to import the module that registers the tool, so the dispatch worker
need not have imported it, and is accepted only when it is that tool's own
``module:qualname``; otherwise the call is refused (see ``_resolve_tool``).
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


class ToolResolutionError(LookupError):
    """A tool call that cannot be resolved to the registered tool of that name.

    Raised instead of running anything: the dispatch fails closed.
    """


def _registered_ref(defn: Any) -> str:
    """The registered tool's own ``module:qualname``, as the SDK compiler
    writes it into the run's ``tools`` map (``agent_ir._tools_map``)."""
    return f"{defn.fn.__module__}:{defn.fn.__qualname__}"


def _resolve_tool(name: str | None, tools: dict[str, str]) -> Callable[..., Any]:
    """Resolve a tool call to a registered ``@tool`` of that name.

    The run's ``tools`` map is NOT trusted to choose code: it is part of the
    run's state, which whoever starts the run seeds. An entry is accepted only
    when the object it names is itself a ``@tool`` registered under this call's
    name, and the entry is exactly that tool's own ``module:qualname`` (what
    the SDK compiler writes). The path is walked with
    :func:`inspect.getattr_static`, so no descriptor or attribute magic runs.
    Anything else, such as an attribute path (``mod:send_wire.__call__``), a
    re-export under another name, or an entry pointing a tool name at other
    code, raises :class:`ToolResolutionError` and nothing runs. A call with no
    entry resolves through the registry by name.
    """
    from jamjet.tools.decorators import _TOOL_REGISTRY, get_tool  # noqa: PLC0415 - avoid import cycle

    if not name:
        raise ToolResolutionError("a tool call has no name")
    ref = tools.get(name) if isinstance(tools, dict) else None
    if ref is None:
        defn = get_tool(name)
        if defn is None:
            raise ToolResolutionError(f"tool {name!r} not found in the @tool registry")
        return defn.fn

    if not isinstance(ref, str) or ":" not in ref:
        raise ToolResolutionError(f"tools map entry for {name!r} is not a module:function reference")
    module_path, _, fn_path = ref.partition(":")

    # The common case, with no import: the tool registered under this name
    # is the one the entry names.
    registered = get_tool(name)
    if registered is not None and _registered_ref(registered) == ref:
        return registered.fn

    # Otherwise the entry's module is imported to find what it names. Importing
    # runs its @tool decorators, which would overwrite any tool already
    # registered under the same name for the life of the worker, so a later
    # call with no entry would run the import's tool. Any name the import
    # overwrote is put back; new names may stay.
    snapshot = dict(_TOOL_REGISTRY)
    try:
        obj: Any = importlib.import_module(module_path)
    except ImportError as exc:
        raise ToolResolutionError(f"tool {name!r}: tools map entry {ref!r} cannot be imported to verify it") from exc
    finally:
        for existing_name, existing in snapshot.items():
            if _TOOL_REGISTRY.get(existing_name) is not existing:
                _TOOL_REGISTRY[existing_name] = existing
    for part in fn_path.split("."):
        obj = inspect.getattr_static(obj, part, None)
        if obj is None:
            raise ToolResolutionError(f"tool {name!r}: tools map entry {ref!r} names nothing")
    defn = getattr(obj, "_jamjet_tool", None)
    if defn is None or defn.name != name or _registered_ref(defn) != ref:
        raise ToolResolutionError(
            f"tool {name!r}: tools map entry {ref!r} is not the registered @tool {name!r}'s own reference"
        )
    return obj


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
