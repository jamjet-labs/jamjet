"""Seeded random/uuid/clock helpers injected as self.random / self.uuid_gen / self.now.

These are deterministic across runs given the same execution_id seed. Author code
that wants replay-faithful non-determinism must use these instead of stdlib calls.

The same seeds are also available to durable ``@tool`` handlers running under
``jamjet worker`` via the context-variable API: ``get_current_random()``,
``get_current_uuid_gen()``, and ``get_current_clock()``.  The worker calls
``_inject_seeds(execution_id)`` before each handler invocation.
"""

from __future__ import annotations

import contextvars as _cv
import hashlib
import random as _random
import uuid as _uuid
from datetime import UTC, datetime, timedelta


def _seed_int(execution_id: str) -> int:
    return int.from_bytes(hashlib.sha256(execution_id.encode()).digest()[:8], "big")


class SeededRandom(_random.Random):
    def __init__(self, execution_id: str) -> None:
        super().__init__(_seed_int(execution_id))


class SeededUuidGen:
    def __init__(self, execution_id: str) -> None:
        self._rng = _random.Random(_seed_int(execution_id) ^ 0xA5A5A5A5)

    def uuid4(self) -> _uuid.UUID:
        return _uuid.UUID(int=self._rng.getrandbits(128), version=4)


class SeededClock:
    """Monotonically advancing clock starting at a known instant.

    Each call to now() advances by 1ms — keeps timestamps unique and deterministic.
    """

    def __init__(self, start_iso: str | None = None) -> None:
        self._t = datetime.fromisoformat(start_iso) if start_iso else datetime.now(tz=UTC)

    def now(self) -> datetime:
        out = self._t
        self._t = self._t + timedelta(milliseconds=1)
        return out


# ── Context-variable seed injection (used by `jamjet worker`) ─────────────────

_current_random: _cv.ContextVar[SeededRandom | None] = _cv.ContextVar("_current_random", default=None)
_current_uuid_gen: _cv.ContextVar[SeededUuidGen | None] = _cv.ContextVar("_current_uuid_gen", default=None)
_current_clock: _cv.ContextVar[SeededClock | None] = _cv.ContextVar("_current_clock", default=None)


def _inject_seeds(execution_id: str) -> None:
    """Set deterministic seed sources for the current async context.

    Called by the ``jamjet worker`` process before each tool handler invocation.
    Seeds are keyed on *execution_id* so replay runs produce identical values.
    """
    _current_random.set(SeededRandom(execution_id))
    _current_uuid_gen.set(SeededUuidGen(execution_id))
    _current_clock.set(SeededClock())


def get_current_random() -> SeededRandom | None:
    """Return the seeded random for the current tool invocation, or None outside a worker."""
    return _current_random.get()


def get_current_uuid_gen() -> SeededUuidGen | None:
    """Return the seeded UUID generator for the current tool invocation, or None outside a worker."""
    return _current_uuid_gen.get()


def get_current_clock() -> SeededClock | None:
    """Return the seeded clock for the current tool invocation, or None outside a worker."""
    return _current_clock.get()
