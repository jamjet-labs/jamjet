"""Default model-seam middleware chain. ONE wiring point for Track 3."""

from __future__ import annotations

from jamjet.model.metering import MeteringMiddleware
from jamjet.model.middleware import ModelAllowlistMiddleware, ModelMiddleware


def default_model_middleware() -> list[ModelMiddleware]:
    """The Track-1 default chain: allow-all + metering.

    ``ModelAllowlistMiddleware(None)`` is allow-all by design for Track 1;
    Track 3 replaces the ``None`` with the agent's policy-derived allowlist
    (and adds budget + PII middleware) HERE, so every seam call site inherits
    it from one place.
    """
    return [ModelAllowlistMiddleware(None), MeteringMiddleware()]
