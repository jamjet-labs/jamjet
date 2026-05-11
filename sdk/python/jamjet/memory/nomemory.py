"""NoMemory stub for MemoryConfig(enabled=False) or backend='none'."""

from __future__ import annotations

from typing import Any


class NoMemory:
    """Raises a clear error on every method. Surfaces config bugs early."""

    def _bail(self, op: str) -> Any:
        raise RuntimeError(
            f"self.memory.{op}() called but memory is disabled "
            "(MemoryConfig.enabled=False or backend='none'). "
            "Enable memory by passing memory=MemoryConfig() to @DurableAgent."
        )

    async def record(self, *a: Any, **k: Any) -> Any:
        return self._bail("record")

    async def record_message(self, *a: Any, **k: Any) -> Any:
        return self._bail("record_message")

    async def recall(self, *a: Any, **k: Any) -> Any:
        return self._bail("recall")

    async def context(self, *a: Any, **k: Any) -> Any:
        return self._bail("context")

    async def synthesize(self, *a: Any, **k: Any) -> Any:
        return self._bail("synthesize")

    async def ask(self, *a: Any, **k: Any) -> Any:
        return self._bail("ask")
