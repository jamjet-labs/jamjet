"""
JamjetClient — HTTP client for the JamJet runtime API.

Used by the CLI and the SDK to communicate with a running JamJet runtime.
"""

from __future__ import annotations

import os
from typing import Any

import httpx


class JamjetClient:
    """
    HTTP client for the JamJet runtime REST API.

    Defaults to http://localhost:7700 (the default `jamjet dev` address).
    """

    def __init__(
        self,
        base_url: str = "http://localhost:7700",
        api_token: str | None = None,
        timeout: float = 30.0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self._token = api_token or os.environ.get("JAMJET_TOKEN")
        self._client = httpx.AsyncClient(
            base_url=self.base_url,
            timeout=timeout,
            headers=self._auth_headers(),
        )

    def _auth_headers(self) -> dict[str, str]:
        if self._token:
            return {"Authorization": f"Bearer {self._token}"}
        return {}

    # ── Health ────────────────────────────────────────────────────────────

    async def health(self) -> dict[str, Any]:
        r = await self._client.get("/health")
        r.raise_for_status()
        return r.json()

    # ── Workflows ─────────────────────────────────────────────────────────

    async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
        r = await self._client.post("/workflows", json={"ir": ir})
        r.raise_for_status()
        return r.json()

    # ── Executions ────────────────────────────────────────────────────────

    async def start_execution(
        self,
        workflow_id: str,
        input: dict[str, Any],
        workflow_version: str | None = None,
    ) -> dict[str, Any]:
        body: dict[str, Any] = {"workflow_id": workflow_id, "input": input}
        if workflow_version:
            body["workflow_version"] = workflow_version
        r = await self._client.post("/executions", json=body)
        r.raise_for_status()
        return r.json()

    async def get_execution(self, execution_id: str) -> dict[str, Any]:
        r = await self._client.get(f"/executions/{execution_id}")
        r.raise_for_status()
        return r.json()

    async def list_executions(
        self,
        status: str | None = None,
        limit: int = 50,
        offset: int = 0,
    ) -> dict[str, Any]:
        params: dict[str, Any] = {"limit": limit, "offset": offset}
        if status:
            params["status"] = status
        r = await self._client.get("/executions", params=params)
        r.raise_for_status()
        return r.json()

    async def cancel_execution(self, execution_id: str) -> dict[str, Any]:
        r = await self._client.post(f"/executions/{execution_id}/cancel")
        r.raise_for_status()
        return r.json()

    async def get_events(self, execution_id: str) -> dict[str, Any]:
        r = await self._client.get(f"/executions/{execution_id}/events")
        r.raise_for_status()
        return r.json()

    async def approve(
        self,
        execution_id: str,
        decision: str,
        comment: str | None = None,
        state_patch: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        body: dict[str, Any] = {"decision": decision}
        if comment:
            body["comment"] = comment
        if state_patch:
            body["state_patch"] = state_patch
        r = await self._client.post(f"/executions/{execution_id}/approve", json=body)
        r.raise_for_status()
        return r.json()

    async def send_external_event(
        self,
        execution_id: str,
        correlation_key: str,
        payload: dict[str, Any],
    ) -> dict[str, Any]:
        r = await self._client.post(
            f"/executions/{execution_id}/external-event",
            json={"correlation_key": correlation_key, "payload": payload},
        )
        r.raise_for_status()
        return r.json()

    # ── Agents ────────────────────────────────────────────────────────────

    async def register_agent(self, card: dict[str, Any]) -> dict[str, Any]:
        r = await self._client.post("/agents", json=card)
        r.raise_for_status()
        return r.json()

    async def list_agents(self) -> dict[str, Any]:
        r = await self._client.get("/agents")
        r.raise_for_status()
        return r.json()

    async def get_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._client.get(f"/agents/{agent_id}")
        r.raise_for_status()
        return r.json()

    async def activate_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._client.post(f"/agents/{agent_id}/activate")
        r.raise_for_status()
        return r.json()

    async def deactivate_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._client.post(f"/agents/{agent_id}/deactivate")
        r.raise_for_status()
        return r.json()

    async def discover_agent(self, url: str) -> dict[str, Any]:
        r = await self._client.post("/agents/discover", json={"url": url})
        r.raise_for_status()
        return r.json()

    async def get_agent_trace(self, agent_id: str, limit: int = 20) -> dict[str, Any]:
        r = await self._client.get(f"/agents/{agent_id}/trace", params={"limit": limit})
        r.raise_for_status()
        return r.json()

    # ── Generic helpers (for one-off API paths not covered above) ─────────

    async def post(self, path: str, **kwargs: Any) -> dict[str, Any]:
        r = await self._client.post(path, **kwargs)
        r.raise_for_status()
        return r.json()

    async def get(self, path: str, **kwargs: Any) -> dict[str, Any]:
        r = await self._client.get(path, **kwargs)
        r.raise_for_status()
        return r.json()

    async def __aenter__(self) -> JamjetClient:
        return self

    async def __aexit__(self, *args: Any) -> None:
        await self._client.aclose()
