"""An identity token from a token source, on every request to one origin.

``TokenSourceAuth`` keeps a token for its own lifetime (``exp - iat``), counted
on the monotonic clock from the fetch, and fetches a fresh one a margin before
that ends, one fetch at a time. A 401 from the platform in front of the engine
gets one fresh token and one resend. A request is never sent without its token.
"""

from __future__ import annotations

import asyncio
import base64
import json
import logging
import threading
import time
from collections.abc import AsyncIterator, Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import httpx
import pytest
import respx

from jamjet.auth import IdentityTokenError, TokenSourceAuth, token_source
from jamjet.client import JamjetAPIError, JamjetClient
from jamjet.transport import CredentialScopeError, PlatformAuthError

BASE = "https://engine.test"
JJ = "jj-test-token-71c2"
WALL = 1795168800  # 2026-11-20 10:00:00 UTC
JWT_HEADER = "eyJhbGciOiJSUzI1NiJ9"  # {"alg":"RS256"}


def token(claims: dict[str, Any], sig: str = "c2ln") -> str:
    """A token carrying these claims. Nothing here checks its signature."""
    payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).rstrip(b"=").decode()
    return f"{JWT_HEADER}.{payload}.{sig}"


def jwt(iat: float, exp: float, sig: str = "c2ln") -> str:
    return token({"iat": iat, "exp": exp}, sig)


FIRST = jwt(WALL, WALL + 3600, sig="Zmlyc3Q")
SECOND = jwt(WALL + 60, WALL + 3660, sig="c2Vjb25k")


def platform_401(body: bytes = b"<html><body>401 Unauthorized</body></html>") -> httpx.Response:
    return httpx.Response(401, headers={"Content-Type": "text/html"}, content=body)


def ok() -> httpx.Response:
    return httpx.Response(200, json={"status": "ok"})


class Source:
    """A token source that counts its calls. Each call gives the next item
    (the last one repeats); an exception is raised instead of returned."""

    def __init__(self, *items: Any) -> None:
        self.items = list(items) or [FIRST]
        self.calls = 0

    def next(self) -> Any:
        item = self.items[min(self.calls, len(self.items) - 1)]
        self.calls += 1
        if isinstance(item, BaseException):
            raise item
        return item

    def get(self) -> Any:
        return self.next()

    async def aget(self) -> Any:
        return self.next()


class Clock:
    def __init__(self) -> None:
        self.now = 1000.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


@pytest.fixture(autouse=True)
def _no_env_token(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("JAMJET_TOKEN", raising=False)


@pytest.fixture(autouse=True)
def clock(monkeypatch: pytest.MonkeyPatch) -> Clock:
    fake = Clock()
    monkeypatch.setattr(token_source, "_monotonic", fake)
    return fake


@pytest.fixture(autouse=True)
def waits(monkeypatch: pytest.MonkeyPatch) -> list[float]:
    """The waits between fetch tries, recorded instead of slept."""
    recorded: list[float] = []

    async def sleep(seconds: float) -> None:
        recorded.append(seconds)
        await asyncio.sleep(0)

    def sleep_sync(seconds: float) -> None:
        recorded.append(seconds)

    monkeypatch.setattr(token_source, "_sleep", sleep)
    monkeypatch.setattr(token_source, "_sleep_sync", sleep_sync)
    return recorded


def client(auth: httpx.Auth, base: str = BASE) -> JamjetClient:
    return JamjetClient(base, api_token=JJ, auth=auth)


async def spin(until: Any = None, seconds: float = 2.0) -> None:
    """Let other tasks run: until ``until()`` is true (polling, for work on
    another thread), then 50 more turns of the loop."""
    if until is not None:
        deadline = time.monotonic() + seconds
        while not until() and time.monotonic() < deadline:
            await asyncio.sleep(0.001)
    for _ in range(50):
        await asyncio.sleep(0)


# ── caching ──────────────────────────────────────────────────────────────────


@pytest.mark.parametrize("kind", ["async", "sync"])
async def test_the_token_is_fetched_once_and_reused(kind: str) -> None:
    # Ten calls in a row.
    source = Source()
    fetch = source.aget if kind == "async" else source.get
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").mock(side_effect=lambda _: ok())
        async with client(TokenSourceAuth(fetch, origin=BASE)) as c:
            for _ in range(10):
                await c.health()
    assert source.calls == 1
    assert route.call_count == 10
    assert {call.request.headers["x-serverless-authorization"] for call in route.calls} == {f"Bearer {FIRST}"}

    # Twenty at once, while the one fetch is still running.
    calls = 0
    if kind == "async":
        gate = asyncio.Event()

        async def gated() -> str:
            nonlocal calls
            calls += 1
            await gate.wait()
            return FIRST

        release = gate.set
        auth = TokenSourceAuth(gated, origin=BASE)
    else:
        thread_gate = threading.Event()

        def gated_sync() -> str:
            nonlocal calls
            calls += 1
            thread_gate.wait(5)
            return FIRST

        release = thread_gate.set
        auth = TokenSourceAuth(gated_sync, origin=BASE)
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").mock(side_effect=lambda _: ok())
        async with client(auth) as c:
            tasks = [asyncio.create_task(c.health()) for _ in range(20)]
            await spin(until=lambda: calls > 0)
            # The race opened: all twenty wait while the one fetch runs.
            assert calls == 1
            assert sum(not t.done() for t in tasks) == 20
            assert route.call_count == 0
            release()
            await asyncio.gather(*tasks)
    assert calls == 1
    assert route.call_count == 20


@pytest.mark.parametrize(
    ("lifetime", "margin", "fetch_takes", "refresh_after"),
    [
        (3600, None, 0, 3300),
        (3600, 60, 0, 3540),
        (3600, 0, 0, 3600),
        (400, None, 0, 200),
        (3600, None, 5, 3300),
    ],
    ids=["default-margin", "margin-60", "no-margin", "margin-capped-at-half", "counted-from-the-fetch-start"],
)
async def test_refresh_comes_margin_before_expiry_by_monotonic_time(
    clock: Clock, lifetime: int, margin: float | None, fetch_takes: float, refresh_after: float
) -> None:
    calls = 0

    async def fetch() -> str:
        nonlocal calls
        calls += 1
        clock.advance(fetch_takes)
        return jwt(WALL, WALL + lifetime)

    kwargs: dict[str, Any] = {} if margin is None else {"refresh_margin_s": margin}
    start = clock.now
    with respx.mock(assert_all_called=False) as mock:
        mock.get(f"{BASE}/health").mock(side_effect=lambda _: ok())
        async with client(TokenSourceAuth(fetch, origin=BASE, **kwargs)) as c:
            await c.health()
            clock.now = start + refresh_after - 0.001
            await c.health()
            assert calls == 1
            clock.now = start + refresh_after
            await c.health()
            assert calls == 2
            await c.health()
            assert calls == 2


@pytest.mark.parametrize("shift", [-3600, 0, 3600], ids=["behind", "right", "ahead"])
async def test_a_wall_clock_an_hour_ahead_or_behind_changes_nothing(
    clock: Clock, monkeypatch: pytest.MonkeyPatch, shift: int
) -> None:
    start = clock.now
    # The issuer's clock is right. This machine's wall clock is off by `shift`
    # and runs at the same pace as the monotonic clock.
    monkeypatch.setattr(time, "time", lambda: WALL + shift + (clock.now - start))
    calls = 0

    async def fetch() -> str:
        nonlocal calls
        calls += 1
        issued = WALL + (clock.now - start)
        return jwt(issued, issued + 3600)

    seen = []
    with respx.mock(assert_all_called=False) as mock:
        mock.get(f"{BASE}/health").mock(side_effect=lambda _: ok())
        async with client(TokenSourceAuth(fetch, origin=BASE)) as c:
            for _ in range(5):
                await c.health()
            seen.append(calls)
            clock.advance(3299)
            await c.health()
            seen.append(calls)
            clock.advance(1)
            await c.health()
            seen.append(calls)
            for _ in range(5):
                await c.health()
            seen.append(calls)
    assert seen == [1, 1, 2, 2]


@pytest.mark.parametrize(
    "claims",
    [{}, {"exp": WALL + 3600}, {"iat": WALL}, {"iat": "today", "exp": WALL + 3600}, {"iat": True, "exp": WALL + 3600}],
    ids=["none", "exp-only", "iat-only", "iat-not-a-number", "iat-a-bool"],
)
async def test_a_token_without_exp_and_iat_is_fetched_per_request_with_one_warning(
    claims: dict[str, Any], caplog: pytest.LogCaptureFixture
) -> None:
    caplog.set_level(logging.DEBUG, logger="jamjet")
    value = token(claims, sig="bm9leHA")
    source = Source(value)
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").mock(side_effect=lambda _: ok())
        async with client(TokenSourceAuth(source.aget, origin=BASE)) as c:
            for _ in range(3):
                await c.health()
    assert source.calls == 3
    assert route.call_count == 3
    warnings = [r for r in caplog.records if r.levelno == logging.WARNING and r.name.startswith("jamjet.auth")]
    assert len(warnings) == 1
    assert "not cached" in warnings[0].getMessage()
    assert value not in caplog.text
    assert "bm9leHA" not in caplog.text


# ── a platform 401 ───────────────────────────────────────────────────────────


async def test_a_token_expiring_mid_run_is_refreshed_once_on_a_platform_401() -> None:
    source = Source(FIRST, SECOND)
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health")
        route.side_effect = [platform_401(), ok(), ok()]
        async with client(TokenSourceAuth(source.aget, origin=BASE)) as c:
            assert await c.health() == {"status": "ok"}
            assert source.calls == 2
            assert route.call_count == 2
            # The fresh token is kept.
            await c.health()
    assert source.calls == 2
    sent = [call.request.headers for call in route.calls]
    assert [h["x-serverless-authorization"] for h in sent] == [f"Bearer {t}" for t in (FIRST, SECOND, SECOND)]
    assert [h["authorization"] for h in sent] == [f"Bearer {JJ}"] * 3


async def test_a_second_platform_401_is_platform_auth_error() -> None:
    source = Source(FIRST, SECOND)
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").mock(side_effect=lambda _: platform_401())
        async with client(TokenSourceAuth(source.aget, origin=BASE)) as c:
            with pytest.raises(PlatformAuthError) as exc:
                await c.health()
    assert source.calls == 2
    assert route.call_count == 2
    assert exc.value.response.status_code == 401
    assert "platform refused a fresh identity token: check the audience and the caller's invoker permission" in str(
        exc.value
    )


@pytest.mark.parametrize(
    ("status", "content_type", "body", "error"),
    [
        (401, "application/json", b'{"error": "invalid or expired token"}', JamjetAPIError),
        (403, "application/json", b'{"error": "operator role required"}', JamjetAPIError),
        (403, "text/html", b"<html><body>403 Forbidden</body></html>", PlatformAuthError),
    ],
    ids=["engine-401", "engine-403", "platform-403"],
)
async def test_an_engine_401_does_not_refresh(status: int, content_type: str, body: bytes, error: type) -> None:
    source = Source(FIRST, SECOND)
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").respond(status, headers={"Content-Type": content_type}, content=body)
        async with client(TokenSourceAuth(source.aget, origin=BASE)) as c:
            with pytest.raises(error) as exc:
                await c.health()
    if error is JamjetAPIError:
        assert not isinstance(exc.value, PlatformAuthError)
    assert source.calls == 1
    assert route.call_count == 1


async def test_many_requests_refused_at_once_refresh_once() -> None:
    gate = asyncio.Event()
    calls = 0

    async def fetch() -> str:
        nonlocal calls
        calls += 1
        if calls == 1:
            return FIRST
        await gate.wait()
        return SECOND

    def answer(request: httpx.Request) -> httpx.Response:
        refused = request.headers["x-serverless-authorization"] == f"Bearer {FIRST}"
        return platform_401() if refused else ok()

    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").mock(side_effect=answer)
        async with client(TokenSourceAuth(fetch, origin=BASE)) as c:
            tasks = [asyncio.create_task(c.health()) for _ in range(5)]
            await spin()
            # The race opened: all five were refused before the one refresh ended.
            assert route.call_count == 5
            assert calls == 2
            gate.set()
            await asyncio.gather(*tasks)
    assert calls == 2
    assert route.call_count == 10


@pytest.mark.parametrize("through", ["jamjet-client", "httpx-client"])
async def test_the_resend_after_a_platform_401_carries_the_whole_body(through: str) -> None:
    async def body() -> AsyncIterator[bytes]:
        yield b"hello "
        yield b"world"

    source = Source(FIRST, SECOND)
    auth = TokenSourceAuth(source.aget, origin=BASE)
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/custom")
        route.side_effect = [platform_401(), httpx.Response(200, json={"ok": True})]
        if through == "jamjet-client":
            async with client(auth) as c:
                assert await c.post("/custom", content=body()) == {"ok": True}
        else:
            async with httpx.AsyncClient(auth=auth) as plain:
                assert (await plain.post(f"{BASE}/custom", content=body())).status_code == 200
    assert [call.request.content for call in route.calls] == [b"hello world", b"hello world"]


# ── a source that fails, or gives something that is not a token ─────────────


@pytest.mark.parametrize(
    ("error", "tries", "expected_waits"),
    [
        (ConnectionRefusedError("refused"), 3, [0.2, 0.5]),
        (TimeoutError("slow"), 3, [0.2, 0.5]),
        (httpx.ConnectError("no route to the token source"), 3, [0.2, 0.5]),
        (RuntimeError("no credentials on this machine"), 1, []),
    ],
    ids=["os-error", "timeout", "httpx-transport-error", "other-error"],
)
async def test_a_failing_source_sends_nothing(
    waits: list[float], error: Exception, tries: int, expected_waits: list[float]
) -> None:
    source = Source(error)
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        async with client(TokenSourceAuth(source.aget, origin=BASE)) as c:
            with pytest.raises(IdentityTokenError) as exc:
                await c.health()
    assert not anywhere.called
    assert source.calls == tries
    assert waits == expected_waits
    assert exc.value.__cause__ is error
    assert "Nothing was sent" in str(exc.value)


async def test_the_fetch_is_tried_twice_more_after_a_transport_error(waits: list[float]) -> None:
    source = Source(ConnectionResetError("reset"), httpx.ReadTimeout("slow"), FIRST)
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").mock(side_effect=lambda _: ok())
        async with client(TokenSourceAuth(source.aget, origin=BASE)) as c:
            await c.health()
    assert source.calls == 3
    assert waits == [0.2, 0.5]
    assert route.call_count == 1
    assert route.calls.last.request.headers["x-serverless-authorization"] == f"Bearer {FIRST}"


async def test_a_failed_refresh_after_a_platform_401_sends_nothing_more(waits: list[float]) -> None:
    source = Source(FIRST, RuntimeError("the source is gone"))
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").mock(side_effect=lambda _: platform_401())
        async with client(TokenSourceAuth(source.aget, origin=BASE)) as c:
            with pytest.raises(IdentityTokenError) as exc:
                await c.health()
    assert route.call_count == 1
    assert source.calls == 2
    assert "not sent again" in str(exc.value)


async def test_requests_waiting_on_a_failed_fetch_share_its_error(waits: list[float]) -> None:
    tasks: list[asyncio.Task[Any]] = []
    pending_at_last_try: list[int] = []
    calls = 0

    async def fetch() -> str:
        nonlocal calls
        calls += 1
        if calls == 3:
            pending_at_last_try.append(sum(not t.done() for t in tasks))
        raise ConnectionRefusedError("refused")

    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        async with client(TokenSourceAuth(fetch, origin=BASE)) as c:
            tasks.extend(asyncio.create_task(c.health()) for _ in range(5))
            results = await asyncio.gather(*tasks, return_exceptions=True)
    # The race opened: all five were waiting on the one round of tries.
    assert pending_at_last_try == [5]
    assert calls == 3
    assert [type(r) for r in results] == [IdentityTokenError] * 5
    assert not anywhere.called


MALFORMED = {
    "one-part": "eyJhbGciOiJSUzI1NiJ9",
    "not-base64url": "eyJhbGciOiJSUzI1NiJ9.eyJpYXQiOjE3OTUxNjg4MDB9.c2ln+/=",
    "payload-not-json": "eyJhbGciOiJSUzI1NiJ9.bm90IGpzb24.c2ln",
    "exp-not-after-iat": "eyJhbGciOiJSUzI1NiJ9.eyJpYXQiOjE3OTUxNjg4MDAsImV4cCI6MTc5NTE2ODgwMH0.c2ln",
}


@pytest.mark.parametrize("bad", list(MALFORMED.values()), ids=list(MALFORMED))
async def test_a_malformed_token_is_refused(waits: list[float], bad: str) -> None:
    source = Source(bad)
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        async with client(TokenSourceAuth(source.aget, origin=BASE)) as c:
            with pytest.raises(IdentityTokenError) as exc:
                await c.health()
    assert not anywhere.called
    assert source.calls == 1
    assert waits == []
    assert bad not in str(exc.value)
    assert exc.value.__cause__ is None


@pytest.mark.parametrize(
    "value",
    [None, FIRST.encode(), "", 42, "eyJhbGciOiJSUzI1NiJ9.WzE3OTUxNjg4MDBd.c2ln", f"{FIRST}\r\nX-Injected: 1"],
    ids=["none", "bytes", "empty", "a-number", "claims-not-an-object", "a-header-break"],
)
async def test_a_source_that_gives_no_token_is_refused(value: Any) -> None:
    source = Source(value)
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        async with client(TokenSourceAuth(source.aget, origin=BASE)) as c:
            with pytest.raises(IdentityTokenError):
                await c.health()
    assert not anywhere.called
    assert source.calls == 1


# ── where the token goes ─────────────────────────────────────────────────────


@pytest.mark.parametrize(
    ("kwargs", "header", "expected"),
    [
        ({}, "X-Serverless-Authorization", f"Bearer {FIRST}"),
        ({"header": "X-Platform-Token", "scheme": ""}, "X-Platform-Token", FIRST),
    ],
    ids=["defaults", "own-header-no-scheme"],
)
async def test_headers_carry_both_credentials_apart(kwargs: dict[str, Any], header: str, expected: str) -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").mock(side_effect=lambda _: ok())
        async with client(TokenSourceAuth(Source(FIRST).aget, origin=BASE, **kwargs)) as c:
            await c.health()
    sent = route.calls.last.request.headers
    assert sent.get_list(header) == [expected]
    assert sent.get_list("authorization") == [f"Bearer {JJ}"]


@pytest.mark.parametrize("credential", ["api-token", "env-token"])
@pytest.mark.parametrize("header", ["Authorization", "authorization", "AUTHORIZATION"])
def test_a_token_source_on_authorization_beside_a_jamjet_token_is_refused_at_construction(
    header: str, credential: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    source = Source()
    auth = TokenSourceAuth(source.get, origin=BASE, header=header)
    kwargs: dict[str, Any] = {}
    if credential == "api-token":
        kwargs["api_token"] = JJ
    else:
        monkeypatch.setenv("JAMJET_TOKEN", JJ)
    with pytest.raises(ValueError, match="Authorization"):
        JamjetClient(BASE, auth=auth, **kwargs)
    assert source.calls == 0


async def test_a_token_source_may_use_authorization_without_a_jamjet_token() -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").mock(side_effect=lambda _: ok())
        auth = TokenSourceAuth(Source(FIRST).aget, origin=BASE, header="Authorization")
        async with JamjetClient(BASE, auth=auth) as c:
            await c.health()
    assert route.calls.last.request.headers.get_list("authorization") == [f"Bearer {FIRST}"]


ELSEWHERE = {
    "other-host": "https://other.example/health",
    "plain-http": "http://engine.test/health",
    "other-port": "https://engine.test:8443/health",
}
SAME_ORIGIN_WRITTEN_ANOTHER_WAY = "HTTPS://Engine.TEST:443/health"


@pytest.mark.parametrize("url", list(ELSEWHERE.values()), ids=list(ELSEWHERE))
async def test_the_token_goes_only_to_its_origin(url: str) -> None:
    source = Source()
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        async with httpx.AsyncClient(auth=TokenSourceAuth(source.aget, origin=BASE)) as plain:
            with pytest.raises(CredentialScopeError):
                await plain.get(url)
            assert source.calls == 0
            assert not anywhere.called
            # The same origin, written another way, gets it.
            await plain.get(SAME_ORIGIN_WRITTEN_ANOTHER_WAY)
    assert source.calls == 1
    assert anywhere.call_count == 1
    assert anywhere.calls.last.request.headers["x-serverless-authorization"] == f"Bearer {FIRST}"


async def test_a_client_for_another_origin_sends_nothing() -> None:
    source = Source()
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        async with client(TokenSourceAuth(source.aget, origin=BASE), base="https://elsewhere.test") as c:
            with pytest.raises(CredentialScopeError):
                await c.health()
    assert source.calls == 0
    assert not anywhere.called


# ── a sync client, and more than one event loop ──────────────────────────────


def test_a_sync_client_works_with_a_sync_source(waits: list[float]) -> None:
    source = Source(FIRST, SECOND)
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health")
        route.side_effect = [ok(), platform_401(), ok(), ok()]
        with httpx.Client(auth=TokenSourceAuth(source.get, origin=BASE)) as plain:
            for _ in range(3):
                assert plain.get(f"{BASE}/health").status_code == 200
    assert source.calls == 2
    sent = [call.request.headers["x-serverless-authorization"] for call in route.calls]
    assert sent == [f"Bearer {FIRST}", f"Bearer {FIRST}", f"Bearer {SECOND}", f"Bearer {SECOND}"]

    failing = Source(ConnectionRefusedError("refused"))
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        with httpx.Client(auth=TokenSourceAuth(failing.get, origin=BASE)) as plain:
            with pytest.raises(IdentityTokenError):
                plain.get(f"{BASE}/health")
    assert failing.calls == 3
    assert waits == [0.2, 0.5]
    assert not anywhere.called


@pytest.mark.parametrize("url", list(ELSEWHERE.values()), ids=list(ELSEWHERE))
def test_a_sync_client_sends_the_token_only_to_its_origin(url: str) -> None:
    source = Source()
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        with httpx.Client(auth=TokenSourceAuth(source.get, origin=BASE)) as plain:
            with pytest.raises(CredentialScopeError):
                plain.get(url)
            assert source.calls == 0
            assert not anywhere.called
            # The same origin, written another way, gets it.
            plain.get(SAME_ORIGIN_WRITTEN_ANOTHER_WAY)
    assert source.calls == 1
    assert anywhere.call_count == 1
    assert anywhere.calls.last.request.headers["x-serverless-authorization"] == f"Bearer {FIRST}"


@pytest.mark.parametrize("kind", ["async-def", "returns-a-coroutine"])
def test_a_sync_client_refuses_an_async_source(kind: str) -> None:
    source = Source()
    fetch = source.aget if kind == "async-def" else (lambda: source.aget())
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        with httpx.Client(auth=TokenSourceAuth(fetch, origin=BASE)) as plain:
            with pytest.raises(IdentityTokenError, match="async"):
                plain.get(f"{BASE}/health")
    assert source.calls == 0
    assert not anywhere.called


def test_one_auth_serves_two_event_loops(clock: Clock) -> None:
    source = Source(FIRST, SECOND)

    async def fetch() -> str:
        await asyncio.sleep(0)  # long enough for the others to wait on the lock
        return source.next()

    auth = TokenSourceAuth(fetch, origin=BASE)

    async def three_at_once() -> None:
        async with client(auth) as c:
            await asyncio.gather(*(c.health() for _ in range(3)))

    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").mock(side_effect=lambda _: ok())
        asyncio.run(three_at_once())
        clock.advance(3600)
        asyncio.run(three_at_once())
    assert source.calls == 2
    assert route.call_count == 6


# ── settings ─────────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    ("kwargs", "error"),
    [
        ({"fetch": "not a function"}, TypeError),
        ({"origin": "ftp://engine.test"}, ValueError),
        ({"origin": "engine.test"}, ValueError),
        ({"header": ""}, ValueError),
        ({"header": "X Platform"}, ValueError),
        ({"header": "X-Platform\r\nX-Injected"}, ValueError),
        ({"scheme": "Bear er"}, ValueError),
        ({"refresh_margin_s": -1}, ValueError),
        ({"refresh_margin_s": float("nan")}, ValueError),
        ({"refresh_margin_s": True}, ValueError),
    ],
    ids=[
        "fetch-not-callable",
        "origin-not-http",
        "origin-without-scheme",
        "header-empty",
        "header-with-a-space",
        "header-with-a-line-break",
        "scheme-with-a-space",
        "margin-negative",
        "margin-nan",
        "margin-a-bool",
    ],
)
def test_a_bad_setting_is_refused_at_construction(kwargs: dict[str, Any], error: type) -> None:
    settings: dict[str, Any] = {"fetch": Source().get, "origin": BASE, **kwargs}
    fetch = settings.pop("fetch")
    with pytest.raises(error):
        TokenSourceAuth(fetch, **settings)


# ── no token text anywhere ───────────────────────────────────────────────────

JJ_SENTINEL = "jj-sentinel-0b6e41d9a2"
ID_SIG_SENTINEL = "idSentinel53fa08e1c7"
ID_SENTINEL = jwt(WALL, WALL + 3600, sig=ID_SIG_SENTINEL)


class _Platform(BaseHTTPRequestHandler):
    """A loopback platform that refuses every request and quotes its identity header back."""

    def do_GET(self) -> None:  # noqa: N802
        said = self.headers.get("X-Serverless-Authorization", "")
        body = f"<html><body>401: the token {said} was refused</body></html>".encode()
        self.send_response(401)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: Any) -> None:
        pass


@pytest.fixture
def platform() -> Iterator[str]:
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Platform)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(5)


async def test_no_token_text_in_errors_repr_or_logs(platform: str, caplog: pytest.LogCaptureFixture) -> None:
    for name in (None, "httpx", "httpcore", "jamjet"):
        caplog.set_level(logging.DEBUG, logger=name)
    texts: list[str] = []
    errors: list[BaseException] = []

    refused = TokenSourceAuth(Source(ID_SENTINEL).aget, origin=platform)
    async with JamjetClient(platform, api_token=JJ_SENTINEL, auth=refused) as c:
        with pytest.raises(PlatformAuthError) as exc:
            await c.health()
    errors.append(exc.value)
    texts += [repr(refused), str(refused)]
    # The test can fail: the platform did quote the token back.
    assert ID_SENTINEL in exc.value.response.text

    for value in (f"{ID_SENTINEL}.extra", f"{JWT_HEADER}.{ID_SIG_SENTINEL}.c2ln", ID_SIG_SENTINEL):
        auth = TokenSourceAuth(Source(value).aget, origin=platform)
        async with JamjetClient(platform, api_token=JJ_SENTINEL, auth=auth) as c:
            with pytest.raises(IdentityTokenError) as bad:
                await c.health()
        errors.append(bad.value)

    no_claims = token({}, sig=ID_SIG_SENTINEL)
    quiet = TokenSourceAuth(Source(no_claims).aget, origin=platform)
    async with JamjetClient(platform, api_token=JJ_SENTINEL, auth=quiet) as c:
        with pytest.raises(PlatformAuthError) as exc:
            await c.health()
    errors.append(exc.value)

    for error in errors:
        texts += [str(error), repr(error), *(str(a) for a in error.args), getattr(error, "detail", "")]
        texts += [str(error.__cause__), repr(error.__cause__)]
    texts.append(caplog.text)
    assert "not cached" in caplog.text
    assert "HTTP Request: GET" in caplog.text
    for text in texts:
        assert JJ_SENTINEL not in text
        assert ID_SIG_SENTINEL not in text
