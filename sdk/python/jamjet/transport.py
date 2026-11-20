"""
How a request reaches the engine.

Credentials go only to the engine's own origin: the scheme, host and port of
the client's base URL. That covers the JamJet token and anything a caller's
``auth`` hook adds, such as a platform identity token. A redirect is raised,
never followed. A 401 or 403 that does not carry the engine's JSON error body
came from something in front of the engine and is raised as
``PlatformAuthError``.

A call is sent again only when that cannot do it twice: when the request never
left, when the answer was a 429, or when the call is safe to repeat. An unsafe
call whose answer was lost raises ``OutcomeUnknown`` (see ``RetryPolicy``).
"""

from __future__ import annotations

import asyncio
import email.utils
import logging
import math
import random
import time
from collections.abc import AsyncGenerator, Awaitable, Callable, Generator, Iterable, Mapping
from dataclasses import dataclass
from datetime import UTC
from typing import Any

import httpx

# How much of an error body goes into the exception message. Runtime errors can
# quote the request back (a value, a key), so keep only the start of it.
_MAX_ERROR_DETAIL = 500
_BIDI_CONTROLS = frozenset("\u200e\u200f\u202a\u202b\u202c\u202d\u202e\u2066\u2067\u2068\u2069")
_DEFAULT_PORTS = {"http": 80, "https": 443}
_REDACTED = "[redacted]"

# Nothing reached the engine: no connection was made, or none was free.
_NOT_SENT = (httpx.ConnectError, httpx.ConnectTimeout, httpx.PoolTimeout)

_log = logging.getLogger(__name__)

Origin = tuple[str, str, int | None]

# Seams for tests: how the send loop waits, draws its jitter, and reads the
# wall clock to turn a Retry-After date into seconds.
_sleep: Callable[[float], Awaitable[None]] = asyncio.sleep
_random: Callable[[], float] = random.random
_wall_clock: Callable[[], float] = time.time


@dataclass(frozen=True)
class RetryPolicy:
    """How often the client sends a call again, and how long it waits first.

    A request that never left (no connection could be made, or none was free)
    is sent again for every call, and so is a 429, which the engine did not
    process. After a lost answer (a timeout or a broken connection once the
    request may have gone out, or a 5xx) only a call that is safe to repeat is
    sent again; any other raises ``OutcomeUnknown``. Nothing else is retried.
    A call whose body can be read only once (an iterator, or a file that cannot
    seek) is sent once, since a second send would carry what was left of it.

    ``attempts`` counts every send, the first included. Before send ``n + 1``
    the client waits a random time between 0 and
    ``min(max_delay_s, base_delay_s * 2 ** (n - 1))``, or what the answer's
    ``Retry-After`` asks for. A ``Retry-After`` above ``max_retry_after_s`` is
    raised at once. ``RetryPolicy.off()`` sends every call once.
    """

    attempts: int = 3  # total sends for a retryable failure
    base_delay_s: float = 0.2  # doubling, with full jitter
    max_delay_s: float = 5.0
    max_retry_after_s: float = 30.0

    def __post_init__(self) -> None:
        if isinstance(self.attempts, bool) or not isinstance(self.attempts, int) or self.attempts < 1:
            raise ValueError(f"attempts must be a whole number, 1 or more, not {self.attempts!r}")
        for name in ("base_delay_s", "max_delay_s", "max_retry_after_s"):
            value = getattr(self, name)
            if isinstance(value, bool) or not isinstance(value, int | float) or not math.isfinite(value) or value < 0:
                raise ValueError(f"{name} must be a finite number of seconds, 0 or more, not {value!r}")

    @classmethod
    def off(cls) -> RetryPolicy:
        """Send every call once."""
        return cls(attempts=1)


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


class PlatformAuthError(JamjetAPIError):
    """A 401 or 403 from something in front of the engine (no engine error body).

    Never a separation-of-duties refusal.
    """


class CredentialScopeError(RuntimeError):
    """A request would have carried a credential to another origin. Nothing was sent."""


class OutcomeUnknown(RuntimeError):  # noqa: N818 - the name the SDK's callers match on
    """The request may have taken effect and its answer was lost. `operation` names the call."""

    def __init__(self, message: str, *, operation: str) -> None:
        super().__init__(message)
        self.operation = operation


class EngineFeatureMissing(RuntimeError):  # noqa: N818 - the name the SDK's callers match on
    """The engine does not have a feature this call needs.

    ``feature`` is the name an engine lists in ``/health``. ``engine_commit`` is
    the commit it reported, when it reported one.
    """

    def __init__(self, feature: str, *, engine_commit: str | None = None) -> None:
        super().__init__(f"This engine does not have {feature!r}. Run an engine that lists it in /health features.")
        self.feature = feature
        self.engine_commit = engine_commit


def is_engine_error_body(response: httpx.Response) -> bool:
    """True when the body is the engine's own error: JSON, an object, with a string ``error``.

    The engine answers every refusal that way. A platform's login page, an
    empty body or a JSON error shaped another way is not the engine's.
    """
    media_type = response.headers.get("content-type", "").split(";", 1)[0].strip().lower()
    if media_type != "application/json":
        return False
    try:
        body = response.json()
    except ValueError:
        return False
    return isinstance(body, dict) and isinstance(body.get("error"), str)


def origin_of(url: httpx.URL | str) -> Origin:
    """The scheme, host and port of a URL, lower-cased, with the default port filled in."""
    url = httpx.URL(url)
    scheme = url.scheme.lower()
    port = url.port if url.port is not None else _DEFAULT_PORTS.get(scheme)
    return scheme, url.host.lower(), port


def show_origin(origin: Origin) -> str:
    """An origin from ``origin_of`` written as a URL, such as ``https://engine.example:443``."""
    scheme, host, port = origin
    if ":" in host:
        host = f"[{host}]"
    return f"{scheme}://{host}" if port is None else f"{scheme}://{host}:{port}"


def _fits_a_header(value: str) -> bool:
    """True when a value can go in a header as it is: no line break, NUL or
    other control character, and no space at either end. h11 refuses some of
    these and quotes the whole value in its error."""
    return value.isprintable() and value == value.strip()


class EngineAuth(httpx.Auth):
    """The client's auth flow: the JamJet token, and a caller's ``auth`` run
    inside it, for requests to the engine's own origin only.

    A request to any other origin raises ``CredentialScopeError`` before the
    caller's hook runs and before anything is sent. Every request the hook
    yields is checked again. The token goes into ``Authorization`` last; when a
    hook or the request itself put another value there, the request is refused,
    so two credentials never share one header. A token, or a value the hook
    writes, that cannot go in a header is refused without being quoted.
    """

    def __init__(self, base_url: httpx.URL | str, *, token: str | None, auth: httpx.Auth | None) -> None:
        if token and not (isinstance(token, str) and token.isascii() and _fits_a_header(token)):
            raise ValueError(
                "The JamJet token cannot go in a header as it is: it has a line break, another control character "
                "or a character that is not ASCII, or a space at one end. A token read from a file often ends "
                "with a line break. The token is not shown."
            )
        if token and auth is not None:
            # Imported here: that module imports this one.
            from jamjet.auth.token_source import TokenSourceAuth

            if isinstance(auth, TokenSourceAuth) and auth.header.lower() == "authorization":
                raise ValueError(
                    "Two credentials for the Authorization header. It carries the JamJet token, so the token "
                    "source must use another header, such as X-Serverless-Authorization."
                )
        self._origin = origin_of(base_url)
        self._authorization = f"Bearer {token}" if token else None
        self._auth = auth
        self._written: set[str] = {"authorization"}

    @property
    def credential_headers(self) -> frozenset[str]:
        """The headers that can carry a credential: ``Authorization`` and every header the caller's hook wrote."""
        return frozenset(self._written)

    def _admit(self, request: httpx.Request) -> None:
        origin = origin_of(request.url)
        if origin != self._origin:
            raise CredentialScopeError(
                f"Refused to send a credential to {show_origin(origin)}. This client sends its credentials only to "
                f"{show_origin(self._origin)}, the origin of its base URL. The request was not sent."
            )

    def _add_token(self, request: httpx.Request) -> None:
        if self._authorization is None:
            return
        if any(value != self._authorization for value in request.headers.get_list("authorization")):
            raise ValueError(
                "Two credentials for the Authorization header. It carries the JamJet token, so the auth hook "
                "or the request must put its credential in another header, such as X-Serverless-Authorization. "
                "The request was not sent."
            )
        request.headers["Authorization"] = self._authorization

    async def async_auth_flow(self, request: httpx.Request) -> AsyncGenerator[httpx.Request, httpx.Response]:
        self._admit(request)
        if self._auth is None:
            self._add_token(request)
            yield request
            return
        if self._auth.requires_request_body:
            await request.aread()
        before = set(request.headers.multi_items())
        flow = self._auth.async_auth_flow(request)
        try:
            outgoing = await flow.__anext__()
            while True:
                self._admit(outgoing)
                written = [
                    (name, value) for name, value in outgoing.headers.multi_items() if (name, value) not in before
                ]
                for name, value in written:
                    if not _fits_a_header(value):
                        raise ValueError(
                            f"The auth hook put a value in {_clean_detail(name)} that cannot go in a header: "
                            "it has a line break or another control character, or a space at one end. "
                            "The value is not shown. The request was not sent."
                        )
                self._written.update(name for name, _ in written)
                self._add_token(outgoing)
                response = yield outgoing
                if self._auth.requires_response_body:
                    await response.aread()
                try:
                    outgoing = await flow.asend(response)
                except StopAsyncIteration:
                    return
        finally:
            await flow.aclose()

    def sync_auth_flow(self, request: httpx.Request) -> Generator[httpx.Request, httpx.Response, None]:
        raise RuntimeError("the JamJet client's auth flow runs on an async client only")


def _credential_values(request: httpx.Request, headers: Iterable[str]) -> list[str]:
    """Each credential the request carried, whole and without its scheme, longest first."""
    values: set[str] = set()
    for name in {"authorization", *headers}:
        for value in request.headers.get_list(name):
            values.add(value)
            values.add(value.partition(" ")[2])
    return sorted((v for v in values if v), key=len, reverse=True)


def _redact(text: str, secrets: Iterable[str]) -> str:
    for secret in secrets:
        text = text.replace(secret, _REDACTED)
    return text


def _scrub(error: BaseException, secrets: Iterable[str]) -> None:
    """Replace each secret in the message of ``error`` and of every exception
    it holds or chains to, in place, so no traceback of any of them shows one.

    A secret is matched as it is and as Python quotes it, the way h11 quotes a
    header value it refuses.
    """
    forms = sorted({form for s in secrets for form in (s, repr(s)[1:-1]) if form}, key=len, reverse=True)
    if not forms:
        return
    seen: set[int] = set()
    todo = [error]
    while todo:
        current = todo.pop()
        if id(current) in seen:
            continue
        seen.add(id(current))
        current.args = tuple(_redact(arg, forms) if isinstance(arg, str) else arg for arg in current.args)
        linked = (current.__cause__, current.__context__, *current.args)
        todo.extend(e for e in linked if isinstance(e, BaseException))


def _request_secrets(error: httpx.RequestError, credential_headers: Iterable[str]) -> list[str]:
    """The credentials on the request ``error`` is about, or none when it names no request."""
    try:
        request = error.request
    except RuntimeError:  # httpx sets it on every error from a send; this is in case it did not
        return []
    return _credential_values(request, credential_headers)


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


def _error_detail(response: httpx.Response, secrets: Iterable[str] = ()) -> str:
    """The server's reason for a failed request, or "" when the body is empty.
    A credential the server quoted back is replaced before the text is cut."""
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
    return _clean_detail(_redact(text, secrets))


def _status_line(response: httpx.Response, secrets: Iterable[str]) -> str:
    request = response.request
    return _redact(f"{response.status_code} {response.reason_phrase} for {request.method} {request.url}", secrets)


def platform_auth_error(
    response: httpx.Response, advice: str, *, credential_headers: Iterable[str] = ()
) -> PlatformAuthError:
    """A ``PlatformAuthError`` for ``response``: its status line, then
    ``advice``, then what the answer said. Credentials are replaced as in
    ``raise_for_status``."""
    request = response.request
    secrets = _credential_values(request, credential_headers)
    detail = _error_detail(response, secrets)
    message = f"{_status_line(response, secrets)}. {advice}"
    if detail:
        message = f"{message} The answer said: {detail}"
    return PlatformAuthError(message, request=request, response=response, detail=detail)


def raise_for_status(response: httpx.Response, *, credential_headers: Iterable[str] = ()) -> None:
    """``response.raise_for_status()``, but the raised error carries the
    server's message instead of only the status line.

    A 401 or 403 without an engine error body raises ``PlatformAuthError``. A
    redirect raises ``JamjetAPIError`` and says it was not followed. The value
    of ``Authorization`` and of each of ``credential_headers`` on the request is
    replaced with ``[redacted]`` wherever the answer quotes it.
    """
    try:
        response.raise_for_status()
    except httpx.HTTPStatusError as exc:
        if response.status_code in (401, 403) and not is_engine_error_body(response):
            raise platform_auth_error(
                response,
                "The refusal came from something in front of the engine, not the engine: the answer has no "
                "engine error body. Check that the client sends the identity the platform expects "
                "(JamjetClient(auth=...)) and that this caller may call the service.",
                credential_headers=credential_headers,
            ) from exc
        request = response.request
        secrets = _credential_values(request, credential_headers)
        detail = _error_detail(response, secrets)
        message = _status_line(response, secrets)
        if response.has_redirect_location:
            message = f"{message} (redirects are not followed)"
        if detail:
            message = f"{message}: {detail}"
        raise JamjetAPIError(message, request=request, response=response, detail=detail) from exc


def _retry_after(response: httpx.Response) -> float | None:
    """The wait an answer's ``Retry-After`` asks for, in seconds.

    The header holds whole seconds or an HTTP date; a date in the past asks for
    no wait. None when there is no header or it cannot be read.
    """
    value = response.headers.get("retry-after", "").strip()
    if not value:
        return None
    if value.isascii() and value.isdigit():
        return float(value) if len(value) <= 15 else math.inf
    try:
        when = email.utils.parsedate_to_datetime(value)
    except (TypeError, ValueError, IndexError, OverflowError):
        return None
    if when.tzinfo is None:  # no zone, or "-0000": an HTTP date is in GMT
        when = when.replace(tzinfo=UTC)
    return max(0.0, when.timestamp() - _wall_clock())


def body_is_replayable(*, content: Any = None, data: Any = None, files: Any = None) -> bool:
    """True when every send of a request built from these httpx arguments
    carries the same body.

    bytes, str, ``json`` and form fields are encoded again for each send, and
    so are multipart files given as bytes, str or a file that can seek. An
    iterator, or a file that cannot seek, is read once: a second send would go
    out short or empty. The rules follow the order httpx reads the arguments in.
    """
    if data is not None and not isinstance(data, Mapping):
        content = data  # httpx sends a data= that is not a form as raw content
    if content is not None:
        return isinstance(content, bytes | str)
    if not files:
        return True
    try:
        values = files.values() if isinstance(files, Mapping) else [value for _, value in files]
        return all(_reads_again(value[1] if isinstance(value, tuple) else value) for value in values)
    except (TypeError, ValueError, IndexError):  # not files httpx can send; it raises its own error
        return False


def _reads_again(file: Any) -> bool:
    """True for multipart file content that httpx reads from the start on every send."""
    if isinstance(file, bytes | str):
        return True
    seekable = getattr(file, "seekable", None)
    if not callable(seekable) or not callable(getattr(file, "seek", None)):
        return False
    try:
        return bool(seekable())
    except (OSError, ValueError):  # a closed file
        return False


def _backoff(policy: RetryPolicy, sends: int) -> float:
    """Full jitter: a uniform draw up to the delay for this retry, which doubles from ``base_delay_s``."""
    ceiling = min(policy.max_delay_s, policy.base_delay_s * 2.0 ** min(sends - 1, 64))
    return ceiling * _random()


def _outcome_unknown(operation: str, what: str) -> OutcomeUnknown:
    return OutcomeUnknown(
        f"{operation} may have taken effect, but {what}. It was not sent again, because a second send could do "
        "it twice. Check whether it took effect before you repeat it.",
        operation=operation,
    )


async def send_with_retries(
    send: Callable[[], Awaitable[httpx.Response]],
    *,
    check: Callable[[httpx.Response], None],
    safe: bool,
    replayable: bool,
    operation: str,
    policy: RetryPolicy,
    credential_headers: Callable[[], Iterable[str]] = tuple,
) -> httpx.Response:
    """Send a call, and send it again while ``policy`` and the failure allow.

    ``send`` makes one attempt and ``check`` raises ``JamjetAPIError`` for an
    answer that is not 2xx. ``safe`` says the call may be repeated after it may
    have taken effect; ``operation`` names it in ``OutcomeUnknown``. A call
    whose body is not ``replayable`` (see ``body_is_replayable``) is sent once.
    After the last send the last failure is raised as it is. A 401 is never
    sent again here: an auth hook that refreshes a token sends its own one
    resend. A transport error has the value of ``Authorization`` and of each
    of ``credential_headers()`` replaced in its message and its chain.
    """
    # Such a body is not sent again even when the request never left: an auth
    # hook may have read it before the connection was tried.
    attempts = policy.attempts if replayable else 1
    sends = 0
    while True:
        sends += 1
        try:
            response = await send()
        except httpx.RequestError as exc:
            _scrub(exc, _request_secrets(exc, credential_headers()))
            # Nothing left, and a second send would fail the same way: a URL the
            # client cannot send to, or a request h11 would not write. Over
            # HTTP/1.1, the only protocol this client speaks, httpx raises
            # LocalProtocolError only before the first byte of the request.
            if isinstance(exc, httpx.UnsupportedProtocol | httpx.LocalProtocolError):
                raise
            if not safe and not isinstance(exc, _NOT_SENT):
                raise _outcome_unknown(operation, f"its answer was lost ({type(exc).__name__})") from exc
            if sends >= attempts:
                raise
            reason = type(exc).__name__
            wait = _backoff(policy, sends)
        else:
            try:
                check(response)
                return response
            except JamjetAPIError as exc:
                status = response.status_code
                if status != 429 and status < 500:
                    raise
                if status >= 500 and not safe:
                    raise _outcome_unknown(
                        operation,
                        f"the answer was {status} {response.reason_phrase}, which does not say whether it did",
                    ) from exc
                asked = _retry_after(response)
                if asked is not None and asked > policy.max_retry_after_s:
                    raise
                if sends >= attempts:
                    raise
                reason = str(status)
                wait = _backoff(policy, sends) if asked is None else asked
        _log.debug(
            "%s failed (%s); sending it again in %.2f s, send %d of %d",
            operation,
            reason,
            wait,
            sends + 1,
            attempts,
        )
        await _sleep(wait)
