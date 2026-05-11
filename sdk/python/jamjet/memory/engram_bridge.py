"""AgentMemory — what self.memory IS inside a @DurableAgent. Wraps an Engram instance."""

from __future__ import annotations

from contextlib import contextmanager
from typing import Any

from engram import ChatMessage, Engram, Fact, Reader, RuleBasedClassifier
from engram import Scope as EngramScope
from engram.llm.openai import OpenAILLM

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

    async def recall(
        self,
        query: str,
        *,
        top_k: int = 10,
        role_filter: tuple[str, ...] | None = None,
    ) -> list[Any]:
        results = await self._engram.recall(
            query,
            user_id=self._scope.user_id,
            org_id=self._scope.org_id,
            top_k=top_k,
        )
        rf = role_filter if role_filter is not None else self._config.default_role_filter
        if rf:
            results = [r for r in results if r.fact.metadata.get("role") in rf]
        return results

    async def context(
        self,
        query: str,
        *,
        token_budget: int | None = None,
        role_filter: tuple[str, ...] | None = None,
        decompose: bool | None = None,
    ) -> str:
        classifier = RuleBasedClassifier() if self._config.use_classifier else None
        return await self._engram.context(
            query,
            user_id=self._scope.user_id,
            org_id=self._scope.org_id,
            token_budget=token_budget if token_budget is not None else self._config.default_token_budget,
            classifier=classifier,
            decompose=self._config.decompose if decompose is None else decompose,
            role_filter=role_filter if role_filter is not None else self._config.default_role_filter,
        )

    async def synthesize(
        self,
        query: str,
        *,
        role_filter: tuple[str, ...] | None = None,
    ) -> str:
        """Reader-mode synthesis. Requires MemoryConfig.llm to construct an Engram LLM client.

        If MemoryConfig.llm is unset, raises RuntimeError. Phase 3 will add cleaner
        wiring via the runtime injector.
        """
        if self._config.llm is None:
            raise RuntimeError(
                "synthesize() requires MemoryConfig.llm to be set so we can construct "
                "an Engram-compatible LLM client. Set memory=MemoryConfig(llm=LLMConfig(...))."
            )
        if self._config.llm.provider != "openai":
            raise NotImplementedError(
                f"synthesize() with provider={self._config.llm.provider!r} lands in Phase 3. Use 'openai' for now."
            )
        llm = OpenAILLM(model=self._config.llm.model)
        ctx = await self.context(query, role_filter=role_filter)
        reader = Reader(llm=llm, mode="synthesis")
        result = await reader.read(query, ctx, scope=self._scope)
        return result.answer

    async def ask(
        self,
        query: str,
        *,
        mode: str | None = None,
        **overrides: Any,
    ) -> Any:
        m = mode or self._config.default_mode
        if m == "recall":
            return await self.recall(query, **overrides)
        if m == "context":
            return await self.context(query, **overrides)
        if m in ("synthesize", "synthesis"):
            return await self.synthesize(query, **overrides)
        raise ValueError(f"Unknown memory mode {m!r}")

    @contextmanager
    def as_scope(self, *, user_id: str | None = None, org_id: str | None = None) -> Any:
        old = self._scope
        new = EngramScope(
            org_id=org_id if org_id is not None else old.org_id,
            user_id=user_id if user_id is not None else old.user_id,
        )
        self._scope = new
        try:
            yield self
        finally:
            self._scope = old
