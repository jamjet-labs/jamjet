from __future__ import annotations

import json
from typing import Any

from .budget import get_budget
from .events import emit
from .exceptions import JamJetPolicyBlocked, JamJetUnenforceableCall
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


def _is_server_executed(tool: Any) -> bool:
    """Tools the vendor runs itself (Anthropic server tools such as web_search)
    execute before the response comes back, so refusing the call afterwards is
    too late."""
    kind = _field(tool, "type")
    if not isinstance(kind, str) or kind in ("function", "custom"):
        return False
    return _field(tool, "input_schema") is None


def _stripped_by(tool: Any) -> tuple[str, str, str | None] | None:
    """(name, kind, pattern) when a rule removes ``tool`` from the request."""
    evaluator = get_evaluator()
    if _field(tool, "type") == "mcp_toolset":
        # Tool names inside an MCP server are unknown before the call; any
        # restrictive rule could match one.
        if evaluator.has_restrictive_rules():
            return f"mcp_toolset:{_field(tool, 'mcp_server_name') or ''}", "block", "*"
        return None
    name = tool_name_of(tool)
    decision = evaluator.evaluate(name)
    if decision.blocked:
        return name, decision.policy_kind, decision.pattern
    if decision.policy_kind == "require_approval" and _is_server_executed(tool):
        return name, decision.policy_kind, decision.pattern
    return None


def _filter_tools_pre_call(kwargs: dict[str, Any]) -> None:
    """Strip refused tools from ``tools`` and the legacy OpenAI ``functions``.

    Refuses a call whose ``tool_choice`` / ``function_call`` forces a removed
    tool, and drops the settings vendors reject once no tools are left.
    """
    for key, choice, drop in (
        ("tools", "tool_choice", ("tool_choice", "parallel_tool_calls")),
        ("functions", "function_call", ("function_call",)),
    ):
        listed = kwargs.get(key)
        if not listed:
            continue
        # Materialise once: a generator would otherwise be spent by the check
        # and reach the vendor empty.
        listed = list(listed)
        kwargs[key] = listed
        kept: list[Any] = []
        removed: list[str] = []
        for tool in listed:
            hit = _stripped_by(tool)
            if hit is None:
                kept.append(tool)
            else:
                removed.append(hit[0])
        if not removed:
            continue
        forced = tool_name_of(kwargs.get(choice))
        if forced and forced in removed:
            decision = get_evaluator().evaluate(forced)
            raise JamJetPolicyBlocked(forced, decision.pattern or "*")
        if kept:
            kwargs[key] = kept
        else:
            # Vendors reject an empty list, and the settings that only make sense with tools.
            kwargs.pop(key, None)
            for k in drop:
                kwargs.pop(k, None)
    # Remote MCP servers run their tools on the vendor side under names unknown
    # before the call; with any restrictive rule they are not offered.
    if kwargs.get("mcp_servers") and get_evaluator().has_restrictive_rules():
        kwargs.pop("mcp_servers", None)


def _response_tool_calls(vendor: str, result: Any) -> list[tuple[str | None, str]]:
    """(id, name) of every tool call in a finished response."""
    calls: list[tuple[str | None, str]] = []
    if vendor == "openai":
        for choice in _field(result, "choices") or []:
            message = _field(choice, "message")
            for tc in _field(message, "tool_calls") or []:
                fn = _field(tc, "function") or _field(tc, "custom")
                name = _field(fn, "name")
                calls.append((_field(tc, "id"), name if isinstance(name, str) else ""))
            # Legacy `functions` API: a single message.function_call.
            legacy = _field(message, "function_call")
            if legacy is not None:
                calls.append((None, tool_name_of(legacy)))
        return calls
    for block in _field(result, "content") or []:
        if _field(block, "type") in ("tool_use", "server_tool_use", "mcp_tool_use"):
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

    # `with_raw_response.create()` goes through this patch and gets the raw
    # response back. Enforce on its parsed body (cached, so the caller's own
    # .parse() returns the same, enforced object) and hand back the raw one.
    raw = result if _is_raw_response(result) else None
    if raw is not None:
        result = raw.parse()

    if kwargs.get("stream") is True:
        enforced = _enforce_streamed(vendor, result, span, model, kwargs)
        return raw if raw is not None else enforced

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
    return raw if raw is not None else result


def _is_raw_response(result: Any) -> bool:
    return hasattr(result, "http_response") and callable(getattr(result, "parse", None))


def _refuse_unenforced(label: str, original: Any) -> Any:
    """Wrap an SDK method this module does not enforce: it runs normally, unless
    a block or require_approval rule is active, when it raises instead."""

    def refusing(self_inner: Any, *args: Any, **kwargs: Any) -> Any:
        if get_evaluator().has_restrictive_rules():
            raise JamJetUnenforceableCall(label)
        return original(self_inner, *args, **kwargs)

    return refusing


def _patch_methods(key: str, cls: Any, methods: dict[str, Any]) -> None:
    """Replace ``cls.<name>`` with ``make(original)`` for each entry, remembering
    the originals under ``key`` for unpatching."""
    saved: list[tuple[Any, str, Any]] = []
    for name, make in methods.items():
        original = cls.__dict__.get(name)
        if original is None:
            continue
        saved.append((cls, name, original))
        setattr(cls, name, make(original))
    _originals.setdefault(key, []).extend(saved)


def _enforcing(vendor: str) -> Any:
    def make(original: Any) -> Any:
        def patched(self_inner: Any, *args: Any, **kwargs: Any) -> Any:
            return _run_enforced(vendor, original, self_inner, args, kwargs)

        return patched

    return make


def _unpatch(key: str) -> None:
    for cls, name, original in _originals.pop(key, []):
        setattr(cls, name, original)


# ---------------------------------------------------------------------------
# OpenAI patching
# ---------------------------------------------------------------------------


def patch_openai() -> None:
    """Monkey-patch the openai SDK so both module-level and instance usage are
    captured: ``openai.chat.completions.create(...)`` and
    ``OpenAI().chat.completions.create(...)`` both flow through one wrapper.

    Patched at the *class* level, so every client instance picks it up.
    ``chat.completions.create`` and ``.parse`` are enforced; ``.stream()`` and
    ``with_raw_response.create()`` go through ``create``. The Responses API is
    not enforced and refuses to run while a block or require_approval rule is
    active. The async client is not patched.
    """
    try:
        from openai.resources.chat.completions import Completions
    except ImportError:
        return

    if "openai" in _originals:
        return  # already patched

    _patch_methods("openai", Completions, {"create": _enforcing("openai"), "parse": _enforcing("openai")})
    try:
        from openai.resources.responses import Responses
    except ImportError:
        return
    _patch_methods(
        "openai",
        Responses,
        {
            m: (lambda original, m=m: _refuse_unenforced(f"openai responses.{m}", original))
            for m in ("create", "stream", "parse")
        },
    )


def unpatch_openai() -> None:
    """Restore the original openai methods."""
    _unpatch("openai")


# ---------------------------------------------------------------------------
# Anthropic patching
# ---------------------------------------------------------------------------


def _anthropic_messages_classes() -> list[Any]:
    """The sync Messages resource classes: ``messages`` and ``beta.messages``.

    ``anthropic.Anthropic.messages`` is a ``cached_property`` on the client
    class, so reading it on the class yields the descriptor, not the resource;
    the classes have to come from ``anthropic.resources``.
    """
    classes: list[Any] = []
    try:
        from anthropic.resources.messages import Messages
    except ImportError:
        try:
            from anthropic.resources import Messages  # type: ignore[no-redef,attr-defined,unused-ignore]
        except ImportError:
            return classes
    classes.append(Messages)
    try:
        from anthropic.resources.beta.messages import Messages as BetaMessages
    except ImportError:
        return classes
    classes.append(BetaMessages)
    return classes


def patch_anthropic() -> None:
    """Monkey-patch ``Anthropic().messages`` and ``Anthropic().beta.messages`` (sync).

    ``create`` and ``parse`` are enforced, including ``with_raw_response``.
    ``stream()`` posts directly instead of calling ``create``, so it is not
    enforced and refuses to run while a block or require_approval rule is
    active.
    """
    if "anthropic" in _originals:
        return
    for cls in _anthropic_messages_classes():
        _patch_methods(
            "anthropic",
            cls,
            {
                "create": _enforcing("anthropic"),
                "parse": _enforcing("anthropic"),
                "stream": lambda original: _refuse_unenforced("anthropic messages.stream", original),
            },
        )


def unpatch_anthropic() -> None:
    """Restore the original anthropic methods."""
    _unpatch("anthropic")


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
