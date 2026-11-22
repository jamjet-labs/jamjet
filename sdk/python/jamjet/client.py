"""
JamjetClient — HTTP client for the JamJet runtime API.

Used by the CLI and the SDK to communicate with a running JamJet runtime.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any, Literal

import httpx

from jamjet.transport import EngineAuth, EngineFeatureMissing, RetryPolicy, RunKeyConflict
from jamjet.transport import JamjetAPIError as JamjetAPIError
from jamjet.transport import body_is_replayable as _body_is_replayable
from jamjet.transport import raise_for_status as _raise_for_status
from jamjet.transport import send_with_retries as _send_with_retries

# The engine's rule for a run_key: 1 to 200 bytes of these characters, taken
# as sent (never trimmed or normalised).
_MAX_RUN_KEY = 200
_RUN_KEY_CHARS = frozenset("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._:/@+=-")
_REUSE = ("never", "failed_only")


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

    ``retry`` (a ``RetryPolicy``; ``RetryPolicy.off()`` sends every call once)
    sends a call again when that cannot do it twice. A request that never left
    and a 429 are sent again for every call. A call that is safe to repeat
    (every GET, a heartbeat, a complete or fail with a lease fence, registering
    a workflow, storing an artifact, a start with a ``run_key``) is also sent
    again after a lost answer or a 5xx. Any other call whose answer was lost
    raises ``OutcomeUnknown`` and is not sent again: check whether it took
    effect before you repeat it. A call whose body can be read only once (an
    iterator, or a file that cannot seek) is sent once.
    """

    def __init__(
        self,
        base_url: str = "http://localhost:7700",
        api_token: str | None = None,
        timeout: float = 30.0,
        *,
        auth: httpx.Auth | None = None,
        retry: RetryPolicy | None = None,
    ) -> None:
        if auth is not None and not isinstance(auth, httpx.Auth):
            raise TypeError("auth must be an httpx.Auth")
        if retry is not None and not isinstance(retry, RetryPolicy):
            raise TypeError("retry must be a RetryPolicy")
        self._retry = RetryPolicy() if retry is None else retry
        self._health: dict[str, Any] | None = None
        self.base_url = base_url.rstrip("/")
        token = api_token or os.environ.get("JAMJET_TOKEN")
        self._auth = EngineAuth(self.base_url, token=token, auth=auth) if token or auth is not None else None
        self._client = httpx.AsyncClient(
            base_url=self.base_url,
            timeout=timeout,
            auth=self._auth,
            follow_redirects=False,
        )

    async def _request(
        self, method: str, url: str, *, safe: bool, operation: str, advice: str | None = None, **kwargs: Any
    ) -> httpx.Response:
        """Send one call and raise ``JamjetAPIError`` for a non-2xx answer.

        Every call goes through here. Credentials come from the client's own
        auth flow, so a call cannot bring its own ``auth`` or follow a redirect.
        ``safe`` says whether the call may be sent again after it may have
        taken effect; ``operation`` names it in ``OutcomeUnknown`` and
        ``advice`` ends that message.
        """
        refused = [name for name in ("auth", "follow_redirects") if name in kwargs]
        if refused:
            raise TypeError(
                f"JamjetClient calls do not take {' or '.join(refused)}: credentials come from the client, "
                "and redirects are never followed"
            )
        return await _send_with_retries(
            lambda: self._client.request(method, url, follow_redirects=False, **kwargs),
            check=self._check,
            safe=safe,
            replayable=_body_is_replayable(
                content=kwargs.get("content"), data=kwargs.get("data"), files=kwargs.get("files")
            ),
            operation=operation,
            policy=self._retry,
            credential_headers=self._credential_headers,
            advice=advice,
        )

    def _credential_headers(self) -> frozenset[str]:
        return self._auth.credential_headers if self._auth is not None else frozenset()

    def _check(self, response: httpx.Response) -> None:
        _raise_for_status(response, credential_headers=self._credential_headers())

    # ── Health ────────────────────────────────────────────────────────────

    async def health(self) -> dict[str, Any]:
        r = await self._request("GET", "/health", safe=True, operation="health")
        return r.json()

    async def engine_features(self) -> frozenset[str]:
        """The features the engine lists in ``/health``, read once per client.

        An engine that lists none (an older build) gives an empty set.
        """
        if self._health is None:
            self._health = await self.health()
        listed = self._health.get("features") if isinstance(self._health, dict) else None
        if not isinstance(listed, list):
            return frozenset()
        return frozenset(f for f in listed if isinstance(f, str))

    async def require_features(self, *features: str) -> None:
        """Raise ``EngineFeatureMissing`` for the first of ``features`` the
        engine does not list in ``/health``. Sends nothing but that read."""
        listed = await self.engine_features()
        for feature in features:
            if feature not in listed:
                raise EngineFeatureMissing(feature, engine_commit=self._engine_commit())

    def _engine_commit(self) -> str | None:
        commit = (self._health or {}).get("commit") if isinstance(self._health, dict) else None
        return commit if isinstance(commit, str) else None

    # ── Request-driven mode ───────────────────────────────────────────────

    async def tick(self, *, budget_ms: int | None = None) -> dict[str, Any]:
        """Ask an engine with its loops off to do one round of background work.

        ``POST /ops/tick`` needs the operator role. A tick that is already
        running elsewhere answers ``{"ran": false, "reason": "busy", ...}``.
        A repeated tick is harmless, so a lost answer is retried. Raises
        ``EngineFeatureMissing("ops_tick")`` when the engine has no such route.
        """
        body: dict[str, Any] = {} if budget_ms is None else {"budget_ms": budget_ms}
        try:
            r = await self._request("POST", "/ops/tick", json=body, safe=True, operation="tick")
        except JamjetAPIError as exc:
            if exc.response.status_code == 404:
                raise EngineFeatureMissing("ops_tick", engine_commit=self._engine_commit()) from exc
            raise
        return r.json()

    # ── Workflows ─────────────────────────────────────────────────────────

    async def create_workflow(self, ir: dict[str, Any]) -> dict[str, Any]:
        # Registering the same IR again changes nothing.
        r = await self._request("POST", "/workflows", json={"ir": ir}, safe=True, operation="create_workflow")
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
        r = await self._request("POST", "/cron", json=body, safe=False, operation="create_cron_job")
        return r.json()

    async def list_cron_jobs(self) -> dict[str, Any]:
        r = await self._request("GET", "/cron", safe=True, operation="list_cron_jobs")
        return r.json()

    # ── Executions ────────────────────────────────────────────────────────

    async def start_execution(
        self,
        workflow_id: str,
        input: dict[str, Any],
        workflow_version: str | None = None,
        *,
        run_key: str | None = None,
        reuse: Literal["never", "failed_only"] | None = None,
    ) -> dict[str, Any]:
        """Start a run.

        With ``run_key``, a start sent again returns the run the first one
        started instead of a second run: 201 for the first start, 200 with the
        same ``execution_id`` after. So a start with a key is sent again after
        a lost answer. The key is 1 to 200 bytes of ``A-Z a-z 0-9 . _ : / @ + = -``
        and is checked here, before anything is sent. ``reuse`` is
        ``"never"`` (the default) or ``"failed_only"``: a new run starts under
        the key once the run it names ended without success. A start with
        ``"failed_only"`` is not sent again after a lost answer, since the
        second send could start a new run.

        An engine that does not know ``run_key`` drops it and starts a new run
        on every retry. So before the first start with a key the client reads
        ``/health`` and raises ``EngineFeatureMissing("run_key")`` when the
        engine does not list it; nothing is written. An answer that does not
        echo the key raises ``JamjetAPIError`` naming the run that started. The
        same key with another workflow, version or input raises
        ``RunKeyConflict``.
        """
        if run_key is None:
            if reuse is not None:
                raise ValueError("reuse applies to a start with a run_key only")
        else:
            _check_run_key(run_key)
            if reuse is not None and reuse not in _REUSE:
                raise ValueError(f"reuse must be 'never' or 'failed_only', not {reuse!r}")
            await self.require_features("run_key")
        body: dict[str, Any] = {"workflow_id": workflow_id, "input": input}
        if workflow_version:
            body["workflow_version"] = workflow_version
        if run_key is not None:
            body["run_key"] = run_key
        if reuse is not None:
            body["reuse"] = reuse
        # Without a key, a second send could start a second run. With one, the
        # engine answers it with the first run, unless failed_only lets the
        # key move to a new run in between.
        safe = run_key is not None and reuse != "failed_only"
        if run_key is None:
            advice = "Start it with a run_key to make a start safe to send again."
        else:
            advice = f"Look the run up with find_execution({run_key!r}) before you start it again."
        try:
            r = await self._request(
                "POST", "/executions", json=body, safe=safe, operation="start_execution", advice=advice
            )
        except JamjetAPIError as exc:
            if run_key is None or exc.response.status_code != 409:
                raise
            raise await self._run_key_conflict(exc, run_key) from exc
        answer = r.json()
        if run_key is not None and not (isinstance(answer, dict) and answer.get("run_key") == run_key):
            started = answer.get("execution_id") if isinstance(answer, dict) else None
            message = (
                "this engine does not support run_key; upgrade it before relying on it. "
                f"It did not echo run_key {run_key!r}, and it started run {started!r}"
            )
            raise JamjetAPIError(message, request=r.request, response=r, detail=message)
        return answer

    async def _run_key_conflict(self, exc: JamjetAPIError, run_key: str) -> JamjetAPIError:
        """The error for a 409 on a keyed start. It is ``RunKeyConflict`` when
        the key holds a run; any other 409 (a version that cannot be started)
        is returned as it is, and so is one whose lookup fails."""
        try:
            held = await self.find_execution(run_key)
        except Exception:  # noqa: BLE001 - the 409 is the error to raise; the lookup only names the run
            return exc
        if held is None:
            return exc
        existing = held.get("execution_id")
        return RunKeyConflict(
            str(exc),
            request=exc.request,
            response=exc.response,
            detail=exc.detail,
            existing_execution_id=existing if isinstance(existing, str) else None,
        )

    async def find_execution(self, run_key: str) -> dict[str, Any] | None:
        """The run ``run_key`` names, as ``GET /executions?run_key=`` lists it,
        or None when the key names no run.

        An engine that does not know the lookup ignores the query and lists
        every run, so an answer that does not echo the key raises
        ``EngineFeatureMissing("run_key")``.
        """
        _check_run_key(run_key)
        r = await self._request(
            "GET", "/executions", params={"run_key": run_key}, safe=True, operation="find_execution"
        )
        answer = r.json()
        rows = answer.get("executions") if isinstance(answer, dict) else None
        if not (isinstance(answer, dict) and answer.get("run_key") == run_key):
            raise EngineFeatureMissing("run_key", engine_commit=self._engine_commit())
        if not isinstance(rows, list) or len(rows) > 1 or not all(isinstance(row, dict) for row in rows):
            raise ValueError(f"the answer to GET /executions?run_key= is not one run or none: {answer!r}")
        return rows[0] if rows else None

    async def get_execution(self, execution_id: str) -> dict[str, Any]:
        r = await self._request("GET", f"/executions/{execution_id}", safe=True, operation="get_execution")
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
        r = await self._request("GET", "/executions", params=params, safe=True, operation="list_executions")
        return r.json()

    async def cancel_execution(self, execution_id: str) -> dict[str, Any]:
        r = await self._request("POST", f"/executions/{execution_id}/cancel", safe=False, operation="cancel_execution")
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
            r = await self._request(
                "GET", f"/executions/{execution_id}/events", params=params, safe=True, operation="get_events"
            )
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
        request_sequence: int | None = None,
        choice: str | None = None,
        note: str | None = None,
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

        ``request_sequence`` is the sequence of the request you reviewed
        (``request_sequence`` in :meth:`list_pending_approvals`, ``sequence``
        in :meth:`list_approvals`). The engine refuses the decision with 409
        if the node has asked again since. Send it when you retry a decision
        whose answer was lost: without it, a retry after the node asked again
        with the same digest decides the new request.

        ``choice`` is the id of the option you picked. A ``human_approval``
        node that declares ``options`` needs one on an approval, among the ids
        the pending request lists (``options`` in :meth:`list_approvals` and
        :meth:`list_pending_approvals`). A request without options, and a
        rejection, take none. ``note`` is a free text of at most 4 KiB, kept
        with the decision. ``comment`` is the old name of ``note`` and is sent
        as it is; the engine refuses a request whose two differ.

        Returns the engine's answer unchanged: ``execution_id``, ``node_id``,
        ``accepted``, ``repeat`` (true when the same decision was already
        recorded; nothing is written again), ``sequence`` (the decision's place
        in the log) and ``waiting_for`` (what the run waits for next, or null).
        """
        body: dict[str, Any] = {"decision": decision}
        if node_id:
            body["node_id"] = node_id
        if comment:
            body["comment"] = comment
        if note:
            body["note"] = note
        if choice is not None:
            body["choice"] = choice
        if state_patch:
            body["state_patch"] = state_patch
        if action_digest:
            body["action_digest"] = action_digest
        if user_id:
            body["user_id"] = user_id
        if request_sequence is not None:
            body["request_sequence"] = request_sequence
        r = await self._request(
            "POST", f"/executions/{execution_id}/approve", json=body, safe=False, operation="approve"
        )
        return r.json()

    async def list_approvals(self, execution_id: str) -> dict[str, Any]:
        """Pending and decided approvals for an execution.

        Returns ``{"pending": [...], "decided": [...]}`` as served by
        ``GET /executions/{id}/approvals``.
        """
        r = await self._request("GET", f"/executions/{execution_id}/approvals", safe=True, operation="list_approvals")
        return r.json()

    async def list_pending_approvals(self, limit: int | None = None, after: str | None = None) -> dict[str, Any]:
        """Pending approvals across runs, oldest first.

        Returns ``{"approvals": [...], "next_after": ...}`` as served by
        ``GET /approvals``. Each row carries the ``action_digest`` to echo in
        :meth:`approve`. ``next_after`` is set when a page is full; pass it back
        as ``after`` for the next page. The list reads the approvals read model,
        which can lag the log by one projector tick; :meth:`approve` decides.

        An engine from before this list answers ``GET /approvals`` with its web
        page. That raises :class:`JamjetAPIError` asking for an upgrade.
        """
        params: dict[str, Any] = {}
        if limit is not None:
            params["limit"] = limit
        if after is not None:
            params["after"] = after
        r = await self._request("GET", "/approvals", params=params, safe=True, operation="list_pending_approvals")
        try:
            body = r.json()
        except ValueError:
            body = None
        if not isinstance(body, dict) or not isinstance(body.get("approvals"), list):
            detail = "this engine does not list approvals across runs; upgrade it"
            raise JamjetAPIError(
                f"GET /approvals: {detail}",
                request=r.request,
                response=r,
                detail=detail,
            )
        return body

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
            safe=False,
            operation="send_external_event",
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
        # Content-addressed: the same bytes stored twice are one artifact.
        r = await self._request(
            "POST", "/artifacts", content=data, headers=headers, safe=True, operation="put_artifact"
        )
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
        r = await self._request("GET", f"/artifacts/{hash}", safe=True, operation="get_artifact")
        return r.content

    # ── Agents ────────────────────────────────────────────────────────────

    async def register_agent(self, card: dict[str, Any]) -> dict[str, Any]:
        r = await self._request("POST", "/agents", json=card, safe=False, operation="register_agent")
        return r.json()

    async def list_agents(self) -> dict[str, Any]:
        r = await self._request("GET", "/agents", safe=True, operation="list_agents")
        return r.json()

    async def get_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._request("GET", f"/agents/{agent_id}", safe=True, operation="get_agent")
        return r.json()

    async def activate_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._request("POST", f"/agents/{agent_id}/activate", safe=False, operation="activate_agent")
        return r.json()

    async def deactivate_agent(self, agent_id: str) -> dict[str, Any]:
        r = await self._request("POST", f"/agents/{agent_id}/deactivate", safe=False, operation="deactivate_agent")
        return r.json()

    async def discover_agent(self, url: str) -> dict[str, Any]:
        r = await self._request("POST", "/agents/discover", json={"url": url}, safe=False, operation="discover_agent")
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
        # Sent again, it could claim a second item while the first one's lease runs.
        r = await self._request("POST", "/work-items/claim", json=body, safe=False, operation="claim_work_item")
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
        the event and count toward the run's ``token_budget`` and
        ``cost_budget_usd`` and the budgets of the agent the node acts for.
        ``cost_usd`` must be a number from 0 to 10000, in USD (the runtime
        answers 400 otherwise). Under a dollar cap, a fenced completion with a
        cost the runtime cannot count also fails the step for good.

        When ``lease_fence`` is provided (and non-zero), it is echoed back so the
        runtime can prove this worker still holds the lease: a stale fence (the
        item was reclaimed/replayed) is rejected with 409 and no NodeCompleted is
        emitted. A falsy/absent fence keeps the legacy unfenced behavior, which
        settles only an item still claimed: sent again, it is refused with 409.
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
        # Fenced, a second send cannot complete the node twice: once the first
        # lands, the engine refuses the fence with 409. Unfenced, it could.
        await self._request(
            "POST",
            f"/work-items/{item_id}/complete",
            json=body,
            safe=bool(lease_fence),
            operation="complete_work_item",
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
        # Fenced, a second send is refused with 409 once the first lands.
        await self._request(
            "POST",
            f"/work-items/{item_id}/fail",
            json=body,
            safe=bool(lease_fence),
            operation="fail_work_item",
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
            safe=True,
            operation="heartbeat_work_item",
        )
        try:
            body = r.json()
        except ValueError:
            return {}
        return body if isinstance(body, dict) else {}

    # ── Generic helpers (for one-off API paths not covered above) ─────────

    async def post(self, path: str, **kwargs: Any) -> dict[str, Any]:
        """POST to any engine path. Never sent again once it may have taken effect."""
        r = await self._request("POST", path, safe=False, operation=_operation("POST", path), **kwargs)
        return r.json()

    async def get(self, path: str, **kwargs: Any) -> dict[str, Any]:
        r = await self._request("GET", path, safe=True, operation=_operation("GET", path), **kwargs)
        return r.json()

    async def __aenter__(self) -> JamjetClient:
        return self

    async def __aexit__(self, *args: Any) -> None:
        await self._client.aclose()


def _operation(method: str, path: str) -> str:
    """How ``OutcomeUnknown`` names a generic call: the method and the path, without host or query."""
    return f"{method} {httpx.URL(path).path}"


def _check_run_key(run_key: str) -> None:
    """Refuse a key the engine would refuse, before anything is sent."""
    if not (
        isinstance(run_key, str) and 1 <= len(run_key) <= _MAX_RUN_KEY and all(ch in _RUN_KEY_CHARS for ch in run_key)
    ):
        raise ValueError(
            f"run_key must be 1 to {_MAX_RUN_KEY} characters of A-Z, a-z, 0-9 and . _ : / @ + = - "
            "(no space, control or format character)"
        )
