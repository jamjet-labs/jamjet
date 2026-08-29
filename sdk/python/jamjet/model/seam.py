"""The Model seam: the single governed path for every model call.

User code never calls a provider directly. The seam runs the middleware chain
(``before`` denies/mutates, ``after`` meters/audits) around a swappable backend.
"""

from __future__ import annotations

from collections.abc import AsyncIterator, Iterable
from dataclasses import replace
from typing import Any

from jamjet.model.middleware import ModelMiddleware
from jamjet.model.types import ModelRequest, ModelResponse, StreamChunk


class Model:
    """The seam, governed by default.

    Omitting ``middleware`` builds the default chain
    (:func:`jamjet.model.defaults.default_model_middleware`): PII redaction and
    a metering recorder, plus an allowlist and a budget that stay no-ops until a
    ``GovernanceConfig`` supplies them.  ``Model(ungoverned=True)`` opts out.

    Two limits, so this reads as the safe default it is rather than an
    enforcement guarantee:

    * ``stream()`` runs only the ``before`` chain, so a streamed call is
      redacted but NOT metered and cannot trip a budget.  See ``stream``.
    * Any non-empty chain of ``ModelMiddleware`` is accepted, whatever it does.
      A caller-supplied no-op chain is as ungoverned as ``ungoverned=True``
      without saying so.  The default protects the caller who does not think
      about middleware; it does not constrain one who does.
    """

    def __init__(
        self,
        *,
        middleware: Iterable[ModelMiddleware] | None = None,
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
        else:
            # Materialize BEFORE the emptiness check: an exhausted iterator is
            # truthy, so checking the argument would let `(mw for mw in [])`
            # through as a silent ungoverned chain.
            chain = list(middleware)
            bad = [mw for mw in chain if not isinstance(mw, ModelMiddleware)]
            if bad:
                # Without this, Model(middleware="pii") iterates into
                # ['p','i','i'] and the failure surfaces as a bare
                # AttributeError at the provider boundary instead of here.
                raise TypeError(
                    "Model(middleware=...) takes ModelMiddleware objects; got "
                    f"{', '.join(type(mw).__name__ for mw in bad)}."
                )

        if not chain and not ungoverned:
            # Also covers the default path, so the invariant holds however the
            # chain was built rather than only on the caller-supplied branch.
            raise ValueError(
                "Model(middleware=[]) would run the seam ungoverned: no PII "
                "redaction, no metering, no budget. Pass ungoverned=True to mean "
                "that deliberately, or omit `middleware` for the default chain."
            )
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
