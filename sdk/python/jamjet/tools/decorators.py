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

from pydantic import BaseModel

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
        input_props: dict[str, Any] = {}
        required: list[str] = []
        for param_name, param in sig.parameters.items():
            if param_name in ("self", "cls"):
                continue
            hint = hints.get(param_name, Any)
            input_props[param_name] = _type_to_schema(hint)
            if param.default is inspect.Parameter.empty:
                required.append(param_name)

        input_schema = {
            "type": "object",
            "properties": input_props,
            "required": required,
        }

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
            return await fn(*args, **kwargs)

        wrapper._jamjet_tool = defn  # type: ignore[attr-defined]
        return wrapper  # type: ignore[return-value]

    if func is not None:
        return decorator(func)
    return decorator


def get_tool(name: str) -> ToolDefinition | None:
    return _TOOL_REGISTRY.get(name)


def list_tools() -> list[ToolDefinition]:
    return list(_TOOL_REGISTRY.values())


def _type_to_schema(t: Any) -> Any:
    """Very basic type → JSON schema mapping."""
    if t is str or t is type(None):
        return "string"
    if t is int:
        return "integer"
    if t is float:
        return "number"
    if t is bool:
        return "boolean"
    return "object"
