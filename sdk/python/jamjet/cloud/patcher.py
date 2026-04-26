from __future__ import annotations

from typing import Any

from .budget import get_budget
from .events import emit
from .policy import get_evaluator
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


def _classify_exception(exc: BaseException) -> str:
    """Map a raised exception to a failure_mode enum value (Plan 5 Phase 4).

    Recognizes common openai/anthropic/httpx errors plus our own exceptions
    (BudgetExceeded, JamJetPolicyBlocked). Falls back to ``custom`` for
    anything unrecognized — never raises from this function.
    """
    name = type(exc).__name__
    # Our own exceptions, named explicitly
    if name == "JamJetBudgetExceeded":
        return "budget_exceeded"
    if name == "JamJetPolicyBlocked":
        return "policy_block"
    if name in ("JamJetApprovalRejected",):
        return "approval_rejected"
    if name == "JamJetApprovalTimeout":
        return "downstream_failure"
    # Provider rate limits
    if name in ("RateLimitError", "TooManyRequestsError"):
        return "model_rate_limited"
    # Provider timeouts
    if name in ("APITimeoutError", "TimeoutException", "ReadTimeout", "WriteTimeout", "ConnectTimeout"):
        return "model_timeout" if "API" in name else "network_error"
    # Network-level
    if name in ("ConnectError", "NetworkError", "ConnectionError", "RemoteProtocolError"):
        return "network_error"
    # Provider-side bad request / refusal
    if name in ("BadRequestError", "UnprocessableEntityError"):
        return "model_invalid_request"
    if name in ("ContentPolicyViolationError",):
        return "model_refusal"
    # Validation errors from pydantic / similar
    if name in ("ValidationError", "ValueError", "TypeError"):
        return "validation_error"
    return "custom"


def _estimate_cost(model: str, input_tokens: int, output_tokens: int) -> float:
    """Estimate cost in USD for a given model and token counts."""
    rates = _COST_PER_TOKEN.get(model)
    if rates is None:
        # Fallback: rough average
        return (input_tokens * 3e-6) + (output_tokens * 15e-6)
    return (input_tokens * rates[0]) + (output_tokens * rates[1])


# ---------------------------------------------------------------------------
# OpenAI patching
# ---------------------------------------------------------------------------


def patch_openai() -> None:
    """Monkey-patch the openai SDK so both module-level and instance usage are
    captured: ``openai.chat.completions.create(...)`` and
    ``OpenAI().chat.completions.create(...)`` both flow through one wrapper.

    We patch the bound method on ``Completions`` (and ``AsyncCompletions``) at
    the *class* level — every client instance picks the patch up automatically.
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
        budget = get_budget()
        evaluator = get_evaluator()
        ctx = get_context()

        tools = kwargs.get("tools")
        if tools:
            allowed, blocked = evaluator.filter_tools(tools)
            if not allowed and blocked:
                kwargs.pop("tools", None)
                kwargs.pop("tool_choice", None)
            elif blocked:
                kwargs["tools"] = allowed

        model = kwargs.get("model", "gpt-4o")
        budget.check_or_raise(_estimate_cost(model, 500, 1000))

        span = ctx.new_span(kind="llm_call", name=f"openai.{model}")
        try:
            result = original(self_inner, *args, **kwargs)
        except BaseException as exc:
            span.fail(mode=_classify_exception(exc))
            emit(span.to_event_dict())
            raise

        usage = getattr(result, "usage", None)
        input_tokens = getattr(usage, "prompt_tokens", 0) or 0
        output_tokens = getattr(usage, "completion_tokens", 0) or 0
        actual_model = getattr(result, "model", model) or model
        cost = _estimate_cost(actual_model, input_tokens, output_tokens)

        span.model = actual_model
        span.input_tokens = input_tokens
        span.output_tokens = output_tokens
        span.cost_usd = cost
        span.finish(status="ok")

        budget.record(cost)
        emit(span.to_event_dict())
        return result

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


def patch_anthropic() -> None:
    """Monkey-patch ``anthropic.Anthropic.messages.create`` (sync)."""
    try:
        import anthropic
    except ImportError:
        return

    if "anthropic" in _originals:
        return

    # anthropic.Anthropic().messages is a Messages resource; we patch the class
    # method. mypy can't see through the descriptor protocol used by the
    # anthropic SDK to assign create at the class level, so silence the
    # attr-defined errors here and at the assignment below.
    messages_cls = anthropic.Anthropic.messages.__class__
    original = messages_cls.create  # type: ignore[attr-defined]
    _originals["anthropic"] = (messages_cls, original)

    def patched_create(self_inner: Any, *args: Any, **kwargs: Any) -> Any:
        budget = get_budget()
        ctx = get_context()

        model = kwargs.get("model", "claude-sonnet-4-6")
        budget.check_or_raise(_estimate_cost(model, 500, 1000))

        span = ctx.new_span(kind="llm_call", name=f"anthropic.{model}")
        try:
            result = original(self_inner, *args, **kwargs)
        except BaseException as exc:
            span.fail(mode=_classify_exception(exc))
            emit(span.to_event_dict())
            raise

        # Anthropic response has .usage with input_tokens / output_tokens
        usage = getattr(result, "usage", None)
        input_tokens = getattr(usage, "input_tokens", 0) or 0
        output_tokens = getattr(usage, "output_tokens", 0) or 0
        actual_model = getattr(result, "model", model) or model
        cost = _estimate_cost(actual_model, input_tokens, output_tokens)

        span.model = actual_model
        span.input_tokens = input_tokens
        span.output_tokens = output_tokens
        span.cost_usd = cost
        span.finish(status="ok")

        budget.record(cost)
        emit(span.to_event_dict())
        return result

    messages_cls.create = patched_create  # type: ignore[attr-defined]


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

    httpx.Client.send = patched_sync_send  # type: ignore[method-assign]
    httpx.AsyncClient.send = patched_async_send  # type: ignore[method-assign]


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
