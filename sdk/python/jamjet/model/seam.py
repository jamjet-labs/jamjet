"""The Model seam: the single governed path for every model call.

User code never calls a provider directly. The seam runs the middleware chain
(``before`` denies/mutates, ``after`` meters/audits) around a swappable backend.
"""

from __future__ import annotations

from collections.abc import AsyncIterator
from dataclasses import replace
from typing import Any

from jamjet.model.middleware import ModelMiddleware
from jamjet.model.types import ModelRequest, ModelResponse, StreamChunk


class Model:
    def __init__(
        self,
        *,
        middleware: list[ModelMiddleware] | None = None,
        backend: Any | None = None,
    ) -> None:
        if backend is None:
            from jamjet.model.litellm_backend import LiteLLMBackend

            backend = LiteLLMBackend()
        self._backend = backend
        self._middleware: list[ModelMiddleware] = list(middleware or [])

    async def complete(self, request: ModelRequest) -> ModelResponse:
        for mw in self._middleware:
            request = await mw.before(request)
        response = await self._backend.complete(request)
        for mw in reversed(self._middleware):
            response = await mw.after(request, response)
        return response

    async def stream(self, request: ModelRequest) -> AsyncIterator[StreamChunk]:
        streaming = replace(request, stream=True)
        for mw in self._middleware:
            streaming = await mw.before(streaming)
        async for chunk in self._backend.stream(streaming):
            yield chunk
