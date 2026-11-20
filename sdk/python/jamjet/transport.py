"""
How a request reaches the engine.

Credentials go only to the engine's own origin: the scheme, host and port of
the client's base URL. That covers the JamJet token and anything a caller's
``auth`` hook adds, such as a platform identity token. A redirect is raised,
never followed. A 401 or 403 that does not carry the engine's JSON error body
came from something in front of the engine and is raised as
``PlatformAuthError``.
"""

from __future__ import annotations

from collections.abc import AsyncGenerator, Generator, Iterable

import httpx

# How much of an error body goes into the exception message. Runtime errors can
# quote the request back (a value, a key), so keep only the start of it.
_MAX_ERROR_DETAIL = 500
_BIDI_CONTROLS = frozenset("‎‏‪‫‬‭‮⁦⁧⁨⁩")
_DEFAULT_PORTS = {"http": 80, "https": 443}
_REDACTED = "[redacted]"

Origin = tuple[str, str, int | None]


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


def _show(origin: Origin) -> str:
    scheme, host, port = origin
    if ":" in host:
        host = f"[{host}]"
    return f"{scheme}://{host}" if port is None else f"{scheme}://{host}:{port}"


class EngineAuth(httpx.Auth):
    """The client's auth flow: the JamJet token, and a caller's ``auth`` run
    inside it, for requests to the engine's own origin only.

    A request to any other origin raises ``CredentialScopeError`` before the
    caller's hook runs and before anything is sent. Every request the hook
    yields is checked again. The token goes into ``Authorization`` last; when a
    hook or the request itself put another value there, the request is refused,
    so two credentials never share one header.
    """

    def __init__(self, base_url: httpx.URL | str, *, token: str | None, auth: httpx.Auth | None) -> None:
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
                f"Refused to send a credential to {_show(origin)}. This client sends its credentials only to "
                f"{_show(self._origin)}, the origin of its base URL. The request was not sent."
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
                written = (name for name, value in outgoing.headers.multi_items() if (name, value) not in before)
                self._written.update(written)
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
        request = response.request
        secrets = _credential_values(request, credential_headers)
        detail = _error_detail(response, secrets)
        line = _redact(f"{response.status_code} {response.reason_phrase} for {request.method} {request.url}", secrets)
        if response.status_code in (401, 403) and not is_engine_error_body(response):
            message = (
                f"{line}. The refusal came from something in front of the engine, not the engine: the answer "
                "has no engine error body. Check that the client sends the identity the platform expects "
                "(JamjetClient(auth=...)) and that this caller may call the service."
            )
            if detail:
                message = f"{message} The answer said: {detail}"
            raise PlatformAuthError(message, request=request, response=response, detail=detail) from exc
        message = line
        if response.has_redirect_location:
            message = f"{message} (redirects are not followed)"
        if detail:
            message = f"{message}: {detail}"
        raise JamjetAPIError(message, request=request, response=response, detail=detail) from exc
