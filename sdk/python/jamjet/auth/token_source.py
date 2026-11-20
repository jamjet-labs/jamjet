"""
An auth hook that puts an identity token from a token source on every request.

``TokenSourceAuth`` is for an engine behind a platform that checks the caller's
identity before a request reaches the engine. It asks ``fetch`` for a token,
keeps it for the token's own lifetime and puts it in a header of its own, so
the JamJet token keeps ``Authorization``::

    auth = TokenSourceAuth(get_token, origin="https://engine.example")
    client = JamjetClient("https://engine.example", api_token=jamjet_token, auth=auth)

The token goes only to ``origin``. A request is never sent without it: when the
source fails, or gives something that is not a token, ``IdentityTokenError`` is
raised and nothing is sent. A 401 from the platform gets one fresh token and
one resend; a second one raises ``PlatformAuthError``.
"""

from __future__ import annotations

import asyncio
import base64
import concurrent.futures
import inspect
import json
import logging
import math
import re
import threading
import time
import weakref
from collections.abc import AsyncGenerator, Awaitable, Callable, Generator
from dataclasses import dataclass
from typing import Any

import httpx

from jamjet.transport import (
    CredentialScopeError,
    PlatformAuthError,
    is_engine_error_body,
    origin_of,
    platform_auth_error,
    show_origin,
)

# The waits before the second and the third try of a fetch that failed in transit.
_FETCH_WAITS = (0.2, 0.5)
# How long one try of a fetch may take when neither fetch_timeout_s nor the
# request's timeout sets it.
_FETCH_TIMEOUT_S = 30.0
_BASE64URL = re.compile(r"[A-Za-z0-9_-]+")
_HTTP_TOKEN = re.compile(r"[!#$%&'*+.^_`|~0-9A-Za-z-]+")  # a header name, or a scheme such as Bearer

_log = logging.getLogger(__name__)

# Seams for tests: the clock a token's lifetime is counted on, and how the
# tries of a failed fetch wait.
_monotonic: Callable[[], float] = time.monotonic
_sleep: Callable[[float], Awaitable[None]] = asyncio.sleep
_sleep_sync: Callable[[float], None] = time.sleep


class IdentityTokenError(RuntimeError):
    """The token source failed or gave something that is not a token. Nothing was sent."""


class _FetchTimeoutError(TimeoutError):
    """A try of the fetch ran past its limit. A TimeoutError, so it is tried again like one."""

    def __init__(self, limit: float) -> None:
        super().__init__(f"the token source did not answer within {limit:g} s")
        self.limit = limit


def _on_own_thread(fetch: Callable[[], Any]) -> concurrent.futures.Future[Any]:
    """Start ``fetch`` on a daemon thread of its own.

    One that never returns then holds up no thread pool that other work needs,
    and not the program's exit.
    """
    future: concurrent.futures.Future[Any] = concurrent.futures.Future()

    def run() -> None:
        if not future.set_running_or_notify_cancel():
            return
        try:
            value = fetch()
        except BaseException as exc:  # handed to the waiter as it is
            future.set_exception(exc)
        else:
            future.set_result(value)

    threading.Thread(target=run, name="jamjet-token-source", daemon=True).start()
    return future


async def _within(future: asyncio.Future[Any], limit: float) -> Any:
    """What ``future`` gives, or ``_FetchTimeoutError`` once ``limit`` seconds pass.

    A future still running then is cancelled, and not waited for.
    """
    try:
        done, _ = await asyncio.wait({future}, timeout=limit)
    finally:
        if not future.done():
            future.cancel()
    if not done:
        raise _FetchTimeoutError(limit)
    return future.result()


@dataclass(frozen=True)
class _Cached:
    value: str
    refresh_at: float  # on the monotonic clock


def _seconds(value: Any) -> float | None:
    """A claim as seconds, or None when it is not a finite number."""
    if isinstance(value, bool) or not isinstance(value, int | float):
        return None
    try:
        seconds = float(value)
    except OverflowError:
        return None
    return seconds if math.isfinite(seconds) else None


def _positive_seconds(value: Any) -> bool:
    return not isinstance(value, bool) and isinstance(value, int | float) and math.isfinite(value) and value > 0


def _claims(value: Any) -> tuple[float, float] | None:
    """The token's ``iat`` and ``exp``, or None when it has no readable pair.

    Raises ``IdentityTokenError`` for something that is not a token. No message
    quotes the token.
    """
    if not isinstance(value, str):
        raise IdentityTokenError(f"The token source gave {type(value).__name__}, not a token string.")
    parts = value.split(".")
    if len(parts) != 3 or not all(_BASE64URL.fullmatch(part) for part in parts):
        raise IdentityTokenError(
            "The token source gave something that is not a token: a token is three base64url parts joined by dots."
        )
    payload = parts[1]
    try:
        claims = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
    except ValueError:
        raise IdentityTokenError("The token source gave a token whose middle part is not JSON.") from None
    if not isinstance(claims, dict):
        raise IdentityTokenError("The token source gave a token whose claims are not a JSON object.")
    iat, exp = _seconds(claims.get("iat")), _seconds(claims.get("exp"))
    if iat is None or exp is None:
        return None
    if exp <= iat:
        raise IdentityTokenError("The token source gave a token that expires at or before it was issued.")
    return iat, exp


def _platform_refused(response: httpx.Response) -> bool:
    return response.status_code == 401 and not is_engine_error_body(response)


def _not_sent(again: bool) -> str:
    return "The request was not sent again." if again else "Nothing was sent."


class TokenSourceAuth(httpx.Auth):
    """An identity token from ``fetch``, in ``header``, on every request to ``origin``.

    ``fetch`` takes no arguments and returns the token, or an awaitable of it.
    A plain function runs on a thread of its own, so a slow source does not
    hold up the event loop. A sync client (``httpx.Client``) needs a plain
    function.

    The token is kept for its own lifetime, ``exp - iat`` from its claims,
    counted on the monotonic clock from the start of the fetch, so a wall clock
    that is ahead or behind changes nothing. A fresh one is fetched
    ``refresh_margin_s`` before that ends, the margin capped at half the
    lifetime. A token without a readable ``exp`` and ``iat`` is not kept: it is
    fetched for every request, with one warning. One fetch runs at a time.
    Requests that need a token meanwhile wait for it, and share its error if
    it fails.

    A fetch that fails in transit (``OSError``, which covers ``TimeoutError``
    and ``ConnectionError``, or ``httpx.TransportError``) is tried twice more,
    after 0.2 s and 0.5 s. After that, at once for any other error, and for
    something that is not a token, ``IdentityTokenError`` is raised and the
    request is not sent.

    Each try may take ``fetch_timeout_s`` seconds. By default that is the
    timeout of the request that needs the token, its longest part when its
    parts differ, so the client's ``timeout`` covers the fetch too; 30 s when
    the request has none. A try that runs past it counts as one that failed
    in transit, and the requests waiting on it get the same error. An async
    source that runs past it is cancelled. A plain function cannot be
    stopped: its thread runs on until it returns, but nothing waits for it.

    A request to any other origin raises ``CredentialScopeError`` before a
    token is fetched. Use it on a client that does not follow redirects
    (``JamjetClient`` never does): httpx keeps a header like this one on a
    redirect to another host.

    A 401 that does not carry the engine's error body came from the platform:
    the token is fetched again and the request sent once more. A second such
    401 raises ``PlatformAuthError``. The engine's own 401 is left alone.
    """

    requires_request_body = True  # a resend after a 401 carries the whole body
    requires_response_body = True

    def __init__(
        self,
        fetch: Callable[[], Awaitable[str]] | Callable[[], str],
        *,
        origin: str,
        header: str = "X-Serverless-Authorization",
        scheme: str = "Bearer",
        refresh_margin_s: float = 300.0,
        fetch_timeout_s: float | None = None,
    ) -> None:
        if not callable(fetch):
            raise TypeError("fetch must be a function that returns a token")
        url = httpx.URL(origin)
        if url.scheme not in ("http", "https") or not url.host:
            raise ValueError("origin must be an http or https URL with a host, such as https://engine.example")
        if not isinstance(header, str) or not _HTTP_TOKEN.fullmatch(header):
            raise ValueError("header must be an HTTP header name, such as X-Serverless-Authorization")
        if not isinstance(scheme, str) or (scheme and not _HTTP_TOKEN.fullmatch(scheme)):
            raise ValueError("scheme must be one word, such as Bearer, or empty for the token alone")
        margin = refresh_margin_s
        if isinstance(margin, bool) or not isinstance(margin, int | float) or not math.isfinite(margin) or margin < 0:
            raise ValueError(f"refresh_margin_s must be a finite number of seconds, 0 or more, not {margin!r}")
        limit = fetch_timeout_s
        if limit is not None and not _positive_seconds(limit):
            raise ValueError(f"fetch_timeout_s must be a finite number of seconds above 0, or None, not {limit!r}")
        self._fetch: Callable[[], Any] = fetch
        self._fetch_timeout = None if limit is None else float(limit)
        self._fetch_is_async = inspect.iscoroutinefunction(fetch)
        self._origin = origin_of(url)
        self._header = header
        self._scheme = scheme
        self._margin = float(margin)
        # Fetch errors that are tried again, and errors that fail at once even
        # when they are also of a kind in _retry_on. Subclasses add their own.
        self._retry_on: tuple[type[BaseException], ...] = (OSError, httpx.TransportError)
        self._fail_on: tuple[type[BaseException], ...] = ()
        self._cached: _Cached | None = None
        self._rounds = 0  # fetch rounds finished, so a request that waited on one can tell
        self._failure: tuple[str, BaseException | None] | None = None  # why the last round failed
        self._warned = False
        self._thread_lock = threading.Lock()
        self._loop_locks: weakref.WeakKeyDictionary[asyncio.AbstractEventLoop, asyncio.Lock] = (
            weakref.WeakKeyDictionary()
        )

    @property
    def header(self) -> str:
        """The header the token goes in."""
        return self._header

    @property
    def origin(self) -> str:
        """The one origin the token goes to."""
        return show_origin(self._origin)

    def __repr__(self) -> str:
        return f"{type(self).__name__}(origin={self.origin!r}, header={self._header!r})"

    # ── the flows ───────────────────────────────────────────────────────────

    async def async_auth_flow(self, request: httpx.Request) -> AsyncGenerator[httpx.Request, httpx.Response]:
        self._admit(request)
        await request.aread()
        limit = self._limit_for(request)
        token = await self._token_async(stale=None, limit=limit)
        self._put(request, token)
        response = yield request
        if response.status_code != 401:
            return
        await response.aread()
        if not _platform_refused(response):
            return
        _log.debug("the platform refused the identity token (401); fetching a fresh one and sending once more")
        self._put(request, await self._token_async(stale=token, limit=limit))
        response = yield request
        if response.status_code == 401:
            await response.aread()
            if _platform_refused(response):
                raise self._refused(response)

    def sync_auth_flow(self, request: httpx.Request) -> Generator[httpx.Request, httpx.Response, None]:
        self._admit(request)
        request.read()
        limit = self._limit_for(request)
        token = self._token_sync(stale=None, limit=limit)
        self._put(request, token)
        response = yield request
        if response.status_code != 401:
            return
        response.read()
        if not _platform_refused(response):
            return
        _log.debug("the platform refused the identity token (401); fetching a fresh one and sending once more")
        self._put(request, self._token_sync(stale=token, limit=limit))
        response = yield request
        if response.status_code == 401:
            response.read()
            if _platform_refused(response):
                raise self._refused(response)

    def _admit(self, request: httpx.Request) -> None:
        origin = origin_of(request.url)
        if origin != self._origin:
            raise CredentialScopeError(
                f"Refused to send the identity token to {show_origin(origin)}. It goes only to {self.origin}. "
                "The request was not sent."
            )

    def _limit_for(self, request: httpx.Request) -> float:
        """How long one try of the fetch may take for ``request``."""
        if self._fetch_timeout is not None:
            return self._fetch_timeout
        parts = request.extensions.get("timeout")
        seconds = [float(v) for v in parts.values() if _positive_seconds(v)] if isinstance(parts, dict) else []
        return max(seconds, default=_FETCH_TIMEOUT_S)

    def _put(self, request: httpx.Request, token: str) -> None:
        request.headers[self._header] = f"{self._scheme} {token}" if self._scheme else token

    def _refused(self, response: httpx.Response) -> PlatformAuthError:
        return platform_auth_error(
            response,
            "The platform refused a fresh identity token: check the audience and the caller's invoker permission.",
            credential_headers=(self._header,),
        )

    # ── getting a token ─────────────────────────────────────────────────────

    def _usable(self, stale: str | None) -> str | None:
        """The kept token, unless it is due for a refresh or is the one the platform just refused."""
        cached = self._cached
        if cached is None or cached.value == stale or _monotonic() >= cached.refresh_at:
            return None
        return cached.value

    def _shared_failure(self, rounds_before: int, *, again: bool) -> IdentityTokenError | None:
        """The error of a round that failed while this request waited on it."""
        if self._rounds == rounds_before or self._failure is None:
            return None
        reason, cause = self._failure
        error = IdentityTokenError(f"{reason} {_not_sent(again)}")
        error.__cause__ = cause
        return error

    def _async_lock(self) -> asyncio.Lock:
        # One lock per event loop: an asyncio.Lock is bound to the loop it first waits on.
        loop = asyncio.get_running_loop()
        lock = self._loop_locks.get(loop)
        if lock is None:
            lock = self._loop_locks[loop] = asyncio.Lock()
        return lock

    async def _token_async(self, *, stale: str | None, limit: float) -> str:
        again = stale is not None
        token = self._usable(stale)
        if token is not None:
            return token
        rounds_before = self._rounds
        async with self._async_lock():
            token = self._usable(stale)
            if token is not None:
                return token
            failure = self._shared_failure(rounds_before, again=again)
            if failure is not None:
                raise failure
            self._failure = None
            try:
                tried = 0
                while True:
                    tried += 1
                    started = _monotonic()
                    try:
                        value = await self._call_async(limit)
                    except Exception as exc:
                        await _sleep(self._wait_or_raise(exc, tried, again=again))
                    else:
                        return self._keep(value, started, again=again)
            finally:
                self._rounds += 1

    def _token_sync(self, *, stale: str | None, limit: float) -> str:
        again = stale is not None
        if self._fetch_is_async:
            raise IdentityTokenError(
                "This token source is async, so it works on an async client only (httpx.AsyncClient or "
                f"JamjetClient). {_not_sent(again)}"
            )
        token = self._usable(stale)
        if token is not None:
            return token
        rounds_before = self._rounds
        with self._thread_lock:
            token = self._usable(stale)
            if token is not None:
                return token
            failure = self._shared_failure(rounds_before, again=again)
            if failure is not None:
                raise failure
            self._failure = None
            try:
                tried = 0
                while True:
                    tried += 1
                    started = _monotonic()
                    try:
                        value = self._call_sync(limit)
                    except Exception as exc:
                        _sleep_sync(self._wait_or_raise(exc, tried, again=again))
                    else:
                        return self._keep(value, started, again=again)
            finally:
                self._rounds += 1

    async def _call_async(self, limit: float) -> Any:
        if self._fetch_is_async:
            value = await _within(asyncio.ensure_future(self._fetch()), limit)
        else:
            value = await _within(asyncio.wrap_future(_on_own_thread(self._fetch)), limit)
        if inspect.isawaitable(value):
            value = await _within(asyncio.ensure_future(value), limit)
        return value

    def _call_sync(self, limit: float) -> Any:
        future = _on_own_thread(self._fetch)
        done, _ = concurrent.futures.wait([future], timeout=limit)
        if not done:
            future.cancel()
            raise _FetchTimeoutError(limit)
        value = future.result()
        if inspect.isawaitable(value):
            if inspect.iscoroutine(value):
                value.close()
            raise IdentityTokenError(
                "This token source returned an awaitable, so it works on an async client only "
                "(httpx.AsyncClient or JamjetClient)."
            )
        return value

    def _wait_or_raise(self, exc: Exception, tried: int, *, again: bool) -> float:
        """The wait before the next try of a failed fetch, or the error to raise."""
        cause: BaseException | None
        if isinstance(exc, IdentityTokenError):
            reason, cause = str(exc), None
        else:
            if isinstance(exc, _FetchTimeoutError):
                what = f"did not answer within {exc.limit:g} s"
            else:
                what = f"failed ({type(exc).__name__})"
            in_transit = isinstance(exc, self._retry_on) and not isinstance(exc, self._fail_on)
            if in_transit and tried <= len(_FETCH_WAITS):
                wait = _FETCH_WAITS[tried - 1]
                _log.debug(
                    "the identity token source %s; trying again in %.1f s, try %d of %d",
                    what,
                    wait,
                    tried + 1,
                    len(_FETCH_WAITS) + 1,
                )
                return wait
            if in_transit:
                reason = f"The identity token source {what} on each of {tried} tries."
            else:
                reason = f"The identity token source {what}."
            cause = exc
        self._failure = (reason, cause)
        raise IdentityTokenError(f"{reason} {_not_sent(again)}") from cause

    def _keep(self, value: Any, started: float, *, again: bool) -> str:
        """Check what the source gave, and keep it for its lifetime when it has one."""
        try:
            claims = _claims(value)
        except IdentityTokenError as exc:
            self._failure = (str(exc), None)
            raise IdentityTokenError(f"{exc} {_not_sent(again)}") from None
        if claims is None:
            self._cached = None
            if not self._warned:
                self._warned = True
                _log.warning(
                    "The identity token has no readable exp and iat claims, so it is not cached: the token "
                    "source is called for every request."
                )
            return str(value)
        iat, exp = claims
        lifetime = exp - iat
        margin = min(self._margin, lifetime / 2)
        self._cached = _Cached(value, started + lifetime - margin)
        return str(value)
