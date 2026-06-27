"""Starlette sidecar that wraps the governed Model seam.

Exposes two routes consumed by the durable Rust engine:
  POST /v1/complete  -- runs the full middleware chain (allowlist, PII, metering)
  GET  /health       -- liveness probe used by the Rust startup guard

Run with:
  uvicorn jamjet.model.sidecar_server:app --host 127.0.0.1 --port 4280
"""

from __future__ import annotations

import json as _json
from typing import Any

from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

from jamjet.model.defaults import default_model_middleware
from jamjet.model.seam import Model
from jamjet.model.types import ModelRequest, parse_model_ref

_RATE_LIMIT_FALLBACK_SECS = 5
# Cap untrusted provider Retry-After so a huge value cannot overflow timestamp math
# in the Rust worker. Must match MAX_RETRY_AFTER_SECS in runtime/workers/src/worker.rs.
_MAX_RETRY_AFTER_SECS = 3_600


def _extract_retry_after(exc: BaseException) -> int:
    """Return retry_after seconds from a provider rate-limit exception.

    Checks, in order:
    1. ``exc.response.headers["retry-after"]`` (set by litellm / openai SDK).
    2. ``exc.retry_after`` attribute (defensive; uncommon in practice).
    Falls back to ``_RATE_LIMIT_FALLBACK_SECS`` if nothing is found.
    """
    response = getattr(exc, "response", None)
    if response is not None:
        headers = getattr(response, "headers", {})
        raw = headers.get("retry-after") or headers.get("Retry-After")
        if raw is not None:
            try:
                return max(0, min(int(raw), _MAX_RETRY_AFTER_SECS))
            except (ValueError, TypeError):
                pass
    val = getattr(exc, "retry_after", None)
    if val is not None:
        try:
            return max(0, min(int(val), _MAX_RETRY_AFTER_SECS))
        except (ValueError, TypeError):
            pass
    return _RATE_LIMIT_FALLBACK_SECS


# Module-level singleton -- created lazily on first call so tests can inject
# a fake model by monkeypatching _get_model.
_model: Model | None = None


def _get_model() -> Model:
    global _model  # noqa: PLW0603 -- intentional singleton
    if _model is None:
        _model = Model(middleware=default_model_middleware())
    return _model


def _extract_tool_calls(message: Any) -> list[dict[str, Any]]:
    """Extract and normalise tool_calls from an OpenAI-shaped message.

    litellm returns tool_calls as a list of objects with:
      tc.id, tc.function.name, tc.function.arguments (JSON-serialised string)

    We normalise arguments to a JSON object when possible; the Rust side stores
    whatever Value arrives so no information is lost.
    """
    raw = getattr(message, "tool_calls", None)
    if not raw:
        return []
    out: list[dict[str, Any]] = []
    for tc in raw:
        fn = getattr(tc, "function", None)
        name = getattr(fn, "name", None) if fn else getattr(tc, "name", None)
        raw_args = getattr(fn, "arguments", None) if fn else getattr(tc, "arguments", None)
        if isinstance(raw_args, str):
            try:
                raw_args = _json.loads(raw_args)
            except Exception:
                pass  # keep as string if the provider emits malformed JSON
        out.append({"id": getattr(tc, "id", ""), "name": name or "", "arguments": raw_args})
    return out


async def _complete(body: dict[str, Any], model: Model) -> dict[str, Any]:
    """Core completion handler; injectable for testing without Starlette."""
    ref = parse_model_ref(body["model"])
    req = ModelRequest(
        ref=ref,
        messages=body["messages"],
        tools=body.get("tools") or None,
        temperature=body.get("temperature"),
        max_tokens=body.get("max_tokens"),
    )
    resp = await model.complete(req)

    tool_calls = _extract_tool_calls(resp.message)
    finish_reason = getattr(resp.message, "finish_reason", None) or ("tool_calls" if tool_calls else None) or "stop"

    return {
        "message": {
            "content": resp.message.content or "",
            "role": "assistant",
        },
        "tool_calls": tool_calls,
        "input_tokens": resp.input_tokens,
        "output_tokens": resp.output_tokens,
        "cost_usd": resp.cost_usd,
        "model": ref.litellm_model,
        "finish_reason": finish_reason,
    }


async def _handle_complete(request: Request) -> JSONResponse:
    try:
        body = await request.json()
        result = await _complete(body, _get_model())
        return JSONResponse(result)
    except (KeyError, ValueError) as exc:
        return JSONResponse({"error": str(exc)}, status_code=400)
    except Exception as exc:
        # Duck-typed check: litellm.exceptions.RateLimitError (and openai.RateLimitError)
        # carry status_code=429.  We check this without importing litellm at module level
        # so the sidecar server remains importable when the model extra is absent.
        # ModelDeniedError has no status_code attribute and will fall through to ``raise``,
        # preserving the governance fail-closed contract.
        if getattr(exc, "status_code", None) == 429:
            retry_after = _extract_retry_after(exc)
            return JSONResponse(
                {"error": str(exc), "retry_after": retry_after},
                status_code=429,
            )
        raise


async def _handle_health(request: Request) -> JSONResponse:
    return JSONResponse({"ok": True})


app = Starlette(
    routes=[
        Route("/v1/complete", _handle_complete, methods=["POST"]),
        Route("/health", _handle_health, methods=["GET"]),
    ]
)
