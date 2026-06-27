"""Starlette sidecar that wraps the governed Model seam.

Exposes two routes consumed by the durable Rust engine:
  POST /v1/complete  -- runs the full middleware chain (allowlist, PII, metering)
  GET  /health       -- liveness probe used by the Rust startup guard

Run with:
  uvicorn jamjet.model.sidecar_server:app --host 127.0.0.1 --port 4280
"""

from __future__ import annotations

from typing import Any

from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

from jamjet.model.defaults import default_model_middleware
from jamjet.model.seam import Model
from jamjet.model.types import ModelRequest, parse_model_ref

# Module-level singleton -- created lazily on first call so tests can inject
# a fake model by monkeypatching _get_model.
_model: Model | None = None


def _get_model() -> Model:
    global _model  # noqa: PLW0603 -- intentional singleton
    if _model is None:
        _model = Model(middleware=default_model_middleware())
    return _model


async def _complete(body: dict[str, Any], model: Model) -> dict[str, Any]:
    """Core completion handler; injectable for testing without Starlette."""
    ref = parse_model_ref(body["model"])
    req = ModelRequest(
        ref=ref,
        messages=body["messages"],
        temperature=body.get("temperature"),
        max_tokens=body.get("max_tokens"),
    )
    resp = await model.complete(req)
    return {
        "message": {
            "content": resp.message.content,
            "role": "assistant",
        },
        "input_tokens": resp.input_tokens,
        "output_tokens": resp.output_tokens,
        "cost_usd": resp.cost_usd,
        "model": ref.litellm_model,
        "finish_reason": getattr(resp.message, "finish_reason", None) or "stop",
    }


async def _handle_complete(request: Request) -> JSONResponse:
    body = await request.json()
    try:
        result = await _complete(body, _get_model())
        return JSONResponse(result)
    except (KeyError, ValueError) as exc:
        return JSONResponse({"error": str(exc)}, status_code=400)


async def _handle_health(request: Request) -> JSONResponse:
    return JSONResponse({"ok": True})


app = Starlette(
    routes=[
        Route("/v1/complete", _handle_complete, methods=["POST"]),
        Route("/health", _handle_health, methods=["GET"]),
    ]
)
