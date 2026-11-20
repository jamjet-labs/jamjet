"""Retries: what is sent again, and what never is.

A request that never left is sent again for every call, and so is a 429. A
call that is safe to repeat is also sent again after a lost answer or a 5xx.
Any other call whose answer was lost raises ``OutcomeUnknown`` and is never
sent again. No other 4xx is retried.
"""

from __future__ import annotations

import dataclasses
import logging
import math
import random
from typing import Any

import httpx
import pytest
import respx

from jamjet import transport
from jamjet.client import JamjetAPIError, JamjetClient
from jamjet.transport import OutcomeUnknown, PlatformAuthError, RetryPolicy
from tests.test_client_auth import BASE, CALLS, TOKEN, Call, _answer

# 2026-11-20 10:00:00 UTC
NOW = 1795168800.0

# Whether each client call may be sent again after it may have taken effect.
# complete_work_item and fail_work_item are safe here because the table's
# calls carry a lease fence; without one they are unsafe (see UNFENCED).
SAFE = {
    "health": True,
    "create_workflow": True,
    "create_cron_job": False,
    "list_cron_jobs": True,
    "start_execution": False,
    "get_execution": True,
    "list_executions": True,
    "cancel_execution": False,
    "get_events": True,
    "approve": False,
    "list_approvals": True,
    "send_external_event": False,
    "put_artifact": True,
    "get_artifact": True,
    "register_agent": False,
    "list_agents": True,
    "get_agent": True,
    "activate_agent": False,
    "deactivate_agent": False,
    "discover_agent": False,
    "claim_work_item": False,
    "complete_work_item": True,
    "fail_work_item": True,
    "heartbeat_work_item": True,
    "post": False,
    "get": True,
}

# The name OutcomeUnknown gives, where it is not the method's own.
OPERATION = {"post": "POST /custom/route", "get": "GET /custom/route"}

UNFENCED = [
    Call(
        "complete_work_item",
        lambda c: c.complete_work_item("wi-1", "exec_1", "n1", {"r": 1}, {}),
        "POST",
        "/work-items/wi-1/complete",
    ),
    Call("fail_work_item", lambda c: c.fail_work_item("wi-1", "boom"), "POST", "/work-items/wi-1/fail"),
]


@pytest.fixture(autouse=True)
def _no_env_token(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("JAMJET_TOKEN", raising=False)


@pytest.fixture(autouse=True)
def waits(monkeypatch: pytest.MonkeyPatch) -> list[float]:
    """Every wait the send loop asks for, in order. Nothing really sleeps."""
    recorded: list[float] = []

    async def fake_sleep(seconds: float) -> None:
        recorded.append(seconds)

    monkeypatch.setattr(transport, "_sleep", fake_sleep)
    return recorded


def _client(**kwargs: Any) -> JamjetClient:
    return JamjetClient(BASE, api_token=TOKEN, **kwargs)


# ── a request that never left ────────────────────────────────────────────────


@pytest.mark.parametrize("call", CALLS, ids=[c.name for c in CALLS])
async def test_a_connect_error_is_retried_for_every_call(call: Call, waits: list[float]) -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.route().mock(side_effect=[httpx.ConnectError, _answer])
        async with _client() as client:
            await call.call(client)
    assert route.call_count == 2
    assert len(waits) == 1
    first, second = (c.request for c in route.calls)
    assert (second.method, second.url.path) == (call.method, call.path)
    assert (second.method, second.url, second.content) == (first.method, first.url, first.content)
    assert second.headers["authorization"] == first.headers["authorization"] == f"Bearer {TOKEN}"


@pytest.mark.parametrize("error", [httpx.ConnectError, httpx.ConnectTimeout, httpx.PoolTimeout])
async def test_an_unsent_request_is_sent_again_even_for_an_unsafe_call(
    error: type[httpx.RequestError], waits: list[float]
) -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/executions/exec_1/approve")
        route.side_effect = [error, error, httpx.Response(200, json={"status": "approved"})]
        async with _client() as client:
            assert await client.approve("exec_1", "approved", action_digest="d1") == {"status": "approved"}
    assert route.call_count == 3
    assert len(waits) == 2


# ── a lost answer ────────────────────────────────────────────────────────────


async def test_a_read_timeout_on_a_plain_start_is_outcome_unknown() -> None:
    with respx.mock(assert_all_called=False) as mock:
        start = mock.post(f"{BASE}/executions")
        start.side_effect = [httpx.ReadTimeout, httpx.Response(201, json={"execution_id": "exec_2"})]
        beat = mock.post(f"{BASE}/work-items/wi-1/heartbeat")
        beat.side_effect = [httpx.ReadTimeout, httpx.Response(200, json={"cancel_requested": False})]
        async with _client() as client:
            with pytest.raises(OutcomeUnknown) as exc:
                await client.start_execution("w", {"q": "hi"})
            assert await client.heartbeat_work_item("wi-1", "wk-1", lease_fence=3) == {"cancel_requested": False}
    assert start.call_count == 1
    assert beat.call_count == 2
    error = exc.value
    assert error.operation == "start_execution"
    assert isinstance(error.__cause__, httpx.ReadTimeout)
    # A caller's `except httpx.RequestError: retry` must not catch it and send it again.
    assert not isinstance(error, httpx.HTTPError)
    # start_execution takes no run_key yet, so the message must not point at one.
    assert str(error) == (
        "start_execution may have taken effect, but its answer was lost (ReadTimeout). It was not sent again, "
        "because a second send could do it twice. Check whether it took effect before you repeat it."
    )


async def test_an_engine_503_is_retried_for_a_fenced_complete_and_not_for_approve(waits: list[float]) -> None:
    def draining() -> httpx.Response:
        return httpx.Response(503, headers={"Retry-After": "1"}, json={"error": "the engine is shutting down"})

    with respx.mock(assert_all_called=False) as mock:
        fenced = mock.post(f"{BASE}/work-items/wi-1/complete")
        fenced.side_effect = [draining(), httpx.Response(200, json={})]
        unfenced = mock.post(f"{BASE}/work-items/wi-2/complete").mock(return_value=draining())
        approve = mock.post(f"{BASE}/executions/exec_1/approve").mock(return_value=draining())
        async with _client() as client:
            await client.complete_work_item("wi-1", "exec_1", "n1", {"r": 1}, {}, lease_fence=3)
            with pytest.raises(OutcomeUnknown) as approved:
                await client.approve("exec_1", "approved", action_digest="d1")
            with pytest.raises(OutcomeUnknown) as completed:
                await client.complete_work_item("wi-2", "exec_1", "n1", {"r": 1}, {})
    assert fenced.call_count == 2
    assert waits == [1.0]
    assert approve.call_count == 1
    assert unfenced.call_count == 1
    assert approved.value.operation == "approve"
    assert completed.value.operation == "complete_work_item"
    cause = approved.value.__cause__
    assert isinstance(cause, JamjetAPIError)
    assert cause.response.status_code == 503
    assert cause.detail == "the engine is shutting down"
    assert str(approved.value) == (
        "approve may have taken effect, but the answer was 503 Service Unavailable, which does not say whether "
        "it did. It was not sent again, because a second send could do it twice. Check whether it took effect "
        "before you repeat it."
    )


@pytest.mark.parametrize(
    "failure",
    [
        httpx.ReadTimeout,
        httpx.WriteTimeout,
        httpx.ReadError,
        httpx.WriteError,
        httpx.RemoteProtocolError,
        httpx.Response(500, json={"error": "database is locked"}),
        httpx.Response(502, content=b"<html>Bad Gateway</html>"),
        httpx.Response(503, headers={"Retry-After": "1"}, json={"error": "draining"}),
        httpx.Response(504),
    ],
    ids=["ReadTimeout", "WriteTimeout", "ReadError", "WriteError", "RemoteProtocolError", "500", "502", "503", "504"],
)
async def test_every_lost_answer_on_an_unsafe_call_is_outcome_unknown(failure: Any, waits: list[float]) -> None:
    with respx.mock(assert_all_called=False) as mock:
        start = mock.post(f"{BASE}/executions")
        start.side_effect = [failure, httpx.Response(201, json={"execution_id": "exec_2"})]
        async with _client() as client:
            with pytest.raises(OutcomeUnknown) as exc:
                await client.start_execution("w", {"q": "hi"})
    assert start.call_count == 1
    assert waits == []
    assert exc.value.operation == "start_execution"


def test_the_safety_table_covers_every_call() -> None:
    assert set(SAFE) == {c.name for c in CALLS}


@pytest.mark.parametrize(
    ("call", "safe"),
    [(c, SAFE[c.name]) for c in CALLS] + [(c, False) for c in UNFENCED],
    ids=[c.name for c in CALLS] + [f"{c.name}-unfenced" for c in UNFENCED],
)
async def test_each_call_is_sent_again_after_a_lost_answer_only_when_safe(call: Call, safe: bool) -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.route().mock(side_effect=httpx.ReadTimeout)
        async with _client() as client:
            if safe:
                # After the last send, the last failure itself.
                with pytest.raises(httpx.ReadTimeout):
                    await call.call(client)
            else:
                with pytest.raises(OutcomeUnknown) as exc:
                    await call.call(client)
                assert exc.value.operation == OPERATION.get(call.name, call.name)
    assert route.call_count == (3 if safe else 1)
    assert route.calls.last.request.url.path == call.path


# ── Retry-After ──────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    ("status", "retry_after", "expected_waits"),
    [
        (429, "2", [2.0]),
        (429, "30", [30.0]),
        (429, "Fri, 20 Nov 2026 10:00:07 GMT", [7.0]),
        (429, "Fri, 20 Nov 2026 09:59:00 GMT", [0.0]),
        (429, None, [0.1]),
        (429, "soon", [0.1]),
        (503, "4", [4.0]),
        (503, "Fri, 20 Nov 2026 10:00:12 GMT", [12.0]),
        (429, "120", None),
        (429, "Fri, 20 Nov 2026 10:02:00 GMT", None),
        (429, "31", None),
        (503, "120", None),
    ],
    ids=[
        "seconds",
        "30-seconds",
        "date",
        "date-in-the-past",
        "no-header-backs-off",
        "unreadable-backs-off",
        "503-seconds",
        "503-date",
        "120-seconds-raised",
        "date-120-seconds-ahead-raised",
        "31-seconds-raised",
        "503-120-seconds-raised",
    ],
)
async def test_retry_after_is_honoured_and_a_long_one_is_raised(
    status: int,
    retry_after: str | None,
    expected_waits: list[float] | None,
    waits: list[float],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(transport, "_wall_clock", lambda: NOW)
    monkeypatch.setattr(transport, "_random", lambda: 0.5)
    headers = {} if retry_after is None else {"Retry-After": retry_after}
    busy = httpx.Response(status, headers=headers, json={"error": "busy"})

    # A 429 was not processed, so even an unsafe start is sent again. A 5xx is
    # sent again only for a safe call.
    async def call(client: JamjetClient) -> dict[str, Any]:
        if status == 429:
            return await client.start_execution("w", {"q": "hi"})
        return await client.get_execution("exec_1")

    with respx.mock(assert_all_called=False) as mock:
        if status == 429:
            route = mock.post(f"{BASE}/executions")
            answer = {"execution_id": "exec_2"}
        else:
            route = mock.get(f"{BASE}/executions/exec_1")
            answer = {"status": "running"}
        route.side_effect = [busy, httpx.Response(200, json=answer)]
        async with _client() as client:
            if expected_waits is None:
                with pytest.raises(JamjetAPIError) as exc:
                    await call(client)
                assert exc.value.response.status_code == status
            else:
                assert await call(client) == answer
    if expected_waits is None:
        assert route.call_count == 1
        assert waits == []
    else:
        assert route.call_count == 2
        assert waits == pytest.approx(expected_waits)


async def test_a_lower_retry_after_limit_raises_sooner(waits: list[float]) -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").mock(
            return_value=httpx.Response(429, headers={"Retry-After": "6"}, json={"error": "busy"})
        )
        async with _client(retry=RetryPolicy(max_retry_after_s=5.0)) as client:
            with pytest.raises(JamjetAPIError):
                await client.health()
    assert route.call_count == 1
    assert waits == []


# ── never retried ────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    ("status", "headers", "body", "expected"),
    [
        (400, {"Content-Type": "application/json"}, b'{"error": "bad input"}', JamjetAPIError),
        (404, {"Content-Type": "application/json"}, b'{"error": "not found"}', JamjetAPIError),
        (409, {"Content-Type": "application/json"}, b'{"error": "lease lost"}', JamjetAPIError),
        (422, {"Content-Type": "application/json"}, b'{"error": "unprocessable"}', JamjetAPIError),
        (401, {"Content-Type": "application/json"}, b'{"error": "invalid or expired token"}', JamjetAPIError),
        (403, {"Content-Type": "application/json"}, b'{"error": "reviewer tokens cannot claim"}', JamjetAPIError),
        (401, {"Content-Type": "text/html"}, b"<html>Unauthorized</html>", PlatformAuthError),
        (403, {"Content-Type": "text/html"}, b"<html>Forbidden</html>", PlatformAuthError),
    ],
    ids=["400", "404", "409", "422", "engine-401", "engine-403", "platform-401", "platform-403"],
)
async def test_4xx_is_never_retried(
    status: int, headers: dict[str, str], body: bytes, expected: type[Exception], waits: list[float]
) -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/work-items/wi-1/heartbeat")
        route.side_effect = [httpx.Response(status, headers=headers, content=body), httpx.Response(200, json={})]
        async with _client() as client:
            with pytest.raises(expected) as exc:
                await client.heartbeat_work_item("wi-1", "wk-1", lease_fence=3)
    assert type(exc.value) is expected
    assert exc.value.response.status_code == status
    assert route.call_count == 1
    assert waits == []


async def test_a_url_the_client_cannot_send_to_is_not_retried(waits: list[float]) -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.route().mock(side_effect=httpx.UnsupportedProtocol)
        async with _client() as client:
            with pytest.raises(httpx.UnsupportedProtocol):
                await client.health()
    assert route.call_count == 1
    assert waits == []


# ── how many sends, and how long between them ────────────────────────────────


async def test_attempts_are_bounded_and_backoff_has_jitter(waits: list[float], monkeypatch: pytest.MonkeyPatch) -> None:
    assert dataclasses.astuple(RetryPolicy()) == (3, 0.2, 5.0, 30.0)
    assert RetryPolicy.off() == RetryPolicy(attempts=1)

    monkeypatch.setattr(transport, "_random", random.Random(1).random)
    with respx.mock(assert_all_called=False) as mock:
        health = mock.get(f"{BASE}/health").mock(return_value=httpx.Response(503, json={"error": "starting"}))
        async with _client() as client:
            with pytest.raises(JamjetAPIError) as exc:
                await client.health()
    assert exc.value.response.status_code == 503
    assert health.call_count == 3
    # Full jitter: a uniform draw up to 0.2 s, then up to 0.4 s. Seed 1 draws
    # 0.13436424411240122 and 0.8474337369372327.
    assert waits == pytest.approx([0.026872848822480245, 0.3389734947748931])

    waits.clear()
    with respx.mock(assert_all_called=False) as mock:
        down = mock.get(f"{BASE}/health").mock(side_effect=httpx.ConnectError)
        async with _client(retry=RetryPolicy.off()) as client:
            with pytest.raises(httpx.ConnectError):
                await client.health()
    assert down.call_count == 1
    assert waits == []

    # The ceiling doubles from base_delay_s and stops at max_delay_s.
    monkeypatch.setattr(transport, "_random", lambda: 0.5)
    with respx.mock(assert_all_called=False) as mock:
        down = mock.get(f"{BASE}/health").mock(side_effect=httpx.ConnectError)
        async with _client(retry=RetryPolicy(attempts=6, base_delay_s=0.2, max_delay_s=1.0)) as client:
            with pytest.raises(httpx.ConnectError):
                await client.health()
    assert down.call_count == 6
    assert waits == pytest.approx([0.1, 0.2, 0.4, 0.5, 0.5])


@pytest.mark.parametrize(
    "kwargs",
    [
        {"attempts": 0},
        {"attempts": True},
        {"attempts": 2.5},
        {"base_delay_s": -0.1},
        {"max_delay_s": math.nan},
        {"max_retry_after_s": math.inf},
    ],
    ids=["no-attempts", "bool", "fraction", "negative", "nan", "inf"],
)
def test_a_policy_out_of_range_is_refused(kwargs: dict[str, Any]) -> None:
    with pytest.raises(ValueError):
        RetryPolicy(**kwargs)


def test_retry_must_be_a_retry_policy() -> None:
    with pytest.raises(TypeError, match="RetryPolicy"):
        JamjetClient(BASE, retry=3)


# ── no credential text ───────────────────────────────────────────────────────

TOKEN_SENTINEL = "jj-sentinel-retry-61c0a9"
ID_SENTINEL = "id-sentinel-retry-2be74d"


class _HeaderAuth(httpx.Auth):
    def auth_flow(self, request: httpx.Request) -> Any:
        request.headers["X-Serverless-Authorization"] = f"Bearer {ID_SENTINEL}"
        yield request


async def test_retries_and_outcome_unknown_carry_no_credential(caplog: pytest.LogCaptureFixture) -> None:
    for name in (None, "httpx", "jamjet"):
        caplog.set_level(logging.DEBUG, logger=name)
    with respx.mock(assert_all_called=False) as mock:
        mock.get(f"{BASE}/health").mock(side_effect=[httpx.ConnectError, httpx.Response(200, json={"status": "ok"})])
        mock.post(f"{BASE}/executions").mock(side_effect=httpx.ReadTimeout)
        async with JamjetClient(BASE, api_token=TOKEN_SENTINEL, auth=_HeaderAuth()) as client:
            await client.health()
            with pytest.raises(OutcomeUnknown) as exc:
                await client.start_execution("w", {"q": "hi"})
    # The test can fail: the retry was logged.
    assert "health failed (ConnectError)" in caplog.text
    for text in (caplog.text, str(exc.value), repr(exc.value), str(exc.value.__cause__)):
        assert TOKEN_SENTINEL not in text
        assert ID_SENTINEL not in text
