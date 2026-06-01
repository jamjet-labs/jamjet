"""Fixed-order chain runner. Composes middlewares as nested closures and
exposes a single `run()` entrypoint that the patcher calls per LLM call."""

from __future__ import annotations

import os
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any

from jamjet.cloud.middleware import CallContext, PreCallMiddleware, Response


@dataclass
class Chain:
    """Ordered list of middlewares plus a runner. Build once at configure()
    time; reuse for every patched call. Thread-safe iff each middleware is
    thread-safe (the chain itself holds no per-call state)."""

    middlewares: list[PreCallMiddleware] = field(default_factory=list)

    def run(
        self,
        ctx: CallContext,
        terminal: Callable[[CallContext], Response],
    ) -> Response:
        """Execute the chain. `terminal` is the real LLM call; it runs iff no
        middleware short-circuits. Middlewares execute in declaration order;
        each one wraps the next via a `next` continuation it must invoke (or
        explicitly skip to short-circuit)."""

        # Compose nested closures right-to-left so middleware[0] is outermost.
        def make_step(i: int) -> Callable[[CallContext], Response]:
            if i == len(self.middlewares):
                return terminal
            current = self.middlewares[i]
            nxt = make_step(i + 1)

            def step(c: CallContext) -> Response:
                return current(c, nxt)

            return step

        return make_step(0)(ctx)


_FLAG_ENV = "JAMJET_MIDDLEWARE_ENABLED"


def _feature_enabled() -> bool:
    """Phase 1 ships behind a feature flag for a 14-day soak. Returning False
    here yields an empty chain, which is exactly the pre-Phase-1 behaviour."""
    return os.environ.get(_FLAG_ENV) == "1"


def build_chain(policy: dict[str, Any]) -> Chain:
    """Construct a middleware chain from a loaded policy dict. Phase 1
    instantiates only `redact` rules; `cache` (Phase 2) and `fallback`
    (Phase 3) are silently skipped so forward-compatible policies don't
    break today.

    Returns an empty Chain when:
      - The feature flag is off
      - No rules with middleware-eligible actions exist

    Both cases are byte-identical to the pre-Phase-1 patcher behaviour."""
    if not _feature_enabled():
        return Chain(middlewares=[])

    middlewares: list[PreCallMiddleware] = []

    redact_rules = [r for r in policy.get("rules", []) if r.get("action") == "redact"]
    if redact_rules:
        # Local import to avoid a circular reference at module-load time.
        from jamjet.cloud.middleware.pii import PIIMiddleware

        middlewares.append(PIIMiddleware(rules=redact_rules))

    return Chain(middlewares=middlewares)
