"""
JamjetClient — HTTP client for the JamJet runtime API.

Used by the CLI and the SDK to communicate with a running JamJet runtime.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any

import httpx

# How much of an error body goes into the exception message. Runtime errors can
# quote the request back (a value, a key), so keep only the start of it.
_MAX_ERROR_DETAIL = 500
_BIDI_CONTROLS = frozenset("\u200e\u200f\u202a\u202b\u202c\u202d\u202e\u2066\u2067\u2068\u2069")


def _clean_detail(text: str) -> str:
    """One line of plain text: control and bidi characters shown escaped,
    whitespace collapsed, capped at ``_MAX_ERROR_DETAIL`` characters."""
    out = []
    for ch in text:
        code = ord(ch)
        if ch in "\n\r\t":
            out.append(" ")
        elif code < 0x20 or 0x7F <= code <= 0x9F:
            out.append(f"\\x{code:02x}")
        elif ch in _BIDI_CONTROLS:
            out.append(f"\\u{code:04x}")
        else:
            out.append(ch)
    cleaned = " ".join("".join(out).split())
    if len(cleaned) > _MAX_ERROR_DETAIL:
        cleaned = cleaned[:_MAX_ERROR_DETAIL] + "..."
    return cleaned


class JamjetAPIError(httpx.HTTPStatusError):
    """A non-2xx answer from the runtime, with the server's reason attached.

    A subclass of ``httpx.HTTPStatusError``, so existing ``except`` clauses and
    ``.response.status_code`` checks keep working. ``detail`` holds the server's
    message (the ``error`` field of its JSON body, or the body text) as one line
    of at most 500 characters with control characters escaped. The raw body is
    still on ``.response``.
    """

    def __init__(self, message: str, *, request: httpx.Request, response: httpx.Response, detail: str) -> None:
        super().__init__(message, request=request, response=response)
        self.detail = detail


def _error_detail(response: httpx.Response) -> str:
    """The server's reason for a failed request, or "" when the body is empty."""
    try:
        text = response.text.strip()
    except Exception:  # an undecodable body is no reason to hide the status
        return ""
    if not text:
        return ""
    try:
        body = response.json()
    except ValueError:
        body = None
    if isinstance(body, dict):
        for key in ("error", "detail", "message"):
            value = body.get(key)
            if isinstance(value, str) and value.strip():
                text = value.strip()
                break
    return _clean_detail(text)


def _raise_for_status(response: httpx.Response) -> None:
    """``response.raise_for_status()``, but the raised error carries the
    server's message instead of only the status line."""
    try:
        response.raise_for_status()
    except httpx.HTTPStatusError as exc:
        detail = _error_detail(response)
        request = response.request
        message = f"{response.status_code} {response.reason_phrase} for {request.method} {request.url}"
        if detail:
            message = f"{message}: {detail}"
        raise JamjetAPIError(message, request=request, response=response, detail=detail) from exc


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
        _raise_for_status(r)
        return r.json()

    # ── Workflows ─────────────────────────────────────────────────────────

    async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
        r = await self._client.post("/workflows", json={"ir": ir})
        _raise_for_status(r)
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
        r = await self._client.post("/cron", json=body)
        _raise_for_status(r)
        return r.json()

    async def list_cron_jobs(self) -> dict[str, Any]:
        r = await self._client.get("/cron")
        _raise_for_status(r)
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
        _raise_for_status(r)
        return r.json()

    async def get_execution(self, execution_id: str) -> dict[str, Any]:
        r = await self._client.get(f"/executions/{execution_id}")
        _raise_for_status(r)
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
        _raise_for_status(r)
        return r.json()

    async def cancel_execution(self, execution_id: str) -> dict[str, Any]:
        r = await self._client.post(f"/executions/{execution_id}/cancel")
        _raise_for_status(r)
        return r.json()

    async def get_events(self, execution_id: str) -> dict[str, Any]:
        """Return the run's whole event log as ``{"events": [...]}``.

        The runtime returns the log in pages (10000 events at most, with a
        ``next_after`` cursor when there are more); this follows the cursor
        until the last page.
        """
        events: list[Any] = []
        params: dict[str, Any] = {}
        while True:
            r = await self._client.get(f"/executions/{execution_id}/events", params=params)
            _raise_for_status(r)
            page = r.json()
            events.extend(page.get("events", []))
            next_after = page.get("next_after")
            if next_after is None:
                return {"events": events}
            params = {"after": next_after}

    async def approve(
        self,
        execution_id: str,
        decision: str,
        node_id: str | None = None,
        comment: str | None = None,
        state_patch: dict[str, Any] | None = None,
        action_digest: str | None = None,
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
        r = await self._client.post(f"/executions/{execution_id}/approve", json=body)
        _raise_for_status(r)
        return r.json()

    async def list_approvals(self, execution_id: str) -> dict[str, Any]:
        """Pending and decided approvals for an execution.

        Returns ``{"pending": [...], "decided": [...]}`` as served by
        ``GET /executions/{id}/approvals``.
        """
        r = await self._client.get(f"/executions/{execution_id}/approvals")
        _raise_for_status(r)
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
        _raise_for_status(r)
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
        r = await self._client.post("/artifacts", content=data, headers=headers)
        _raise_for_status(r)
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
        r = await self._client.get(f"/artifacts/{hash}")
        _raise_for_status(r)
        return r.content

    # ── Agents ────────────────────────────────────────────────────────────

    async def register_agent(self, card: dict[str, Any]) -> dict[str, Any]:
        r = await self._client.post("/agents", json=card)
        _raise_for_status(r)
        return r.json()

    async def list_agents(self) -> dict[str, Any]:
        r = await self._client.get("/agents")
        _raise_for_status(r)
        return r.json()

    async def get_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._client.get(f"/agents/{agent_id}")
        _raise_for_status(r)
        return r.json()

    async def activate_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._client.post(f"/agents/{agent_id}/activate")
        _raise_for_status(r)
        return r.json()

    async def deactivate_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._client.post(f"/agents/{agent_id}/deactivate")
        _raise_for_status(r)
        return r.json()

    async def discover_agent(self, url: str) -> dict[str, Any]:
        r = await self._client.post("/agents/discover", json={"url": url})
        _raise_for_status(r)
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
        r = await self._client.post("/work-items/claim", json=body)
        _raise_for_status(r)
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
        r = await self._client.post(
            f"/work-items/{item_id}/complete",
            json=body,
        )
        _raise_for_status(r)

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
        r = await self._client.post(
            f"/work-items/{item_id}/fail",
            json=body,
        )
        _raise_for_status(r)

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
        r = await self._client.post(
            f"/work-items/{item_id}/heartbeat",
            json={"worker_id": worker_id, "lease_fence": lease_fence},
        )
        _raise_for_status(r)
        try:
            body = r.json()
        except ValueError:
            return {}
        return body if isinstance(body, dict) else {}

    # ── Generic helpers (for one-off API paths not covered above) ─────────

    async def post(self, path: str, **kwargs: Any) -> dict[str, Any]:
        r = await self._client.post(path, **kwargs)
        _raise_for_status(r)
        return r.json()

    async def get(self, path: str, **kwargs: Any) -> dict[str, Any]:
        r = await self._client.get(path, **kwargs)
        _raise_for_status(r)
        return r.json()

    async def __aenter__(self) -> JamjetClient:
        return self

    async def __aexit__(self, *args: Any) -> None:
        await self._client.aclose()
