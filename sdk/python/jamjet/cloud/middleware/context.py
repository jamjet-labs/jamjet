"""Vendor-kwarg <-> CallContext bidirectional mapping. One pair per provider.
The patcher uses these to convert vendor SDK call args into a portable shape
the middleware chain can operate on, then back into vendor shape for the
terminal call."""
from __future__ import annotations
from typing import Any
from jamjet.cloud.middleware import CallContext


_OPENAI_TOP_LEVEL = {"model", "messages", "tools"}
_ANTHROPIC_TOP_LEVEL = {"model", "messages", "tools", "system"}


def call_context_from_openai_kwargs(kwargs: dict[str, Any]) -> CallContext:
    """Convert OpenAI Completions.create() kwargs to CallContext. The OpenAI
    schema puts system prompts as the first message with role=system; we
    extract that into ctx.system so middleware that should skip system
    prompts (PII) does not have to special-case role checks."""
    messages = list(kwargs.get("messages") or [])
    system = None
    if messages and messages[0].get("role") == "system":
        content = messages[0].get("content")
        # OpenAI accepts str or list-of-parts; coerce to str for ctx.system
        system = content if isinstance(content, str) else str(content)
        messages = messages[1:]
    extra = {k: v for k, v in kwargs.items() if k not in _OPENAI_TOP_LEVEL}
    return CallContext(
        provider="openai",
        model=str(kwargs.get("model") or ""),
        messages=messages,
        tools=list(kwargs.get("tools") or []),
        system=system,
        extra_kwargs=extra,
    )


def openai_kwargs_from_call_context(ctx: CallContext) -> dict[str, Any]:
    """Reverse mapping. Re-inserts the system message at index 0 if present."""
    messages: list[dict[str, Any]] = []
    if ctx.system is not None:
        messages.append({"role": "system", "content": ctx.system})
    messages.extend(ctx.messages)
    return {
        "model": ctx.model,
        "messages": messages,
        "tools": ctx.tools,
        **ctx.extra_kwargs,
    }


def call_context_from_anthropic_kwargs(kwargs: dict[str, Any]) -> CallContext:
    """Anthropic Messages.create() puts the system prompt in a top-level
    `system` kwarg (not in messages), so the extraction is simpler."""
    extra = {k: v for k, v in kwargs.items() if k not in _ANTHROPIC_TOP_LEVEL}
    return CallContext(
        provider="anthropic",
        model=str(kwargs.get("model") or ""),
        messages=list(kwargs.get("messages") or []),
        tools=list(kwargs.get("tools") or []),
        system=kwargs.get("system"),
        extra_kwargs=extra,
    )


def anthropic_kwargs_from_call_context(ctx: CallContext) -> dict[str, Any]:
    out: dict[str, Any] = {
        "model": ctx.model,
        "messages": ctx.messages,
        "tools": ctx.tools,
        **ctx.extra_kwargs,
    }
    if ctx.system is not None:
        out["system"] = ctx.system
    return out
