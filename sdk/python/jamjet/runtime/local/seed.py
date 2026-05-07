"""Seeded random/uuid/clock helpers injected as self.random / self.uuid_gen / self.now.

These are deterministic across runs given the same execution_id seed. Author code
that wants replay-faithful non-determinism must use these instead of stdlib calls.
"""
from __future__ import annotations

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
        self._t = (
            datetime.fromisoformat(start_iso) if start_iso
            else datetime.now(tz=UTC)
        )

    def now(self) -> datetime:
        out = self._t
        self._t = self._t + timedelta(milliseconds=1)
        return out
