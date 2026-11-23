"""Tests for the step executor that `jamjet worker` and drive mode share.

Each test runs one claimed item through `run_claimed_item` against a real
`JamjetClient` whose transport plays the engine, so what is asserted is what
went over the wire.
"""

from __future__ import annotations

import json
import threading
import time
from typing import Any

import httpx

from jamjet.client import JamjetClient
from jamjet.worker_step import StepResult, run_claimed_item

BASE = "http://runtime"


class _Engine:
    """Plays the engine's work-item routes and records every request."""

    def __init__(
        self,
        *,
        complete_status: int = 200,
        fail_status: int = 200,
        heartbeat_answer: dict[str, Any] | None = None,
    ) -> None:
        self.complete_status = complete_status
        self.fail_status = fail_status
        self.heartbeat_answer = heartbeat_answer or {"renewed": True, "cancel_requested": False}
        self.requests: list[tuple[str, dict[str, Any]]] = []
        self.heartbeat_fences: list[int] = []
        # Set once a heartbeat arrives carrying the re-attached fence, which
        # proves the executor adopted it.
        self.saw_new_fence = threading.Event()

    def __call__(self, request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content) if request.content else {}
        path = request.url.path
        self.requests.append((path, body))
        if path.endswith("/heartbeat"):
            self.heartbeat_fences.append(body.get("lease_fence"))
            if body.get("lease_fence") == 12:
                self.saw_new_fence.set()
            return httpx.Response(200, json=self.heartbeat_answer)
        if path.endswith("/complete"):
            if self.complete_status == 409:
                return httpx.Response(409, json={"error": "lease fence does not match"})
            return httpx.Response(self.complete_status, json={"completed": True})
        if path.endswith("/fail"):
            if self.fail_status == 409:
                return httpx.Response(409, json={"error": "lease fence does not match"})
            return httpx.Response(self.fail_status, json={"failed": True})
        return httpx.Response(404, json={"error": f"unexpected {path}"})

    def paths(self, suffix: str) -> list[tuple[str, dict[str, Any]]]:
        return [(p, b) for p, b in self.requests if p.endswith(suffix)]


async def _client(engine: _Engine) -> JamjetClient:
    client = JamjetClient(base_url=BASE)
    await client._client.aclose()
    client._client = httpx.AsyncClient(base_url=BASE, transport=httpx.MockTransport(engine))
    return client


def _item(function: str, *, fence: int = 5, **extra: Any) -> dict[str, Any]:
    return {
        "id": "wi-step",
        "execution_id": "exec_step",
        "node_id": "step_node",
        "queue_type": "python_tool",
        "payload": {"module": "tests.test_worker_step", "function": function, "input": {"a": 2}},
        "lease_fence": fence,
        "lease_secs": 30,
        **extra,
    }


# ── Handlers ──────────────────────────────────────────────────────────────────


async def _double(input: dict) -> dict:
    return {"doubled": input["a"] * 2}


async def _broken(input: dict) -> dict:
    raise RuntimeError("tool broke")


# The engine object of the running test, for the handler below to wait on.
_ENGINE: _Engine | None = None


def _fail_after_reattach(input: dict) -> dict:
    """Blocks until the executor heartbeats with the re-attached fence, then raises."""
    assert _ENGINE is not None
    if not _ENGINE.saw_new_fence.wait(timeout=5.0):
        raise AssertionError("no heartbeat carried the re-attached fence")
    raise RuntimeError("tool broke after the re-attach")


def _until_cancelled(input: dict) -> dict:
    from jamjet.worker_context import current_work

    ctx = current_work()
    deadline = time.monotonic() + 3.0
    while not ctx.cancel_requested.is_set() and time.monotonic() < deadline:
        time.sleep(0.02)
    return {"saw_cancel": ctx.cancel_requested.is_set()}


def _report_keys(input: dict) -> dict:
    from jamjet.worker_context import current_work

    ctx = current_work()
    return {"seen_idempotency_key": ctx.idempotency_key, "seen_run_key": ctx.run_key}


# ── Tests ─────────────────────────────────────────────────────────────────────


async def test_a_409_on_complete_is_lease_lost_not_a_failure() -> None:
    engine = _Engine(complete_status=409)
    client = await _client(engine)
    try:
        result = await run_claimed_item(client, "w-1", _item("_double"))
    finally:
        await client._client.aclose()

    assert result is StepResult.LEASE_LOST
    assert len(engine.paths("/complete")) == 1
    assert engine.paths("/fail") == [], "a lost lease must never be reported as a failure"


async def test_a_409_on_fail_is_lease_lost_and_sent_once() -> None:
    engine = _Engine(fail_status=409)
    client = await _client(engine)
    try:
        result = await run_claimed_item(client, "w-1", _item("_broken"))
    finally:
        await client._client.aclose()

    assert result is StepResult.LEASE_LOST
    assert len(engine.paths("/fail")) == 1, "a refused failure report is not sent again"
    assert engine.paths("/complete") == []


async def test_a_reattached_fence_is_used_and_a_handler_error_is_reported_with_it() -> None:
    global _ENGINE
    engine = _Engine(heartbeat_answer={"renewed": True, "cancel_requested": False, "lease_fence": 12})
    _ENGINE = engine
    client = await _client(engine)
    try:
        result = await run_claimed_item(
            client, "w-1", _item("_fail_after_reattach", fence=11), heartbeat_interval_s=0.05
        )
    finally:
        _ENGINE = None
        await client._client.aclose()

    assert result is StepResult.FAILED_REPORTED
    assert engine.heartbeat_fences[0] == 11, "the first heartbeat carries the claim's fence"
    assert 12 in engine.heartbeat_fences, "later heartbeats carry the re-attached fence"
    fails = engine.paths("/fail")
    assert len(fails) == 1
    assert fails[0][1]["lease_fence"] == 12, "the failure must be reported under the re-attached fence"
    assert fails[0][1]["error"] == "tool broke after the re-attach"
    assert engine.paths("/complete") == []


async def test_cancel_abandons_without_reporting() -> None:
    engine = _Engine(heartbeat_answer={"renewed": True, "cancel_requested": True})
    client = await _client(engine)
    started = time.monotonic()
    try:
        result = await run_claimed_item(client, "w-1", _item("_until_cancelled"), heartbeat_interval_s=0.05)
    finally:
        await client._client.aclose()
    elapsed = time.monotonic() - started

    assert result is StepResult.CANCELLED
    assert elapsed < 2.0, f"the executor must stop waiting once cancelled, took {elapsed:.2f}s"
    assert len(engine.paths("/heartbeat")) >= 1
    assert engine.paths("/complete") == []
    assert engine.paths("/fail") == []


async def test_the_handler_sees_the_idempotency_key_and_run_key() -> None:
    engine = _Engine()
    client = await _client(engine)
    try:
        result = await run_claimed_item(
            client, "w-1", _item("_report_keys", idempotency_key="idem-7"), run_key="order-42"
        )
    finally:
        await client._client.aclose()

    assert result is StepResult.COMPLETED
    completes = engine.paths("/complete")
    assert len(completes) == 1
    body = completes[0][1]
    assert body["output"] == {"seen_idempotency_key": "idem-7", "seen_run_key": "order-42"}
    assert body["idempotency_key"] == "idem-7"
    assert body["lease_fence"] == 5
