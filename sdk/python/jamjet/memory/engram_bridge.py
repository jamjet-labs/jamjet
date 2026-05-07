"""AgentMemory — what self.memory IS inside a @DurableAgent. Wraps an Engram instance."""
from __future__ import annotations

from typing import Any

from engram import ChatMessage, Engram, Fact
from engram import Scope as EngramScope

from jamjet.spec import MemoryConfig


class AgentMemory:
    """Bridge over an Engram instance. Read paths land in T22; this is write-only."""

    def __init__(
        self,
        engram: Engram,
        *,
        scope: EngramScope,
        config: MemoryConfig,
        session_id: str | None = None,
    ) -> None:
        self._engram = engram
        self._scope = scope
        self._config = config
        self._session_id = session_id

    async def record(
        self,
        text: str,
        *,
        role: str | None = None,
        category: str | None = None,
        confidence: float = 1.0,
        metadata: dict[str, Any] | None = None,
    ) -> Fact:
        return await self._engram.record(
            text,
            user_id=self._scope.user_id,
            org_id=self._scope.org_id,
            session_id=self._session_id,
            category=category,
            confidence=confidence,
            metadata=metadata,
            role=role,
        )

    async def record_message(self, content: str, *, role: str = "user") -> ChatMessage:
        return await self._engram.record_message(
            content,
            role=role,
            session_id=self._session_id or "default",
            user_id=self._scope.user_id,
            org_id=self._scope.org_id,
        )
