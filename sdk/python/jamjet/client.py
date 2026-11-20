"""
JamjetClient — HTTP client for the JamJet runtime API.

Used by the CLI and the SDK to communicate with a running JamJet runtime.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any

import httpx

from jamjet.transport import EngineAuth
from jamjet.transport import JamjetAPIError as JamjetAPIError
from jamjet.transport import raise_for_status as _raise_for_status


@dataclass(frozen=True)
class ArtifactRef:
    """A reference to a content-addressed artifact in the JamJet CAS.

    Mirrors the Rust ``ArtifactRef`` returned by ``POST /artifacts``.

    Attributes:
        hash: SHA-256 hex of the stored bytes (64 lowercase hex chars).
        size: Byte length of the stored content.
        media_type: Optional MIME type recorded at store time.
    """

    hash: str
    size: int
    media_type: str | None = None


class JamjetClient:
    """
    HTTP client for the JamJet runtime REST API.

    Defaults to http://localhost:7700 (the default `jamjet dev` address).

    ``api_token`` (or ``JAMJET_TOKEN``) goes in ``Authorization: Bearer``.
    ``auth`` is an ``httpx.Auth`` for whatever stands in front of the engine,
    such as a platform that checks an identity token; it must use a header of
    its own while a JamJet token is set. Both go only to the origin of
    ``base_url``: a request to any other scheme, host or port raises
    ``CredentialScopeError`` and is not sent. Redirects are never followed.
    """

    def __init__(
        self,
        base_url: str = "http://localhost:7700",
        api_token: str | None = None,
        timeout: float = 30.0,
        *,
        auth: httpx.Auth | None = None,
    ) -> None:
        if auth is not None and not isinstance(auth, httpx.Auth):
            raise TypeError("auth must be an httpx.Auth")
        self.base_url = base_url.rstrip("/")
        token = api_token or os.environ.get("JAMJET_TOKEN")
        self._auth = EngineAuth(self.base_url, token=token, auth=auth) if token or auth is not None else None
        self._client = httpx.AsyncClient(
            base_url=self.base_url,
            timeout=timeout,
            auth=self._auth,
            follow_redirects=False,
        )

    async def _request(self, method: str, url: str, **kwargs: Any) -> httpx.Response:
        """Send one request and raise ``JamjetAPIError`` for a non-2xx answer.

        Every call goes through here. Credentials come from the client's own
        auth flow, so a call cannot bring its own ``auth`` or follow a redirect.
        """
        refused = [name for name in ("auth", "follow_redirects") if name in kwargs]
        if refused:
            raise TypeError(
                f"JamjetClient calls do not take {' or '.join(refused)}: credentials come from the client, "
                "and redirects are never followed"
            )
        response = await self._client.request(method, url, follow_redirects=False, **kwargs)
        _raise_for_status(response, credential_headers=self._auth.credential_headers if self._auth is not None else ())
        return response

    # ── Health ────────────────────────────────────────────────────────────

    async def health(self) -> dict[str, Any]:
        r = await self._request("GET", "/health")
        return r.json()

    # ── Workflows ─────────────────────────────────────────────────────────

    async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
        r = await self._request("POST", "/workflows", json={"ir": ir})
        return r.json()

    # ── Cron schedules ────────────────────────────────────────────────────

    async def create_cron_job(
        self,
        name: str,
        cron_expression: str,
        workflow_id: str,
        workflow_version: str | None = None,
        input: dict[str, Any] | None = None,
        enabled: bool = True,
    ) -> dict[str, Any]:
        from jamjet.workflow.types import DEFAULT_WORKFLOW_VERSION

        # Always name the version: a runtime from before 1.0.0 became the default
        # assumes 0.1.0 for POST /cron and would not find a default-versioned workflow.
        body: dict[str, Any] = {
            "name": name,
            "cron_expression": cron_expression,
            "workflow_id": workflow_id,
            "enabled": enabled,
            "workflow_version": workflow_version or DEFAULT_WORKFLOW_VERSION,
        }
        if input is not None:
            body["input"] = input
        r = await self._request("POST", "/cron", json=body)
        return r.json()

    async def list_cron_jobs(self) -> dict[str, Any]:
        r = await self._request("GET", "/cron")
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
        r = await self._request("POST", "/executions", json=body)
        return r.json()

    async def get_execution(self, execution_id: str) -> dict[str, Any]:
        r = await self._request("GET", f"/executions/{execution_id}")
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
        r = await self._request("GET", "/executions", params=params)
        return r.json()

    async def cancel_execution(self, execution_id: str) -> dict[str, Any]:
        r = await self._request("POST", f"/executions/{execution_id}/cancel")
        return r.json()

    async def get_events(self, execution_id: str, *, inline_artifacts: str | None = None) -> dict[str, Any]:
        """Return the run's whole event log as ``{"events": [...]}``.

        The runtime returns the log in pages (10000 events at most, with a
        ``next_after`` cursor when a page is full); this follows the cursor
        until the last page. Spilled node outputs over 256 KiB come back as
        ``$artifact`` references unless ``inline_artifacts="all"``.
        """
        events: list[Any] = []
        base: dict[str, Any] = {}
        if inline_artifacts is not None:
            base["inline_artifacts"] = inline_artifacts
        params: dict[str, Any] = dict(base)
        while True:
            r = await self._request("GET", f"/executions/{execution_id}/events", params=params)
            page = r.json()
            page_events = page.get("events") if isinstance(page, dict) else None
            if not isinstance(page_events, list):
                # Not a page of the log. Read as an empty one, it would say
                # "nothing happened" where the runtime said nothing, and a
                # check like an eval's `did_not_use` would pass on it.
                raise ValueError(f"the answer to GET /executions/{execution_id}/events has no `events` list")
            events.extend(page_events)
            next_after = page.get("next_after")
            if next_after is None:
                return {"events": events}
            params = {**base, "after": next_after}

    async def approve(
        self,
        execution_id: str,
        decision: str,
        node_id: str | None = None,
        comment: str | None = None,
        state_patch: dict[str, Any] | None = None,
        action_digest: str | None = None,
        user_id: str | None = None,
    ) -> dict[str, Any]:
        """Record a decision on a pending approval.

        ``action_digest`` is the ``action_digest`` (or, for an agent tool
        dispatch, the ``calls_hash``) from the ``context`` of the pending
        approval you reviewed, as returned by :meth:`list_approvals`. Outside
        dev mode the runtime requires it to approve, and refuses the decision
        with 409 if that request has since been replaced. Pass the value you
        actually showed the reviewer: fetching the current digest and approving
        in one step would approve whatever is pending, unseen.

        ``state_patch`` is kept for compatibility only. The runtime refuses a
        decision that carries one with HTTP 400 (approval edits are not
        supported: the action would not run on the edited state), and this
        method raises ``httpx.HTTPStatusError`` for it like any other refusal.

        ``user_id`` is the name to record as the approver. A dev-mode engine
        records it as given. With an API token the engine records the token's
        name and ignores it. It is sent only when set.
        """
        body: dict[str, Any] = {"decision": decision}
        if node_id:
            body["node_id"] = node_id
        if comment:
            body["comment"] = comment
        if state_patch:
            body["state_patch"] = state_patch
        if action_digest:
            body["action_digest"] = action_digest
        if user_id:
            body["user_id"] = user_id
        r = await self._request("POST", f"/executions/{execution_id}/approve", json=body)
        return r.json()

    async def list_approvals(self, execution_id: str) -> dict[str, Any]:
        """Pending and decided approvals for an execution.

        Returns ``{"pending": [...], "decided": [...]}`` as served by
        ``GET /executions/{id}/approvals``.
        """
        r = await self._request("GET", f"/executions/{execution_id}/approvals")
        return r.json()

    async def send_external_event(
        self,
        execution_id: str,
        correlation_key: str,
        payload: dict[str, Any],
    ) -> dict[str, Any]:
        r = await self._request(
            "POST",
            f"/executions/{execution_id}/external-event",
            json={"correlation_key": correlation_key, "payload": payload},
        )
        return r.json()

    # ── Artifacts (content-addressed store) ───────────────────────────────

    async def put_artifact(
        self,
        data: bytes,
        media_type: str | None = None,
    ) -> ArtifactRef:
        """Store raw bytes in the tenant-scoped CAS and return an ``ArtifactRef``.

        The bytes are content-addressed: storing identical bytes twice yields
        the same ``hash``. ``media_type`` is sent as the request ``Content-Type``
        and echoed back on the returned ref.
        """
        headers = {"Content-Type": media_type} if media_type else None
        r = await self._request("POST", "/artifacts", content=data, headers=headers)
        j = r.json()
        return ArtifactRef(
            hash=j["hash"],
            size=int(j["size"]),
            media_type=j.get("media_type"),
        )

    async def get_artifact(self, hash: str) -> bytes:
        """Fetch artifact bytes by hash from the tenant-scoped CAS.

        Raises ``httpx.HTTPStatusError`` (404) when no artifact with that hash
        exists for the caller's tenant.
        """
        r = await self._request("GET", f"/artifacts/{hash}")
        return r.content

    # ── Agents ────────────────────────────────────────────────────────────

    async def register_agent(self, card: dict[str, Any]) -> dict[str, Any]:
        r = await self._request("POST", "/agents", json=card)
        return r.json()

    async def list_agents(self) -> dict[str, Any]:
        r = await self._request("GET", "/agents")
        return r.json()

    async def get_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._request("GET", f"/agents/{agent_id}")
        return r.json()

    async def activate_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._request("POST", f"/agents/{agent_id}/activate")
        return r.json()

    async def deactivate_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._request("POST", f"/agents/{agent_id}/deactivate")
        return r.json()

    async def discover_agent(self, url: str) -> dict[str, Any]:
        r = await self._request("POST", "/agents/discover", json={"url": url})
        return r.json()

    # ── Work items (worker protocol) ────────────────────────────────────

    async def claim_work_item(
        self,
        worker_id: str,
        queue_types: list[str],
        lease_secs: int | None = None,
    ) -> dict[str, Any] | None:
        """Claim the next available work item.

        ``lease_secs`` asks the engine for a lease of that length (its default
        is 30 s); renewals keep it. Returns the work_item dict on success, or
        ``None`` when the queue is empty (runtime responds with
        ``{"claimed": false}``).
        """
        body: dict[str, Any] = {"worker_id": worker_id, "queue_types": queue_types}
        if lease_secs is not None:
            body["lease_secs"] = int(lease_secs)
        r = await self._request("POST", "/work-items/claim", json=body)
        data = r.json()
        if not data.get("claimed"):
            return None
        return data["work_item"]

    async def complete_work_item(
        self,
        item_id: str,
        execution_id: str | None,
        node_id: str | None,
        output: Any,
        state_patch: dict[str, Any],
        duration_ms: int = 0,
        gen_ai_model: str | None = None,
        finish_reason: str | None = None,
        lease_fence: int | None = None,
        idempotency_key: str | None = None,
        input_tokens: int | None = None,
        output_tokens: int | None = None,
        cost_usd: float | None = None,
    ) -> None:
        """Mark a work item as completed and emit a NodeCompleted event.

        ``input_tokens``, ``output_tokens`` and ``cost_usd`` are recorded on
        the event and count toward the token and cost budgets of the agent
        the node acts for. ``cost_usd`` must be a non-negative number (the
        runtime answers 400 otherwise).

        When ``lease_fence`` is provided (and non-zero), it is echoed back so the
        runtime can prove this worker still holds the lease: a stale fence (the
        item was reclaimed/replayed) is rejected with 409 and no NodeCompleted is
        emitted. A falsy/absent fence keeps the legacy unfenced behavior.
        """
        body: dict[str, Any] = {
            "output": output,
            "state_patch": state_patch,
            "duration_ms": duration_ms,
        }
        if execution_id:
            body["execution_id"] = execution_id
        if node_id:
            body["node_id"] = node_id
        if gen_ai_model:
            body["gen_ai_model"] = gen_ai_model
        if finish_reason is not None:
            body["finish_reason"] = finish_reason
        if input_tokens is not None:
            body["input_tokens"] = input_tokens
        if output_tokens is not None:
            body["output_tokens"] = output_tokens
        if cost_usd is not None:
            body["cost_usd"] = cost_usd
        if lease_fence:
            body["lease_fence"] = lease_fence
        if idempotency_key is not None:
            # Echoed from the claim so the engine records this result against it.
            # Without it nothing lands in tool_effects and a re-run fires the tool
            # again — the replay guard covers the in-process path only.
            #
            # `is not None`, not a truthiness check: the caller echoes whatever the
            # claim handed out, and silently dropping a falsy-but-present value
            # would turn a malformed key into "no key" — the failure mode this
            # whole change exists to remove, and invisible at the call site.
            body["idempotency_key"] = idempotency_key
        await self._request(
            "POST",
            f"/work-items/{item_id}/complete",
            json=body,
        )

    async def fail_work_item(self, item_id: str, error: str, lease_fence: int = 0) -> None:
        """Report that this work item's node failed.

        Echo ``lease_fence`` from the claim. With it, the runtime applies the
        same retry / backoff / dead-letter rules an expired lease gets, and
        appends ``NodeFailed`` so the execution can actually reach a terminal
        state. Without it the runtime takes the legacy path, which settles the
        item but emits nothing — leaving the node scheduled forever.

        A 409 means the fence no longer matches: another worker owns this item,
        and this one must not narrate its failure.
        """
        body: dict[str, Any] = {"error": error}
        if lease_fence:
            body["lease_fence"] = lease_fence
        await self._request(
            "POST",
            f"/work-items/{item_id}/fail",
            json=body,
        )

    async def heartbeat_work_item(
        self,
        item_id: str,
        worker_id: str,
        lease_fence: int = 0,
    ) -> dict[str, Any]:
        """Renew the lease on a claimed work item.

        ``lease_fence`` must match the value returned when the item was
        claimed; a stale fence causes the runtime to reject the renewal.

        The answer carries ``cancel_requested`` once the execution has been
        cancelled; the worker loop acts on it.
        """
        r = await self._request(
            "POST",
            f"/work-items/{item_id}/heartbeat",
            json={"worker_id": worker_id, "lease_fence": lease_fence},
        )
        try:
            body = r.json()
        except ValueError:
            return {}
        return body if isinstance(body, dict) else {}

    # ── Generic helpers (for one-off API paths not covered above) ─────────

    async def post(self, path: str, **kwargs: Any) -> dict[str, Any]:
        r = await self._request("POST", path, **kwargs)
        return r.json()

    async def get(self, path: str, **kwargs: Any) -> dict[str, Any]:
        r = await self._request("GET", path, **kwargs)
        return r.json()

    async def __aenter__(self) -> JamjetClient:
        return self

    async def __aexit__(self, *args: Any) -> None:
        await self._client.aclose()
