from __future__ import annotations

import json
from typing import Any

from .budget import get_budget
from .events import emit
from .exceptions import JamJetPolicyBlocked
from .middleware import Chain
from .middleware.context import (
    anthropic_kwargs_from_call_context,
    call_context_from_anthropic_kwargs,
    call_context_from_openai_kwargs,
    openai_kwargs_from_call_context,
)
from .middleware.receipts import build_llm_receipt, emit_receipt
from .policy import _field, get_evaluator, tool_name_of
from .stream_enforcement import StreamedToolCall, StreamTally, enforce_stream
from .trace import get_context

# ---------------------------------------------------------------------------
# Cost estimation per-model (USD per token)
# ---------------------------------------------------------------------------

_COST_PER_TOKEN: dict[str, tuple[float, float]] = {
    # (input_cost_per_token, output_cost_per_token)
    "gpt-4o": (2.5e-6, 10e-6),
    "gpt-4o-mini": (0.15e-6, 0.6e-6),
    "gpt-4-turbo": (10e-6, 30e-6),
    "gpt-4": (30e-6, 60e-6),
    "gpt-3.5-turbo": (0.5e-6, 1.5e-6),
    "claude-sonnet-4-6": (3e-6, 15e-6),
    "claude-sonnet-4-20250514": (3e-6, 15e-6),
    "claude-opus-4-6": (15e-6, 75e-6),
    "claude-opus-4-20250514": (15e-6, 75e-6),
    "claude-3-5-haiku-20241022": (0.8e-6, 4e-6),
    "claude-3-haiku-20240307": (0.25e-6, 1.25e-6),
}

_originals: dict[str, Any] = {}


# ---------------------------------------------------------------------------
# Middleware runtime state
# ---------------------------------------------------------------------------
#
# The middleware chain wraps every patched LLM call. The default is an empty
# Chain — that path is byte-identical to the pre-middleware behaviour, so the
# patcher stays no-op when no policy.yaml has been loaded or the feature flag
# is off. `configure()` in cloud/__init__.py is expected to replace
# `_STATE.middleware_chain` with a real Chain built from the loaded policy.


class _RuntimeState:
    middleware_chain: Chain = Chain(middlewares=[])


_STATE = _RuntimeState()


def _runtime_state() -> _RuntimeState:
    return _STATE


def _estimate_cost(model: str, input_tokens: int, output_tokens: int) -> float:
    """Estimate cost in USD for a given model and token counts."""
    rates = _COST_PER_TOKEN.get(model)
    if rates is None:
        # Fallback: rough average
        return (input_tokens * 3e-6) + (output_tokens * 15e-6)
    return (input_tokens * rates[0]) + (output_tokens * rates[1])


# ---------------------------------------------------------------------------
# Shared enforcement for one patched call
# ---------------------------------------------------------------------------


def _estimate_prompt_tokens(kwargs: dict[str, Any]) -> int:
    """~4 characters per token over everything sent as input: messages, tool
    definitions, system, legacy prompt (used when a stream reports no usage)."""
    try:
        sent = [kwargs[k] for k in ("messages", "tools", "system", "prompt") if kwargs.get(k) is not None]
        return -(-len(json.dumps(sent, default=str)) // 4)
    except Exception:  # noqa: BLE001  an estimate must never break the call
        return 0


def _filter_tools_pre_call(kwargs: dict[str, Any]) -> None:
    """Strip blocked tools from the request, in either vendor's tool shape."""
    tools = kwargs.get("tools")
    if not tools:
        return
    # Materialise once: a generator would otherwise be spent by the check and
    # reach the vendor empty.
    tools = list(tools)
    kwargs["tools"] = tools
    evaluator = get_evaluator()
    allowed, blocked = evaluator.filter_tools(tools)
    if not blocked:
        return
    # A tool_choice that forces a stripped tool cannot be honoured: refuse the
    # call rather than let the vendor reject an unknown tool name.
    forced = tool_name_of(kwargs.get("tool_choice"))
    if forced:
        decision = evaluator.evaluate(forced)
        if decision.blocked:
            raise JamJetPolicyBlocked(forced, decision.pattern or "*")
    if allowed:
        kwargs["tools"] = allowed
    else:
        # Vendors reject an empty tools array, and a tool_choice with no tools.
        kwargs.pop("tools", None)
        kwargs.pop("tool_choice", None)


def _response_tool_calls(vendor: str, result: Any) -> list[tuple[str | None, str]]:
    """(id, name) of every tool call in a finished response."""
    calls: list[tuple[str | None, str]] = []
    if vendor == "openai":
        for choice in _field(result, "choices") or []:
            for tc in _field(_field(choice, "message"), "tool_calls") or []:
                fn = _field(tc, "function") or _field(tc, "custom")
                name = _field(fn, "name")
                calls.append((_field(tc, "id"), name if isinstance(name, str) else ""))
        return calls
    for block in _field(result, "content") or []:
        if _field(block, "type") in ("tool_use", "server_tool_use"):
            name = _field(block, "name")
            calls.append((_field(block, "id"), name if isinstance(name, str) else ""))
    return calls


def _refuse_blocked(span: Any, calls: list[tuple[str | None, str]]) -> None:
    """Raise JamJetPolicyBlocked (and emit the span) on the first blocked call."""
    evaluator = get_evaluator()
    for call_id, name in calls:
        decision = evaluator.evaluate(name)
        if decision.blocked:
            entry: dict[str, Any] = {"name": name}
            if call_id is not None:
                entry["id"] = call_id
            span.payload = {**span.payload, "policy_blocked_tool_calls": [entry]}
            span.finish(status="error")
            emit(span.to_event_dict())
            raise JamJetPolicyBlocked(name, decision.pattern or "*")


def _record_usage(span: Any, model: str, input_tokens: int, output_tokens: int, **payload: Any) -> None:
    cost = _estimate_cost(model, input_tokens, output_tokens)
    span.model = model
    span.input_tokens = input_tokens
    span.output_tokens = output_tokens
    span.cost_usd = cost
    if payload:
        span.payload = {**span.payload, **payload}
    get_budget().record(cost)


def _emit_receipt_if_fired(call_ctx: Any) -> None:
    if not call_ctx.middleware_fired:
        return
    try:
        receipt = build_llm_receipt(
            call_ctx,
            matched_rule={"match": call_ctx.middleware_fired[0]},
            policy_version="1",
        )
        emit_receipt(receipt)
    except Exception:  # noqa: BLE001  receipt emission must never break or mask the call
        pass


def _enforce_streamed(vendor: str, result: Any, span: Any, model: str, kwargs: dict[str, Any]) -> Any:
    """Wrap a streaming result so its tool calls and usage are enforced."""
    prompt_estimate = _estimate_prompt_tokens(kwargs)

    def usage_of(tally: StreamTally, complete: bool) -> tuple[int, int]:
        estimated_output = -(-tally.output_chars // 4)
        if complete:
            output = tally.output_tokens if tally.output_tokens is not None else estimated_output
        else:
            # A stream cut short has only a partial output count.
            output = max(tally.output_tokens or 0, estimated_output)
        input_tokens = tally.input_tokens if tally.input_tokens is not None else prompt_estimate
        return input_tokens, output

    def refuse(calls: list[StreamedToolCall], tally: StreamTally) -> None:
        # Check the name the openai SDK keeps (last fragment) and the joined
        # fragments, so a consumer that concatenates them cannot get a blocked name.
        pairs = [(c.id, c.name) for c in calls]
        pairs += [(c.id, c.joined_name) for c in calls if c.joined_name and c.joined_name != c.name]
        evaluator = get_evaluator()
        if not any(evaluator.evaluate(name).blocked for _, name in pairs):
            return
        input_tokens, output = usage_of(tally, complete=False)
        _record_usage(span, tally.model or model, input_tokens, output, usage_estimated=True, stream_incomplete=True)
        _refuse_blocked(span, pairs)

    def on_end(tally: StreamTally, how: str, error: BaseException | None) -> None:
        if how == "error":
            if isinstance(error, JamJetPolicyBlocked):
                return  # recorded where it was raised
            # The tokens the stream produced before failing were still spent.
            input_tokens, output = usage_of(tally, complete=False)
            _record_usage(
                span, tally.model or model, input_tokens, output, usage_estimated=True, stream_incomplete=True
            )
            span.payload = {**span.payload, "error": str(error)}
            span.finish(status="error")
            emit(span.to_event_dict())
            return
        complete = how == "complete" and tally.finished
        input_tokens, output = usage_of(tally, complete)
        reported = tally.input_tokens is not None and tally.output_tokens is not None
        extra: dict[str, Any] = {}
        if not (complete and reported):
            extra["usage_estimated"] = True
        if not complete:
            extra["stream_incomplete"] = True
        _record_usage(span, tally.model or model, input_tokens, output, **extra)
        span.finish(status="ok")
        emit(span.to_event_dict())

    enforced = enforce_stream(
        result,
        vendor=vendor,
        hold_tool_calls=lambda: get_evaluator().has_rules(),
        on_tool_calls=refuse,
        on_tool_call_started=lambda call, tally: refuse([call], tally),
        on_end=on_end,
    )
    if enforced is not None:
        return enforced
    span.finish(status="ok")
    emit(span.to_event_dict())
    return result


def _run_enforced(vendor: str, original: Any, self_inner: Any, args: Any, kwargs: dict[str, Any]) -> Any:
    """One patched call: tool policy before and after, budget, span, middleware."""
    budget = get_budget()
    ctx = get_context()

    _filter_tools_pre_call(kwargs)

    model = kwargs.get("model", "gpt-4o" if vendor == "openai" else "claude-sonnet-4-6")
    budget.check_or_raise(_estimate_cost(model, 500, 1000))

    span = ctx.new_span(kind="llm_call", name=f"{vendor}.{model}")
    # Phase 1 middleware: the chain wraps original(). When the chain is empty
    # (default / flag-off / no middleware rules) the terminal lambda runs
    # unchanged. When a middleware mutates ctx (e.g. PII redact), the rebuilt
    # vendor kwargs flow into the real SDK call via the kwarg<->ctx round-trip.
    if vendor == "openai":
        call_ctx = call_context_from_openai_kwargs(kwargs)
        to_kwargs = openai_kwargs_from_call_context
    else:
        call_ctx = call_context_from_anthropic_kwargs(kwargs)
        to_kwargs = anthropic_kwargs_from_call_context
    chain: Chain = _runtime_state().middleware_chain
    try:
        result = chain.run(call_ctx, terminal=lambda c: original(self_inner, *args, **to_kwargs(c)))
    except BaseException:
        # Middleware short-circuited via raise. Emit a deny-decision receipt
        # iff middleware actually fired, then re-raise unchanged.
        _emit_receipt_if_fired(call_ctx)
        raise
    # Success path: a receipt iff middleware fired (e.g. PII redact).
    _emit_receipt_if_fired(call_ctx)

    if kwargs.get("stream") is True:
        return _enforce_streamed(vendor, result, span, model, kwargs)

    usage = getattr(result, "usage", None)
    if vendor == "openai":
        input_tokens = getattr(usage, "prompt_tokens", 0) or 0
        output_tokens = getattr(usage, "completion_tokens", 0) or 0
    else:
        input_tokens = getattr(usage, "input_tokens", 0) or 0
        output_tokens = getattr(usage, "output_tokens", 0) or 0
    actual_model = getattr(result, "model", model) or model
    # Cost first: a refused response still counts against the budget, the
    # tokens were spent.
    _record_usage(span, actual_model, input_tokens, output_tokens)
    _refuse_blocked(span, _response_tool_calls(vendor, result))
    span.finish(status="ok")
    emit(span.to_event_dict())
    return result


# ---------------------------------------------------------------------------
# OpenAI patching
# ---------------------------------------------------------------------------


def patch_openai() -> None:
    """Monkey-patch the openai SDK so both module-level and instance usage are
    captured: ``openai.chat.completions.create(...)`` and
    ``OpenAI().chat.completions.create(...)`` both flow through one wrapper.

    We patch ``Completions.create`` at the *class* level, so every client
    instance picks the patch up automatically. The async client is not patched.
    """
    try:
        from openai.resources.chat.completions import Completions
    except ImportError:
        return

    if "openai" in _originals:
        return  # already patched

    original = Completions.create
    _originals["openai"] = (Completions, original)

    def patched_create(self_inner: Any, *args: Any, **kwargs: Any) -> Any:
        return _run_enforced("openai", original, self_inner, args, kwargs)

    Completions.create = patched_create  # type: ignore[assignment,method-assign]


def unpatch_openai() -> None:
    """Restore the original Completions.create."""
    entry = _originals.pop("openai", None)
    if entry is None:
        return
    cls, original = entry
    cls.create = original


# ---------------------------------------------------------------------------
# Anthropic patching
# ---------------------------------------------------------------------------


def _anthropic_messages_class() -> Any:
    """The sync Messages resource class.

    ``anthropic.Anthropic.messages`` is a ``cached_property`` on the client
    class, so reading it on the class yields the descriptor, not the resource;
    the class has to come from ``anthropic.resources``.
    """
    try:
        from anthropic.resources.messages import Messages
    except ImportError:
        try:
            from anthropic.resources import Messages  # type: ignore[no-redef,attr-defined,unused-ignore]
        except ImportError:
            return None
    return Messages


def patch_anthropic() -> None:
    """Monkey-patch ``anthropic.Anthropic().messages.create`` (sync)."""
    messages_cls = _anthropic_messages_class()
    if messages_cls is None:
        return

    if "anthropic" in _originals:
        return

    original = messages_cls.create
    _originals["anthropic"] = (messages_cls, original)

    def patched_create(self_inner: Any, *args: Any, **kwargs: Any) -> Any:
        return _run_enforced("anthropic", original, self_inner, args, kwargs)

    messages_cls.create = patched_create


def unpatch_anthropic() -> None:
    """Restore the original anthropic messages.create."""
    if "anthropic" not in _originals:
        return
    messages_cls, original = _originals.pop("anthropic")
    messages_cls.create = original


# ---------------------------------------------------------------------------
# Convenience
# ---------------------------------------------------------------------------


def patch_httpx() -> None:
    """Inject traceparent + tracestate into outbound httpx requests.

    OpenAI's and Anthropic's Python SDKs both speak HTTP through httpx, so
    patching here covers most cross-agent calls without touching the LLM
    SDK auto-patches above (which capture spans, not propagation). Both
    sync ``Client.send`` and async ``AsyncClient.send`` are wrapped.

    Idempotent. Skipped silently if httpx isn't installed.
    """
    try:
        import httpx
    except ImportError:
        return

    if "httpx" in _originals:
        return

    from .propagation import inject_headers

    sync_original = httpx.Client.send
    async_original = httpx.AsyncClient.send
    _originals["httpx"] = (sync_original, async_original)

    def patched_sync_send(self_inner: Any, request: Any, *args: Any, **kwargs: Any) -> Any:
        try:
            inject_headers(request.headers)
        except Exception:  # noqa: BLE001  fail-open — never block the user's HTTP call
            pass
        return sync_original(self_inner, request, *args, **kwargs)

    async def patched_async_send(self_inner: Any, request: Any, *args: Any, **kwargs: Any) -> Any:
        try:
            inject_headers(request.headers)
        except Exception:  # noqa: BLE001
            pass
        return await async_original(self_inner, request, *args, **kwargs)

    httpx.Client.send = patched_sync_send  # type: ignore[method-assign,assignment]
    httpx.AsyncClient.send = patched_async_send  # type: ignore[method-assign,assignment]


def unpatch_httpx() -> None:
    entry = _originals.pop("httpx", None)
    if entry is None:
        return
    try:
        import httpx
    except ImportError:
        return
    sync_original, async_original = entry
    httpx.Client.send = sync_original  # type: ignore[method-assign]
    httpx.AsyncClient.send = async_original  # type: ignore[method-assign]


def patch_all() -> None:
    """Patch all supported integrations: OpenAI, Anthropic, httpx propagation."""
    patch_openai()
    patch_anthropic()
    patch_httpx()


def unpatch_all() -> None:
    """Unpatch all integrations."""
    unpatch_openai()
    unpatch_anthropic()
    unpatch_httpx()
