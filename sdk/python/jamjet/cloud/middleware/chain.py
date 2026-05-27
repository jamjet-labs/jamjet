"""Fixed-order chain runner. Composes middlewares as nested closures and
exposes a single `run()` entrypoint that the patcher calls per LLM call."""
from __future__ import annotations
from dataclasses import dataclass, field
from typing import Callable
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
