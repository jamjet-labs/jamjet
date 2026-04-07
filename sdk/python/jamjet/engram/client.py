"""
EngramClient — async HTTP client for the Engram memory REST API.

Wraps all /v1/memory/* endpoints with typed methods.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from typing import Any

import httpx


@dataclass
class ContextBlock:
    """Token-budgeted context block returned by context()."""

    text: str
    token_count: int
    facts_included: int
    facts_omitted: int
    tier_breakdown: dict[str, int] = field(default_factory=dict)


@dataclass
class ConsolidationResult:
    """Summary of a consolidation cycle."""

    facts_decayed: int = 0
    facts_archived: int = 0
    facts_promoted: int = 0
    facts_deduped: int = 0
    facts_summarized: int = 0
    insights_generated: int = 0
    llm_calls_used: int = 0


class EngramClient:
    """
    Async HTTP client for the Engram memory REST API.

    Usage::

        async with EngramClient("http://localhost:9090") as client:
            await client.add(
                messages=[{"role": "user", "content": "I like pizza"}],
                user_id="alice",
            )
            facts = await client.recall("pizza", user_id="alice")
            ctx = await client.context("recommend food", user_id="alice")
            print(ctx.text)
    """

    def __init__(
        self,
        base_url: str = "http://localhost:9090",
        api_token: str | None = None,
        timeout: float = 30.0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self._token = api_token or os.environ.get("ENGRAM_TOKEN")
        self._client = httpx.AsyncClient(
            base_url=self.base_url,
            timeout=timeout,
            headers=self._auth_headers(),
        )

    def _auth_headers(self) -> dict[str, str]:
        if self._token:
            return {"Authorization": f"Bearer {self._token}"}
        return {}

    async def __aenter__(self) -> EngramClient:
        return self

    async def __aexit__(self, *args: Any) -> None:
        await self._client.aclose()

    async def close(self) -> None:
        await self._client.aclose()

    # ── Health ────────────────────────────────────────────────────────────

    async def health(self) -> dict[str, Any]:
        """Check server health."""
        r = await self._client.get("/health")
        r.raise_for_status()
        return r.json()

    # ── Add ───────────────────────────────────────────────────────────────

    async def add(
        self,
        messages: list[dict[str, str]],
        *,
        user_id: str | None = None,
        org_id: str | None = None,
        session_id: str | None = None,
    ) -> dict[str, Any]:
        """Extract and store facts from conversation messages."""
        body: dict[str, Any] = {"messages": messages}
        if user_id:
            body["user_id"] = user_id
        if org_id:
            body["org_id"] = org_id
        if session_id:
            body["session_id"] = session_id
        r = await self._client.post("/v1/memory", json=body)
        r.raise_for_status()
        return r.json()

    # ── Recall ────────────────────────────────────────────────────────────

    async def recall(
        self,
        query: str,
        *,
        user_id: str | None = None,
        org_id: str | None = None,
        max_results: int = 10,
    ) -> list[dict[str, Any]]:
        """Semantic search over stored facts. Returns list of fact dicts."""
        params: dict[str, Any] = {"q": query, "max_results": max_results}
        if user_id:
            params["user_id"] = user_id
        if org_id:
            params["org_id"] = org_id
        r = await self._client.get("/v1/memory/recall", params=params)
        r.raise_for_status()
        return r.json().get("results", [])

    # ── Context ───────────────────────────────────────────────────────────

    async def context(
        self,
        query: str,
        *,
        user_id: str | None = None,
        org_id: str | None = None,
        token_budget: int = 2000,
        format: str = "system_prompt",
    ) -> ContextBlock:
        """Assemble a token-budgeted context block for LLM prompts."""
        body: dict[str, Any] = {
            "query": query,
            "token_budget": token_budget,
            "format": format,
        }
        if user_id:
            body["user_id"] = user_id
        if org_id:
            body["org_id"] = org_id
        r = await self._client.post("/v1/memory/context", json=body)
        r.raise_for_status()
        data = r.json()
        return ContextBlock(
            text=data["text"],
            token_count=data["token_count"],
            facts_included=data["facts_included"],
            facts_omitted=data["facts_omitted"],
            tier_breakdown=data.get("tier_breakdown", {}),
        )

    # ── Forget ────────────────────────────────────────────────────────────

    async def forget(self, fact_id: str, *, reason: str | None = None) -> dict[str, Any]:
        """Soft-delete a fact by ID."""
        # httpx doesn't support a body on .delete(); use .request() instead.
        body: dict[str, Any] = {}
        if reason:
            body["reason"] = reason
        r = await self._client.request(
            "DELETE",
            f"/v1/memory/facts/{fact_id}",
            json=body if body else None,
        )
        r.raise_for_status()
        return r.json()

    # ── Search ────────────────────────────────────────────────────────────

    async def search(
        self,
        query: str,
        *,
        user_id: str | None = None,
        org_id: str | None = None,
        top_k: int = 10,
    ) -> list[dict[str, Any]]:
        """Keyword search over stored facts (FTS5). Returns list of fact dicts."""
        params: dict[str, Any] = {"q": query, "top_k": top_k}
        if user_id:
            params["user_id"] = user_id
        if org_id:
            params["org_id"] = org_id
        r = await self._client.get("/v1/memory/search", params=params)
        r.raise_for_status()
        return r.json().get("results", [])

    # ── Stats ─────────────────────────────────────────────────────────────

    async def stats(self) -> dict[str, Any]:
        """Return aggregate memory statistics."""
        r = await self._client.get("/v1/memory/stats")
        r.raise_for_status()
        return r.json()

    # ── Consolidate ───────────────────────────────────────────────────────

    async def consolidate(
        self,
        *,
        user_id: str | None = None,
        org_id: str | None = None,
    ) -> ConsolidationResult:
        """Run a consolidation cycle."""
        body: dict[str, Any] = {}
        if user_id:
            body["user_id"] = user_id
        if org_id:
            body["org_id"] = org_id
        r = await self._client.post("/v1/memory/consolidate", json=body)
        r.raise_for_status()
        data = r.json()
        return ConsolidationResult(
            facts_decayed=data.get("facts_decayed", 0),
            facts_archived=data.get("facts_archived", 0),
            facts_promoted=data.get("facts_promoted", 0),
            facts_deduped=data.get("facts_deduped", 0),
            facts_summarized=data.get("facts_summarized", 0),
            insights_generated=data.get("insights_generated", 0),
            llm_calls_used=data.get("llm_calls_used", 0),
        )

    # ── Delete user data ──────────────────────────────────────────────────

    async def delete_user(self, user_id: str) -> dict[str, Any]:
        """GDPR: delete all memory data for a user."""
        r = await self._client.delete(f"/v1/memory/users/{user_id}")
        r.raise_for_status()
        return r.json()
