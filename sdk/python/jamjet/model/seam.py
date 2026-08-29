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
    """The governed seam.

    Omitting ``middleware`` builds the default chain
    (:func:`jamjet.model.defaults.default_model_middleware`), so a bare
    ``Model()`` redacts PII and meters spend rather than reaching the provider
    raw. Running with no middleware at all is available, but it has to be asked
    for by name: ``Model(ungoverned=True)``.
    """

    def __init__(
        self,
        *,
        middleware: list[ModelMiddleware] | None = None,
        backend: Any | None = None,
        ungoverned: bool = False,
    ) -> None:
        if backend is None:
            from jamjet.model.litellm_backend import LiteLLMBackend

            backend = LiteLLMBackend()
        self._backend = backend

        if ungoverned and middleware is not None:
            raise ValueError("Model(ungoverned=True) takes no middleware. Pass a chain, or ask for no chain, not both.")

        chain: list[ModelMiddleware]
        if ungoverned:
            chain = []
        elif middleware is None:
            from jamjet.model.defaults import default_model_middleware

            chain = default_model_middleware()
        elif not middleware:
            raise ValueError(
                "Model(middleware=[]) would run the seam ungoverned: no PII "
                "redaction, no metering, no budget. Pass ungoverned=True to mean "
                "that deliberately, or omit `middleware` for the default chain."
            )
        else:
            chain = list(middleware)
        self._middleware: list[ModelMiddleware] = chain

    async def complete(self, request: ModelRequest) -> ModelResponse:
        for mw in self._middleware:
            request = await mw.before(request)
        response = await self._backend.complete(request)
        for mw in reversed(self._middleware):
            response = await mw.after(request, response)
        return response

    async def stream(self, request: ModelRequest) -> AsyncIterator[StreamChunk]:
        # Track 1 scope: only the ``before`` chain runs, so the allowlist (and any
        # future deny/redact middleware) still gates streamed calls. The ``after``
        # chain (metering, audit) is intentionally NOT run here -- streamed
        # token/cost accounting needs a usage-bearing finalizer and lands in
        # Track 2 with the looped, durable stream. Non-streamed completions are
        # fully metered via ``complete()``.
        streaming = replace(request, stream=True)
        for mw in self._middleware:
            streaming = await mw.before(streaming)
        async for chunk in self._backend.stream(streaming):
            yield chunk
