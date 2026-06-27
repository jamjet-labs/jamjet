"""Tests for the Model-seam sidecar server (Task 2e-1).

Tests the core _complete handler directly (no Starlette required) and also
exercises the HTTP routes via httpx ASGITransport (starlette is installed).
No real model calls are made; a FakeModel stub is injected throughout.
"""

import httpx
import pytest

import jamjet.model.sidecar_server as sidecar_module
from jamjet.model.sidecar_server import _complete, app
from jamjet.model.types import ModelResponse


class FakeMessage:
    """Minimal OpenAI-shaped message object returned by the fake backend."""

    def __init__(self, content: str = "hi", finish_reason: str | None = None) -> None:
        self.content = content
        self.finish_reason = finish_reason


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
