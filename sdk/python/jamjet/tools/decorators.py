"""
@tool decorator — registers a Python function as a typed JamJet tool.

Usage::

    from jamjet import tool
    from pydantic import BaseModel

    class SearchResult(BaseModel):
        summary: str
        sources: list[str]

    @tool
    async def web_search(query: str) -> SearchResult:
        ...
"""

from __future__ import annotations

import functools
import inspect
from collections.abc import Callable
from typing import Any, TypeVar, get_type_hints

from pydantic import BaseModel, TypeAdapter

F = TypeVar("F", bound=Callable[..., Any])

# Registry of all tools defined in this process.
_TOOL_REGISTRY: dict[str, ToolDefinition] = {}


class ToolDefinition:
    """Metadata for a registered tool."""

    def __init__(
        self,
        name: str,
        fn: Callable[..., Any],
        input_schema: dict[str, Any],
        output_schema: dict[str, Any],
        description: str | None = None,
        permissions: list[str] | None = None,
    ) -> None:
        self.name = name
        self.fn = fn
        self.input_schema = input_schema
        self.output_schema = output_schema
        self.description = description or (fn.__doc__ or "").strip()
        self.permissions = permissions or []
        # The callable `@tool` returns (with the replay intercept), set by the
        # decorator. Dispatch runs this, never the raw `fn`, whichever way a
        # call is resolved.
        self.wrapper: Callable[..., Any] | None = None

    def __repr__(self) -> str:
        return f"Tool(name={self.name!r})"


def tool(
    func: F | None = None,
    *,
    name: str | None = None,
    permissions: list[str] | None = None,
) -> Any:
    """
    Register a function as a JamJet tool.

    The function must have typed parameters and a typed return value.
    If the return type is a Pydantic BaseModel, its JSON schema is used.

    Usage::

        @tool
        async def get_weather(city: str) -> WeatherResult:
            ...

        @tool(name="custom_name", permissions=["read_only"])
        async def my_tool(x: int) -> str:
            ...
    """

    def decorator(fn: F) -> F:
        tool_name = name or fn.__name__
        hints = get_type_hints(fn)
        return_type = hints.pop("return", None)

        # Build input schema from function signature
        sig = inspect.signature(fn)
        params = [(n, p) for n, p in sig.parameters.items() if n not in ("self", "cls")]
        schemas, definitions = _parameter_schemas([hints.get(n, Any) for n, _ in params])
        input_schema: dict[str, Any] = {
            "type": "object",
            "properties": {n: schema for (n, _), schema in zip(params, schemas)},
            "required": [n for n, p in params if p.default is inspect.Parameter.empty],
        }
        if definitions:
            # Model parameters refer to "#/$defs/<name>", which resolves from
            # the root of the schema the model is shown.
            input_schema["$defs"] = definitions

        # Build output schema
        if return_type is not None and isinstance(return_type, type) and issubclass(return_type, BaseModel):
            output_schema = return_type.model_json_schema()
        else:
            output_schema = {"type": _type_to_schema(return_type)}

        defn = ToolDefinition(
            name=tool_name,
            fn=fn,
            input_schema=input_schema,
            output_schema=output_schema,
            permissions=permissions,
        )
        _TOOL_REGISTRY[tool_name] = defn

        # Keep the function callable — @tool-decorated functions can still be
        # called directly (useful in tests and local dev).
        @functools.wraps(fn)
        async def wrapper(*args: Any, **kwargs: Any) -> Any:
            # Replay intercept (Phase 6.1): when a replay bundle is active,
            # return the recorded output instead of calling the real tool.
            # Import is local to avoid a hard dep cycle at module load.
            try:
                from jamjet.cloud.replay import get_active as _get_replay  # noqa: PLC0415

                _replay = _get_replay()
            except Exception:  # noqa: BLE001
                _replay = None
            if _replay is not None:
                sig_params = list(inspect.signature(fn).parameters.keys())
                tool_input = dict(zip(sig_params, args))
                tool_input.update(kwargs)
                try:
                    return _replay.lookup_tool_output(tool_name, tool_input)
                except (KeyError, TypeError):
                    # KeyError: no recording for this input.
                    # TypeError: input contains non-JSON-serializable values
                    #   (Pydantic model, datetime, bytes, etc.) — treat as cache miss.
                    pass  # Fall through to live execution.
            return await fn(*args, **kwargs)

        wrapper._jamjet_tool = defn  # type: ignore[attr-defined]
        defn.wrapper = wrapper
        return wrapper  # type: ignore[return-value]

    if func is not None:
        return decorator(func)
    return decorator


def get_tool(name: str) -> ToolDefinition | None:
    return _TOOL_REGISTRY.get(name)


def list_tools() -> list[ToolDefinition]:
    return list(_TOOL_REGISTRY.values())


def _parameter_schemas(hints: list[Any]) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    """JSON Schemas for a tool's parameter types, in order, and the definitions they refer to.

    All the types go through one Pydantic schema generation, as one tuple. A
    model used by two parameters then gets one entry in the definitions, and
    two models with the same class name get different entries.
    """
    if not hints:
        return [], {}
    described = [hint if _has_json_schema(hint) else Any for hint in hints]
    whole = TypeAdapter(tuple[tuple(described)]).json_schema()
    schemas: list[dict[str, Any]] = []
    for hint, shown, schema in zip(hints, described, whole["prefixItems"]):
        if shown is not hint:
            # Pydantic has no schema for this type. Keep what @tool has always
            # said about it.
            schemas.append({"type": "object"})
        elif schema == {"additionalProperties": True, "type": "object"}:
            # A plain `dict`. `additionalProperties: true` is the default, so
            # this is the same schema @tool emitted before.
            schemas.append({"type": "object"})
        else:
            schemas.append(schema)
    return schemas, whole.get("$defs", {})


def _has_json_schema(hint: Any) -> bool:
    try:
        TypeAdapter(hint).json_schema()
    except Exception:  # noqa: BLE001 - @tool has never rejected an annotation
        return False
    return True


def _type_to_schema(t: Any) -> Any:
    """Very basic type → JSON schema mapping, used for return types."""
    if t is str or t is type(None):
        return "string"
    if t is int:
        return "integer"
    if t is float:
        return "number"
    if t is bool:
        return "boolean"
    return "object"
