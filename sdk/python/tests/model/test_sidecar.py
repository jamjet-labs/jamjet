"""Tests for the Model-seam sidecar server (Task 2e-1, 2f-5).

Tests the core _complete handler directly (no Starlette required) and also
exercises the HTTP routes via httpx ASGITransport (starlette is installed).
No real model calls are made; a FakeModel stub is injected throughout.
"""

import httpx
import pytest

import jamjet.model.sidecar_server as sidecar_module
from jamjet.model.sidecar_server import (
    _RATE_LIMIT_FALLBACK_SECS,
    _complete,
    _extract_retry_after,
    app,
)
from jamjet.model.types import ModelResponse


class _FakeFunction:
    """Minimal function-call descriptor on a tool call (mirrors litellm shape)."""

    def __init__(self, name: str, arguments: str) -> None:
        self.name = name
        self.arguments = arguments


class _FakeToolCall:
    """Minimal tool-call descriptor returned by litellm on a message."""

    def __init__(self, id: str, name: str, arguments: object) -> None:
        self.id = id
        # Mimic litellm: store under .function.name / .function.arguments
        args_str = arguments if isinstance(arguments, str) else __import__("json").dumps(arguments)
        self.function = _FakeFunction(name=name, arguments=args_str)


class FakeMessage:
    """Minimal OpenAI-shaped message object returned by the fake backend."""

    def __init__(
        self,
        content: str = "hi",
        finish_reason: str | None = None,
        tool_calls: list | None = None,
    ) -> None:
        self.content = content
        self.finish_reason = finish_reason
        self.tool_calls = tool_calls


class FakeModel:
    """Stub that satisfies the Model.complete() contract without a real provider."""

    async def complete(self, request):  # noqa: ANN001, ANN201
        return ModelResponse(
            message=FakeMessage(content="hi"),
            input_tokens=3,
            output_tokens=4,
            cost_usd=0.001,
        )


# --------------------------------------------------------------------------- #
# Core handler tests (no Starlette / HTTP)
# --------------------------------------------------------------------------- #


async def test_complete_core_returns_wrapped_shape() -> None:
    """_complete() maps ModelResponse into the sidecar wire format."""
    result = await _complete(
        {
            "model": "anthropic/claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "hello"}],
        },
        FakeModel(),  # type: ignore[arg-type]
    )

    assert result["message"]["content"] == "hi"
    assert result["message"]["role"] == "assistant"
    assert result["input_tokens"] == 3
    assert result["output_tokens"] == 4
    assert result["cost_usd"] == pytest.approx(0.001)
    assert result["model"] == "anthropic/claude-sonnet-4-6"
    assert result["finish_reason"] == "stop"  # None -> "stop"


async def test_complete_core_finish_reason_passthrough() -> None:
    """finish_reason is forwarded when the message carries one."""

    class FakeModelWithReason:
        async def complete(self, request):  # noqa: ANN001, ANN201
            return ModelResponse(
                message=FakeMessage(content="done", finish_reason="length"),
                input_tokens=1,
                output_tokens=1,
                cost_usd=0.0,
            )

    result = await _complete(
        {"model": "openai/gpt-4o", "messages": []},
        FakeModelWithReason(),  # type: ignore[arg-type]
    )
    assert result["finish_reason"] == "length"


async def test_complete_core_optional_params_forwarded() -> None:
    """temperature and max_tokens are forwarded to the ModelRequest."""
    received_reqs = []

    class CapturingModel:
        async def complete(self, request):  # noqa: ANN001, ANN201
            received_reqs.append(request)
            return ModelResponse(
                message=FakeMessage(content="ok"),
                input_tokens=0,
                output_tokens=0,
                cost_usd=0.0,
            )

    await _complete(
        {
            "model": "anthropic/claude-opus-4-8",
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.7,
            "max_tokens": 512,
        },
        CapturingModel(),  # type: ignore[arg-type]
    )

    assert len(received_reqs) == 1
    req = received_reqs[0]
    assert req.temperature == pytest.approx(0.7)
    assert req.max_tokens == 512


# --------------------------------------------------------------------------- #
# 2j-1: tool-call round-trip and tools forwarded through the governed seam
# --------------------------------------------------------------------------- #


async def test_complete_core_tools_forwarded_to_model() -> None:
    """tools in the request body are passed through to ModelRequest.tools (governed path)."""
    received_reqs = []

    class CapturingModel:
        async def complete(self, request):  # noqa: ANN001, ANN201
            received_reqs.append(request)
            return ModelResponse(message=FakeMessage(content="ok"), input_tokens=0, output_tokens=0, cost_usd=0.0)

    tool_schema = {"type": "function", "function": {"name": "get_weather", "parameters": {}}}
    await _complete(
        {
            "model": "anthropic/claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [tool_schema],
        },
        CapturingModel(),  # type: ignore[arg-type]
    )

    assert len(received_reqs) == 1
    assert received_reqs[0].tools == [tool_schema], "tools must be threaded into ModelRequest (governed path)"


async def test_complete_core_tool_calls_returned() -> None:
    """When the model returns tool_calls, they appear in the response with finish_reason='tool_calls'."""

    class FakeToolCallModel:
        async def complete(self, request):  # noqa: ANN001, ANN201
            tc = _FakeToolCall(id="c1", name="get_weather", arguments={"city": "SF"})
            return ModelResponse(
                message=FakeMessage(content=None, finish_reason="tool_calls", tool_calls=[tc]),
                input_tokens=5,
                output_tokens=3,
                cost_usd=0.001,
            )

    result = await _complete(
        {"model": "anthropic/claude-sonnet-4-6", "messages": [{"role": "user", "content": "weather?"}]},
        FakeToolCallModel(),  # type: ignore[arg-type]
    )

    assert result["finish_reason"] == "tool_calls"
    assert result["tool_calls"] == [{"id": "c1", "name": "get_weather", "arguments": {"city": "SF"}}]
    # content is empty string when the model is making tool calls, not producing text
    assert result["message"]["content"] == ""


async def test_complete_core_tool_calls_infer_finish_reason() -> None:
    """finish_reason is inferred as 'tool_calls' when tool_calls are present but finish_reason is None."""

    class FakeToolCallModelNoReason:
        async def complete(self, request):  # noqa: ANN001, ANN201
            tc = _FakeToolCall(id="c2", name="search", arguments={"q": "hello"})
            return ModelResponse(
                message=FakeMessage(content=None, finish_reason=None, tool_calls=[tc]),
                input_tokens=2,
                output_tokens=1,
                cost_usd=0.0,
            )

    result = await _complete(
        {"model": "openai/gpt-4o", "messages": []},
        FakeToolCallModelNoReason(),  # type: ignore[arg-type]
    )

    assert result["finish_reason"] == "tool_calls", "must infer tool_calls finish_reason when tool_calls present"
    assert len(result["tool_calls"]) == 1


# --------------------------------------------------------------------------- #
# HTTP route tests (via httpx ASGITransport)
# --------------------------------------------------------------------------- #


async def test_health_route() -> None:
    """GET /health returns {"ok": true}."""
    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
        resp = await client.get("/health")

    assert resp.status_code == 200
    assert resp.json()["ok"] is True


async def test_complete_route(monkeypatch: pytest.MonkeyPatch) -> None:
    """POST /v1/complete returns the wire format via the Starlette route."""
    monkeypatch.setattr(sidecar_module, "_get_model", lambda: FakeModel())

    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
        resp = await client.post(
            "/v1/complete",
            json={
                "model": "anthropic/claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "hello"}],
            },
        )

    assert resp.status_code == 200
    data = resp.json()
    assert data["message"]["content"] == "hi"
    assert data["message"]["role"] == "assistant"
    assert data["input_tokens"] == 3
    assert data["output_tokens"] == 4
    assert data["model"] == "anthropic/claude-sonnet-4-6"


async def test_complete_route_malformed_json(monkeypatch: pytest.MonkeyPatch) -> None:
    """POST /v1/complete with a non-JSON body returns 400, not 500.

    Regression guard for the json.JSONDecodeError (a ValueError subclass) fix:
    request.json() is now inside the try so a malformed body returns the intended
    400 rather than an unhandled 500.
    """
    monkeypatch.setattr(sidecar_module, "_get_model", lambda: FakeModel())

    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
        resp = await client.post(
            "/v1/complete",
            content=b"not json at all",
            headers={"content-type": "application/json"},
        )

    assert resp.status_code == 400
    assert "error" in resp.json()


async def test_complete_route_missing_model_key(monkeypatch: pytest.MonkeyPatch) -> None:
    """POST /v1/complete with missing 'model' key returns 400."""
    monkeypatch.setattr(sidecar_module, "_get_model", lambda: FakeModel())

    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
        resp = await client.post(
            "/v1/complete",
            json={"messages": [{"role": "user", "content": "hi"}]},
        )

    assert resp.status_code == 400
    assert "error" in resp.json()


# --------------------------------------------------------------------------- #
# 2f-5: provider 429 must surface as HTTP 429 with retry_after
# --------------------------------------------------------------------------- #


class _FakeRateLimitResponse:
    """Minimal response object carrying a Retry-After header."""

    def __init__(self, retry_after: int | None = None) -> None:
        self.headers: dict[str, str] = {}
        if retry_after is not None:
            self.headers["retry-after"] = str(retry_after)


class _RateLimitError(Exception):
    """Stub that mimics litellm.exceptions.RateLimitError without importing litellm.

    The production handler checks ``status_code == 429`` (duck typing), so this
    stub exercises the exact same code path as the real litellm exception.
    """

    status_code = 429

    def __init__(self, msg: str = "rate limited", *, retry_after: int | None = None) -> None:
        super().__init__(msg)
        self.response = _FakeRateLimitResponse(retry_after=retry_after)


class FakeRateLimitModel:
    """Injects a _RateLimitError (no Retry-After header) into the completion path."""

    async def complete(self, request):  # noqa: ANN001, ANN201
        raise _RateLimitError("provider rate limit")


class FakeRateLimitModelWithRetryAfter:
    """Injects a _RateLimitError carrying Retry-After: 30 in the response headers."""

    async def complete(self, request):  # noqa: ANN001, ANN201
        raise _RateLimitError("provider rate limit", retry_after=30)


async def test_complete_route_rate_limited_returns_429(monkeypatch: pytest.MonkeyPatch) -> None:
    """Provider 429 must surface as HTTP 429 with retry_after in the body (2f-5)."""
    monkeypatch.setattr(sidecar_module, "_get_model", lambda: FakeRateLimitModel())

    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
        resp = await client.post(
            "/v1/complete",
            json={
                "model": "anthropic/claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "hi"}],
            },
        )

    assert resp.status_code == 429, f"expected 429, got {resp.status_code}"
    body = resp.json()
    assert "retry_after" in body, f"retry_after missing from 429 body: {body}"
    assert isinstance(body["retry_after"], int), "retry_after must be an integer"
    assert "error" in body


async def test_complete_route_rate_limited_propagates_retry_after(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Retry-After header from the provider response must be forwarded in the body (2f-5)."""
    monkeypatch.setattr(sidecar_module, "_get_model", lambda: FakeRateLimitModelWithRetryAfter())

    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
        resp = await client.post(
            "/v1/complete",
            json={
                "model": "anthropic/claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "hi"}],
            },
        )

    assert resp.status_code == 429
    assert resp.json()["retry_after"] == 30, f"expected 30, got {resp.json()}"


def test_extract_retry_after_negative_header_clamps_to_zero() -> None:
    """A negative Retry-After header value must clamp to 0, never go negative (2f-5)."""

    class _FakeResp:
        headers = {"retry-after": "-1"}

    class _ExcError(Exception):
        status_code = 429
        response = _FakeResp()

    result = _extract_retry_after(_ExcError())
    assert result == 0, f"expected 0 for Retry-After: -1, got {result}"


def test_extract_retry_after_non_numeric_header_falls_back_to_default() -> None:
    """A non-numeric Retry-After (e.g. an HTTP-date) must not crash; fall back to default (2f-5)."""

    class _FakeResp:
        headers = {"retry-after": "Wed, 21 Oct 2015 07:28:00 GMT"}

    class _ExcError(Exception):
        status_code = 429
        response = _FakeResp()

    result = _extract_retry_after(_ExcError())
    assert result == _RATE_LIMIT_FALLBACK_SECS, (
        f"non-numeric Retry-After must fall back to {_RATE_LIMIT_FALLBACK_SECS}, got {result}"
    )


async def test_complete_route_model_denied_is_non_200(monkeypatch: pytest.MonkeyPatch) -> None:
    """ModelDeniedError must NOT be caught by the 429 branch (fail-closed preserved).

    Starlette's ServerErrorMiddleware converts unhandled exceptions to 500 in
    production (uvicorn).  We use ``raise_server_exceptions=False`` so httpx
    also converts them to 500 rather than re-raising, matching that behaviour.
    """
    from jamjet.model.middleware import ModelDeniedError

    class FakeDeniedModel:
        async def complete(self, request):  # noqa: ANN001, ANN201
            raise ModelDeniedError("model not in allowlist", code="model_not_allowed")

    monkeypatch.setattr(sidecar_module, "_get_model", lambda: FakeDeniedModel())

    # raise_app_exceptions=False mirrors uvicorn: unhandled exceptions become 500.
    transport = httpx.ASGITransport(app=app, raise_app_exceptions=False)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
        resp = await client.post(
            "/v1/complete",
            json={
                "model": "bad/model",
                "messages": [{"role": "user", "content": "hi"}],
            },
        )

    assert resp.status_code != 200, f"ModelDeniedError must never return 200, got {resp.status_code}"
    assert resp.status_code != 429, "ModelDeniedError must not be swallowed by the 429 branch"


# --------------------------------------------------------------------------- #
# C1: sidecar Model must be governed (non-empty middleware chain)
# --------------------------------------------------------------------------- #


def test_get_model_has_governed_middleware() -> None:
    """The real _get_model() constructs a Model with the default middleware chain.

    This test would FAIL against the old bare ``Model()`` construction (no middleware)
    and PASSES once ``default_model_middleware()`` is wired in.
    """
    from jamjet.model.metering import MeteringMiddleware
    from jamjet.model.middleware import ModelAllowlistMiddleware

    # Force a fresh singleton so monkeypatching in other tests doesn't affect us.
    sidecar_module._model = None
    model = sidecar_module._get_model()

    assert len(model._middleware) > 0, (
        "_get_model() returned a bare Model() with no middleware — the sidecar is ungoverned"
    )
    mw_types = [type(mw) for mw in model._middleware]
    assert ModelAllowlistMiddleware in mw_types, (
        "ModelAllowlistMiddleware must be in the sidecar Model's middleware chain"
    )
    assert MeteringMiddleware in mw_types, "MeteringMiddleware must be in the sidecar Model's middleware chain"

    # Restore singleton state so subsequent tests get a fresh instance.
    sidecar_module._model = None
