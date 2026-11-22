"""Credentials go to the engine's own origin and nowhere else.

The JamJet token and anything a caller's ``auth`` adds travel only to the
scheme, host and port of the client's base URL. A redirect is never followed.
A 401 or 403 that does not carry the engine's JSON error body came from
something in front of the engine and is raised as ``PlatformAuthError``.
"""

from __future__ import annotations

import inspect
import json
import logging
import threading
import traceback
from collections.abc import Awaitable, Callable, Generator, Iterator
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import httpcore
import httpx
import pytest
import respx

from jamjet import transport
from jamjet.client import JamjetAPIError, JamjetClient
from jamjet.transport import CredentialScopeError, OutcomeUnknown, PlatformAuthError, is_engine_error_body

BASE = "http://engine.test:7700"
TOKEN = "jj-test-token-5d1e"


@pytest.fixture(autouse=True)
def _no_env_token(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("JAMJET_TOKEN", raising=False)


class HeaderAuth(httpx.Auth):
    """A caller's auth hook: one header on every request."""

    def __init__(self, name: str = "X-Serverless-Authorization", value: str = "Bearer id-token-1") -> None:
        self.name = name
        self.value = value

    def auth_flow(self, request: httpx.Request) -> Generator[httpx.Request, httpx.Response, None]:
        request.headers[self.name] = self.value
        yield request


class RefreshOn401(httpx.Auth):
    """Sends again once, with a new header value, after a 401."""

    requires_response_body = True

    def __init__(self, header: str = "X-Serverless-Authorization") -> None:
        self.header = header
        self.fetches = 0

    def auth_flow(self, request: httpx.Request) -> Generator[httpx.Request, httpx.Response, None]:
        self.fetches += 1
        request.headers[self.header] = f"Bearer id-{self.fetches}"
        response = yield request
        if response.status_code == 401:
            self.fetches += 1
            request.headers[self.header] = f"Bearer id-{self.fetches}"
            yield request


class WritesAuthorizationOnResend(httpx.Auth):
    """Uses its own header first, then Authorization when it sends again."""

    def auth_flow(self, request: httpx.Request) -> Generator[httpx.Request, httpx.Response, None]:
        request.headers["X-Serverless-Authorization"] = "Bearer id-1"
        response = yield request
        if response.status_code == 401:
            request.headers["Authorization"] = "Bearer id-2"
            yield request


class MovesTheRequest(httpx.Auth):
    """A hook that swaps the request for one to another host."""

    def auth_flow(self, request: httpx.Request) -> Generator[httpx.Request, httpx.Response, None]:
        yield httpx.Request(request.method, "https://other.example/x", headers=request.headers)


# ── one origin ───────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "credential",
    ["jamjet-token", "env-token", "caller-auth"],
)
async def test_the_token_goes_only_to_the_base_origin(credential: str, monkeypatch: pytest.MonkeyPatch) -> None:
    kwargs: dict[str, Any] = {}
    if credential == "jamjet-token":
        kwargs["api_token"] = TOKEN
    elif credential == "env-token":
        monkeypatch.setenv("JAMJET_TOKEN", TOKEN)
    else:
        kwargs["auth"] = HeaderAuth()
    with respx.mock(assert_all_called=False) as mock:
        other = mock.route(host="other.example").respond(200, json={})
        engine = mock.get(f"{BASE}/health").respond(200, json={"status": "ok"})
        async with JamjetClient(BASE, **kwargs) as client:
            with pytest.raises(CredentialScopeError):
                await client.get("https://other.example/x")
            assert await client.health() == {"status": "ok"}
    assert not other.called
    sent = engine.calls.last.request.headers
    if credential == "caller-auth":
        assert sent["x-serverless-authorization"] == "Bearer id-token-1"
        assert "authorization" not in sent
    else:
        assert sent["authorization"] == f"Bearer {TOKEN}"


@pytest.mark.parametrize(
    "url",
    [
        "http://engine.test:7701/health",
        "https://engine.test:7700/health",
        "http://engine.test/health",
        "http://engine.test:80/health",
    ],
    ids=["other-port", "https", "default-port", "port-80"],
)
async def test_another_port_or_scheme_is_another_origin(url: str) -> None:
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        async with JamjetClient(BASE, api_token=TOKEN) as client:
            with pytest.raises(CredentialScopeError):
                await client.get(url)
    assert not anywhere.called


@pytest.mark.parametrize(
    ("base", "url"),
    [
        ("http://engine.test:7700", "HTTP://Engine.TEST:7700/health"),
        ("https://engine.test", "https://engine.test:443/health"),
        ("http://engine.test:80", "http://engine.test/health"),
    ],
)
async def test_the_same_origin_written_differently_gets_the_token(base: str, url: str) -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.route().respond(200, json={})
        async with JamjetClient(base, api_token=TOKEN) as client:
            await client.get(url)
    assert route.calls.last.request.headers["authorization"] == f"Bearer {TOKEN}"


async def test_a_caller_auth_cannot_move_the_request_to_another_origin() -> None:
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        async with JamjetClient(BASE, api_token=TOKEN, auth=MovesTheRequest()) as client:
            with pytest.raises(CredentialScopeError):
                await client.health()
    assert not anywhere.called


async def test_a_client_without_credentials_sends_as_before() -> None:
    with respx.mock(assert_all_called=False) as mock:
        other = mock.get("https://other.example/x").respond(200, json={"ok": True})
        async with JamjetClient(BASE) as client:
            assert await client.get("https://other.example/x") == {"ok": True}
    assert "authorization" not in other.calls.last.request.headers


# ── redirects ────────────────────────────────────────────────────────────────


@pytest.mark.parametrize("credential", ["jamjet-token", "caller-auth"])
async def test_a_redirect_is_not_followed(credential: str) -> None:
    kwargs: dict[str, Any] = {"api_token": TOKEN} if credential == "jamjet-token" else {"auth": HeaderAuth()}
    with respx.mock(assert_all_called=False) as mock:
        mock.get(f"{BASE}/executions/exec_1").respond(
            307, headers={"Location": "https://other.example/executions/exec_1"}
        )
        other = mock.route(host="other.example").respond(200, json={})
        async with JamjetClient(BASE, **kwargs) as client:
            with pytest.raises(JamjetAPIError) as exc:
                await client.get_execution("exec_1")
            assert exc.value.response.status_code == 307
            assert "redirects are not followed" in str(exc.value)
            with pytest.raises(TypeError, match="follow_redirects"):
                await client.get("/executions/exec_1", follow_redirects=True)
            # `jamjet workers` uses the httpx client directly.
            raw = await client._client.get("/executions/exec_1")
            assert raw.status_code == 307
    assert not other.called


@pytest.mark.parametrize(
    ("method", "kwargs"),
    [
        ("get", {"auth": HeaderAuth()}),
        ("post", {"auth": HeaderAuth()}),
        ("get", {"follow_redirects": True}),
        ("post", {"follow_redirects": False}),
    ],
    ids=["get-auth", "post-auth", "get-follow", "post-follow"],
)
async def test_a_call_cannot_bring_its_own_auth_or_redirect_rule(method: str, kwargs: dict[str, Any]) -> None:
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        async with JamjetClient(BASE, api_token=TOKEN) as client:
            with pytest.raises(TypeError):
                await getattr(client, method)("/custom", **kwargs)
    assert not anywhere.called


# ── platform refusals ────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    ("headers", "body"),
    [
        ({"Content-Type": "text/html"}, b"<html><body><h1>401 Unauthorized</h1></body></html>"),
        ({"Content-Type": "application/json"}, b'{"error": {"code": 401, "status": "UNAUTHENTICATED"}}'),
        ({"Content-Type": "text/plain"}, b'{"error": "looks like the engine"}'),
        ({}, b""),
    ],
    ids=["html-page", "json-error-object", "json-text-as-plain", "empty"],
)
async def test_a_platform_401_page_is_platform_auth_error(headers: dict[str, str], body: bytes) -> None:
    with respx.mock(assert_all_called=False) as mock:
        mock.get(f"{BASE}/executions/exec_1").respond(401, headers=headers, content=body)
        async with JamjetClient(BASE, api_token=TOKEN) as client:
            with pytest.raises(PlatformAuthError) as exc:
                await client.get_execution("exec_1")
    assert isinstance(exc.value, JamjetAPIError)
    assert isinstance(exc.value, httpx.HTTPStatusError)
    assert exc.value.response.status_code == 401
    assert "in front of the engine" in str(exc.value)


@pytest.mark.parametrize(
    ("status", "content_type", "error"),
    [
        (401, "application/json", "invalid or expired token"),
        (401, "application/json; charset=utf-8", "missing Authorization: Bearer <token>"),
        (403, "application/json", "token is not in tenant default"),
    ],
)
async def test_an_engine_401_is_api_error(status: int, content_type: str, error: str) -> None:
    with respx.mock(assert_all_called=False) as mock:
        mock.get(f"{BASE}/executions/exec_1").respond(
            status, headers={"Content-Type": content_type}, content=json.dumps({"error": error}).encode()
        )
        async with JamjetClient(BASE, api_token=TOKEN) as client:
            with pytest.raises(JamjetAPIError) as exc:
                await client.get_execution("exec_1")
    assert not isinstance(exc.value, PlatformAuthError)
    assert exc.value.detail == error


async def test_a_platform_403_is_never_called_separation_of_duties() -> None:
    page = b"<html><body>Error: Forbidden. Your client does not have permission to get URL.</body></html>"
    engine_refusal = "separation of duties: the principal that started this run cannot approve it"
    with respx.mock(assert_all_called=False) as mock:
        approve = mock.post(f"{BASE}/executions/exec_1/approve")
        approve.side_effect = [
            httpx.Response(403, headers={"Content-Type": "text/html"}, content=page),
            httpx.Response(403, json={"error": engine_refusal}),
        ]
        async with JamjetClient(BASE, api_token=TOKEN) as client:
            with pytest.raises(PlatformAuthError) as platform:
                await client.approve("exec_1", "approved", action_digest="d1")
            with pytest.raises(JamjetAPIError) as engine:
                await client.approve("exec_1", "approved", action_digest="d1")
    assert "separation of duties" not in str(platform.value).lower()
    assert "separation of duties" not in platform.value.detail.lower()
    assert not isinstance(engine.value, PlatformAuthError)
    assert engine.value.detail == engine_refusal


@pytest.mark.parametrize(
    ("content_type", "body", "expected"),
    [
        ("application/json", b'{"error": "not found"}', True),
        ("Application/JSON; charset=utf-8", b'{"error": "not found"}', True),
        ("application/json", b'{"error": {"message": "denied"}}', False),
        ("application/json", b'["error"]', False),
        ("application/json", b"<html></html>", False),
        ("text/html", b'{"error": "not found"}', False),
    ],
)
def test_is_engine_error_body(content_type: str, body: bytes, expected: bool) -> None:
    response = httpx.Response(401, headers={"Content-Type": content_type}, content=body)
    assert is_engine_error_body(response) is expected


# ── error text ───────────────────────────────────────────────────────────────

# Written as escapes on purpose: the raw characters are invisible in an editor.
BIDI_CONTROLS = [
    "\u200e",
    "\u200f",
    "\u202a",
    "\u202b",
    "\u202c",
    "\u202d",
    "\u202e",
    "\u2066",
    "\u2067",
    "\u2068",
    "\u2069",
]


@pytest.mark.parametrize("char", BIDI_CONTROLS, ids=[f"U+{ord(c):04X}" for c in BIDI_CONTROLS])
async def test_a_bidi_control_in_an_error_is_shown_escaped(char: str) -> None:
    """A server's error text could carry a bidi override that reorders what the
    terminal shows. The detail shows it as an escape instead."""
    with respx.mock(assert_all_called=False) as mock:
        mock.get(f"{BASE}/executions/exec_1").respond(400, json={"error": f"no file 'report{char}fdp.exe'"})
        async with JamjetClient(BASE, api_token=TOKEN) as client:
            with pytest.raises(JamjetAPIError) as exc:
                await client.get_execution("exec_1")
    escaped = f"\\u{ord(char):04x}"
    assert exc.value.detail == f"no file 'report{escaped}fdp.exe'"
    assert char not in str(exc.value)


# ── two credentials, one header ──────────────────────────────────────────────


@pytest.mark.parametrize(
    "case",
    ["auth-writes-authorization", "auth-writes-lowercase", "request-headers", "auth-writes-it-on-resend"],
)
async def test_two_credentials_on_one_header_are_refused(case: str) -> None:
    auth: httpx.Auth | None = None
    headers: dict[str, str] | None = None
    if case == "auth-writes-authorization":
        auth = HeaderAuth(name="Authorization")
    elif case == "auth-writes-lowercase":
        auth = HeaderAuth(name="authorization")
    elif case == "request-headers":
        headers = {"Authorization": "Basic b3RoZXI6Y3JlZA=="}
    else:
        auth = WritesAuthorizationOnResend()
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/custom").respond(401, headers={"Content-Type": "text/html"}, content=b"<html/>")
        async with JamjetClient(BASE, api_token=TOKEN, auth=auth) as client:
            with pytest.raises(ValueError, match="Authorization"):
                await client.post("/custom", json={}, headers=headers)
    if case == "auth-writes-it-on-resend":
        # The first send carried the hook's header and the JamJet token; the
        # resend, with the hook's value in Authorization, never left.
        assert route.call_count == 1
        assert route.calls.last.request.headers["authorization"] == f"Bearer {TOKEN}"
    else:
        assert not route.called


async def test_a_caller_auth_without_a_jamjet_token_may_use_authorization() -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health").respond(200, json={"status": "ok"})
        async with JamjetClient(BASE, auth=HeaderAuth(name="Authorization", value="Bearer platform")) as client:
            await client.health()
    assert route.calls.last.request.headers["authorization"] == "Bearer platform"


async def test_a_caller_auth_that_resends_keeps_both_credentials_apart() -> None:
    auth = RefreshOn401()
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{BASE}/health")
        route.side_effect = [
            httpx.Response(401, headers={"Content-Type": "text/html"}, content=b"<html/>"),
            httpx.Response(200, json={"status": "ok"}),
        ]
        async with JamjetClient(BASE, api_token=TOKEN, auth=auth) as client:
            assert await client.health() == {"status": "ok"}
    sent = [call.request.headers for call in route.calls]
    assert [h["x-serverless-authorization"] for h in sent] == ["Bearer id-1", "Bearer id-2"]
    assert [h["authorization"] for h in sent] == [f"Bearer {TOKEN}", f"Bearer {TOKEN}"]


# ── no credential text anywhere ──────────────────────────────────────────────

TOKEN_SENTINEL = "jj-sentinel-9c41e7b2d0"
ID_SENTINEL = "id-sentinel-53fa08e1c7"


class _Engine(BaseHTTPRequestHandler):
    """A loopback server, so httpx and httpcore log as they do for real."""

    def _send(self, status: int, content_type: str, body: bytes) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/health":
            self._send(200, "application/json", b'{"status": "ok"}')
        elif self.path == "/platform":
            self._send(401, "text/html", b"<html><body>Unauthorized</body></html>")
        elif self.path == "/engine":
            self._send(401, "application/json", b'{"error": "invalid or expired token"}')
        elif self.path == "/echo":
            said = f"{self.headers.get('Authorization')} {self.headers.get('X-Serverless-Authorization')}"
            self._send(400, "application/json", json.dumps({"error": f"bad credentials: {said}"}).encode())
        else:
            self._send(404, "application/json", b'{"error": "not found"}')

    def log_message(self, format: str, *args: Any) -> None:
        pass


@pytest.fixture
def loopback() -> Iterator[str]:
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Engine)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(5)


async def test_no_credential_text_in_errors_repr_or_logs(loopback: str, caplog: pytest.LogCaptureFixture) -> None:
    # Some libraries (litellm) set the httpx logger to WARNING on import.
    for name in (None, "httpx", "httpcore"):
        caplog.set_level(logging.DEBUG, logger=name)
    client = JamjetClient(loopback, api_token=TOKEN_SENTINEL, auth=HeaderAuth(value=f"Bearer {ID_SENTINEL}"))
    texts = [repr(client), str(client)]
    errors: list[BaseException] = []
    async with client:
        assert await client.health() == {"status": "ok"}
        for path, kind in [("/platform", PlatformAuthError), ("/engine", JamjetAPIError), ("/echo", JamjetAPIError)]:
            with pytest.raises(kind) as exc:
                await client.get(path)
            errors.append(exc.value)
        port = loopback.rsplit(":", 1)[1]
        with pytest.raises(CredentialScopeError) as scope:
            await client.get(f"http://localhost:{port}/health")
        errors.append(scope.value)
        with pytest.raises(ValueError) as clash:
            await client.get("/health", headers={"Authorization": f"Bearer {TOKEN_SENTINEL}-other"})
        errors.append(clash.value)
    for error in errors:
        texts += [str(error), repr(error), *(str(a) for a in error.args), getattr(error, "detail", "")]
        texts += [str(error.__cause__), repr(error.__cause__)]
    texts.append(caplog.text)

    # The test can fail: the server did echo both credentials, and httpx logged.
    echo = errors[2]
    assert isinstance(echo, JamjetAPIError)
    assert TOKEN_SENTINEL in echo.response.text and ID_SENTINEL in echo.response.text
    assert "HTTP Request: GET" in caplog.text
    assert "send_request_headers" in caplog.text
    for text in texts:
        assert TOKEN_SENTINEL not in text
        assert ID_SENTINEL not in text


# ── a credential that cannot go in a header ──────────────────────────────────

UNSENDABLE = {
    "line-break-at-the-end": f"{TOKEN_SENTINEL}\n",
    "crlf-and-a-header": f"{TOKEN_SENTINEL}\r\nX-Injected: 1",
    "nul": f"{TOKEN_SENTINEL}\x00",
    "tab-inside": f"jj\t{TOKEN_SENTINEL}",
    "delete": f"{TOKEN_SENTINEL}\x7f",
    "space-at-the-start": f" {TOKEN_SENTINEL}",
    "space-at-the-end": f"{TOKEN_SENTINEL} ",
    "not-ascii": f"{TOKEN_SENTINEL}é",
}
UNSENDABLE_TOKEN = (
    "The JamJet token cannot go in a header as it is: it has a line break, another control character or a "
    "character that is not ASCII, or a space at one end. A token read from a file often ends with a line break. "
    "The token is not shown."
)


@pytest.fixture
def attempts(monkeypatch: pytest.MonkeyPatch) -> list[httpx.Request]:
    """Every request that reached the real transport, h11 included."""
    seen: list[httpx.Request] = []
    real = httpx.AsyncHTTPTransport.handle_async_request

    async def counting(self: httpx.AsyncHTTPTransport, request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return await real(self, request)

    monkeypatch.setattr(httpx.AsyncHTTPTransport, "handle_async_request", counting)
    return seen


@pytest.fixture
def no_waits(monkeypatch: pytest.MonkeyPatch) -> None:
    async def no_sleep(_: float) -> None:
        return None

    monkeypatch.setattr(transport, "_sleep", no_sleep)


def _every_text(error: BaseException) -> str:
    """The traceback of ``error`` with its whole chain, and the repr of each exception in it."""
    parts = ["".join(traceback.format_exception(error))]
    seen: set[int] = set()
    todo: list[BaseException] = [error]
    while todo:
        current = todo.pop()
        if id(current) in seen:
            continue
        seen.add(id(current))
        parts += [str(current), repr(current)]
        todo += [e for e in (current.__cause__, current.__context__, *current.args) if isinstance(e, BaseException)]
    return "\n".join(parts)


@pytest.mark.parametrize("token", list(UNSENDABLE.values()), ids=list(UNSENDABLE))
async def test_a_token_that_cannot_go_in_a_header_is_refused_and_not_shown(
    token: str, loopback: str, attempts: list[httpx.Request], no_waits: None
) -> None:
    # Against a real server, so h11 checks the header as it would in production.
    with pytest.raises(ValueError) as exc:
        async with JamjetClient(loopback, api_token=token) as client:
            await client.health()
    assert str(exc.value) == UNSENDABLE_TOKEN
    assert attempts == []
    assert TOKEN_SENTINEL not in _every_text(exc.value)


def test_a_jamjet_token_env_ending_in_a_line_break_is_refused(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("JAMJET_TOKEN", f"{TOKEN_SENTINEL}\n")
    with pytest.raises(ValueError) as exc:
        JamjetClient(BASE)
    assert str(exc.value) == UNSENDABLE_TOKEN


async def test_a_hook_value_that_cannot_go_in_a_header_is_refused_before_anything_is_sent(
    loopback: str, attempts: list[httpx.Request], no_waits: None, caplog: pytest.LogCaptureFixture
) -> None:
    for name in (None, "httpx", "httpcore"):
        caplog.set_level(logging.DEBUG, logger=name)
    auth = HeaderAuth(value=f"Bearer {ID_SENTINEL}\n")
    async with JamjetClient(loopback, api_token=TOKEN, auth=auth) as client:
        with pytest.raises(ValueError) as exc:
            await client.health()
    assert str(exc.value) == (
        "The auth hook put a value in x-serverless-authorization that cannot go in a header: it has a line break "
        "or another control character, or a space at one end. The value is not shown. The request was not sent."
    )
    assert attempts == []
    assert ID_SENTINEL not in _every_text(exc.value)
    assert ID_SENTINEL not in caplog.text


async def test_a_request_that_h11_refuses_is_raised_at_once_and_is_not_outcome_unknown(
    loopback: str, attempts: list[httpx.Request], no_waits: None
) -> None:
    # A header of the caller's own that h11 will not write: nothing left, and
    # a second send would fail the same way.
    bad = {"X-Trace": "a\nb"}
    async with JamjetClient(loopback, api_token=TOKEN) as client:
        with pytest.raises(httpx.LocalProtocolError):
            await client.get("/health", headers=bad)
        assert len(attempts) == 1
        with pytest.raises(httpx.LocalProtocolError) as unsafe:
            await client.post("/custom/route", json={}, headers=bad)
    assert len(attempts) == 2
    assert not isinstance(unsafe.value, OutcomeUnknown)


@pytest.mark.parametrize("call", ["health", "start_execution"], ids=["safe", "unsafe"])
async def test_a_transport_error_that_quotes_a_credential_is_redacted_with_its_chain(
    call: str, no_waits: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    sends: list[httpx.Request] = []

    async def quoting(self: httpx.AsyncHTTPTransport, request: httpx.Request) -> httpx.Response:
        # Shaped as httpcore and h11 shape theirs: the lowest error quotes the
        # value as bytes and is held only as the middle one's argument.
        sends.append(request)
        said = f"{request.headers['authorization']} {request.headers['x-serverless-authorization']}"
        low = RuntimeError(f"the peer quoted {said.encode()!r}")
        raise httpx.ReadError(f"the connection was lost after {said}") from httpcore.ReadError(low)

    monkeypatch.setattr(httpx.AsyncHTTPTransport, "handle_async_request", quoting)
    # A backslash, so the quoted form differs from the value as it is.
    auth = HeaderAuth(value=f"Bearer {ID_SENTINEL}\\x")
    async with JamjetClient(BASE, api_token=TOKEN_SENTINEL, auth=auth) as client:
        with pytest.raises((httpx.ReadError, OutcomeUnknown)) as exc:
            await (client.health() if call == "health" else client.start_execution("w", {}))
    assert len(sends) == (3 if call == "health" else 1)
    text = _every_text(exc.value)
    # The test can fail: each level did quote both credentials.
    assert "the connection was lost after [redacted] [redacted]" in text
    assert "the peer quoted b'[redacted] [redacted]'" in text
    assert TOKEN_SENTINEL not in text
    assert ID_SENTINEL not in text


# ── every call keeps its method, path and body (refactor guard) ──────────────

_HASH = "ab" * 32


@dataclass(frozen=True)
class Call:
    name: str
    call: Callable[[JamjetClient], Awaitable[Any]]
    method: str
    path: str
    body: Any = None  # parsed JSON, raw bytes, or None for no body
    params: dict[str, str] = field(default_factory=dict)
    headers: dict[str, str] = field(default_factory=dict)


CALLS = [
    Call("health", lambda c: c.health(), "GET", "/health"),
    Call(
        "create_workflow",
        lambda c: c.create_workflow({"workflow_id": "w"}),
        "POST",
        "/workflows",
        {"ir": {"workflow_id": "w"}},
    ),
    Call(
        "create_cron_job",
        lambda c: c.create_cron_job("nightly", "0 3 * * *", "w", input={"a": 1}),
        "POST",
        "/cron",
        {
            "name": "nightly",
            "cron_expression": "0 3 * * *",
            "workflow_id": "w",
            "enabled": True,
            "workflow_version": "1.0.0",
            "input": {"a": 1},
        },
    ),
    Call("list_cron_jobs", lambda c: c.list_cron_jobs(), "GET", "/cron"),
    Call(
        "start_execution",
        lambda c: c.start_execution("w", {"q": "hi"}, workflow_version="2.0.0"),
        "POST",
        "/executions",
        {"workflow_id": "w", "input": {"q": "hi"}, "workflow_version": "2.0.0"},
    ),
    Call("get_execution", lambda c: c.get_execution("exec_1"), "GET", "/executions/exec_1"),
    Call(
        "list_executions",
        lambda c: c.list_executions(status="running", limit=5, offset=10),
        "GET",
        "/executions",
        params={"limit": "5", "offset": "10", "status": "running"},
    ),
    Call("cancel_execution", lambda c: c.cancel_execution("exec_1"), "POST", "/executions/exec_1/cancel"),
    Call(
        "get_events",
        lambda c: c.get_events("exec_1", inline_artifacts="all"),
        "GET",
        "/executions/exec_1/events",
        params={"inline_artifacts": "all"},
    ),
    Call(
        "approve",
        lambda c: c.approve(
            "exec_1", "approved", node_id="gate", comment="ok", action_digest="d1", user_id="ana", request_sequence=7
        ),
        "POST",
        "/executions/exec_1/approve",
        {
            "decision": "approved",
            "node_id": "gate",
            "comment": "ok",
            "action_digest": "d1",
            "user_id": "ana",
            "request_sequence": 7,
        },
    ),
    Call("list_approvals", lambda c: c.list_approvals("exec_1"), "GET", "/executions/exec_1/approvals"),
    Call(
        "list_pending_approvals",
        lambda c: c.list_pending_approvals(limit=25, after="ab12"),
        "GET",
        "/approvals",
        params={"limit": "25", "after": "ab12"},
    ),
    Call(
        "send_external_event",
        lambda c: c.send_external_event("exec_1", "order-7", {"paid": True}),
        "POST",
        "/executions/exec_1/external-event",
        {"correlation_key": "order-7", "payload": {"paid": True}},
    ),
    Call(
        "put_artifact",
        lambda c: c.put_artifact(b"hello", media_type="text/plain"),
        "POST",
        "/artifacts",
        b"hello",
        headers={"content-type": "text/plain"},
    ),
    Call("get_artifact", lambda c: c.get_artifact(_HASH), "GET", f"/artifacts/{_HASH}"),
    Call("register_agent", lambda c: c.register_agent({"name": "a"}), "POST", "/agents", {"name": "a"}),
    Call("list_agents", lambda c: c.list_agents(), "GET", "/agents"),
    Call("get_agent", lambda c: c.get_agent("ag-1"), "GET", "/agents/ag-1"),
    Call("activate_agent", lambda c: c.activate_agent("ag-1"), "POST", "/agents/ag-1/activate"),
    Call("deactivate_agent", lambda c: c.deactivate_agent("ag-1"), "POST", "/agents/ag-1/deactivate"),
    Call(
        "discover_agent",
        lambda c: c.discover_agent("https://agents.example/card"),
        "POST",
        "/agents/discover",
        {"url": "https://agents.example/card"},
    ),
    Call(
        "claim_work_item",
        lambda c: c.claim_work_item("wk-1", ["python_tool"], lease_secs=60),
        "POST",
        "/work-items/claim",
        {"worker_id": "wk-1", "queue_types": ["python_tool"], "lease_secs": 60},
    ),
    Call(
        "complete_work_item",
        lambda c: c.complete_work_item(
            "wi-1",
            "exec_1",
            "n1",
            {"r": 1},
            {"s": 2},
            duration_ms=7,
            gen_ai_model="m",
            finish_reason="stop",
            lease_fence=3,
            idempotency_key="k1",
            input_tokens=10,
            output_tokens=20,
            cost_usd=0.5,
        ),
        "POST",
        "/work-items/wi-1/complete",
        {
            "output": {"r": 1},
            "state_patch": {"s": 2},
            "duration_ms": 7,
            "execution_id": "exec_1",
            "node_id": "n1",
            "gen_ai_model": "m",
            "finish_reason": "stop",
            "input_tokens": 10,
            "output_tokens": 20,
            "cost_usd": 0.5,
            "lease_fence": 3,
            "idempotency_key": "k1",
        },
    ),
    Call(
        "complete_model_step",
        lambda c: c.complete_model_step(
            "wi-1",
            {"content": "hi", "finish_reason": "stop", "model": "m", "input_tokens": 1, "output_tokens": 2},
            lease_fence=3,
            idempotency_key="k1",
            duration_ms=7,
        ),
        "POST",
        "/work-items/wi-1/complete",
        {
            "lease_fence": 3,
            "duration_ms": 7,
            "model_result": {
                "content": "hi",
                "finish_reason": "stop",
                "model": "m",
                "input_tokens": 1,
                "output_tokens": 2,
            },
            "idempotency_key": "k1",
        },
    ),
    Call(
        "fail_work_item",
        lambda c: c.fail_work_item("wi-1", "boom", lease_fence=3),
        "POST",
        "/work-items/wi-1/fail",
        {"error": "boom", "lease_fence": 3},
    ),
    Call(
        "heartbeat_work_item",
        lambda c: c.heartbeat_work_item("wi-1", "wk-1", lease_fence=3),
        "POST",
        "/work-items/wi-1/heartbeat",
        {"worker_id": "wk-1", "lease_fence": 3},
    ),
    Call("tick", lambda c: c.tick(budget_ms=5000), "POST", "/ops/tick", {"budget_ms": 5000}),
    Call("engine_features", lambda c: c.engine_features(), "GET", "/health"),
    Call("require_features", lambda c: c.require_features(), "GET", "/health"),
    Call(
        "find_execution",
        lambda c: c.find_execution("job:1"),
        "GET",
        "/executions",
        params={"run_key": "job:1"},
    ),
    Call("post", lambda c: c.post("/custom/route", json={"k": "v"}), "POST", "/custom/route", {"k": "v"}),
    Call(
        "get",
        lambda c: c.get("/custom/route", params={"page": 2}),
        "GET",
        "/custom/route",
        params={"page": "2"},
    ),
]


def _answer(request: httpx.Request) -> httpx.Response:
    path = request.url.path
    if path == "/work-items/claim":
        return httpx.Response(200, json={"claimed": True, "work_item": {"id": "wi-1"}})
    if path == "/artifacts":
        return httpx.Response(200, json={"hash": _HASH, "size": 5, "media_type": "text/plain"})
    if path.startswith("/artifacts/"):
        return httpx.Response(200, content=b"hello")
    if path.endswith("/events"):
        return httpx.Response(200, json={"events": []})
    if path == "/approvals":
        return httpx.Response(200, json={"approvals": [], "next_after": None})
    if path == "/executions" and "run_key" in request.url.params:
        return httpx.Response(200, json={"run_key": request.url.params["run_key"], "executions": []})
    return httpx.Response(200, json={"ok": True})


def test_the_call_table_covers_every_method() -> None:
    methods = {
        name
        for name, member in inspect.getmembers(JamjetClient, inspect.iscoroutinefunction)
        if not name.startswith("_")
    }
    assert {c.name for c in CALLS} == methods


@pytest.mark.parametrize("call", CALLS, ids=[c.name for c in CALLS])
async def test_every_call_keeps_its_method_path_and_body(call: Call) -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.route().mock(side_effect=_answer)
        async with JamjetClient(BASE, api_token=TOKEN) as client:
            await call.call(client)
    assert route.call_count == 1
    sent = route.calls.last.request
    assert sent.method == call.method
    assert sent.url.host == "engine.test"
    assert sent.url.port == 7700
    assert sent.url.path == call.path
    assert dict(sent.url.params) == call.params
    if call.body is None:
        assert sent.content == b""
    elif isinstance(call.body, bytes):
        assert sent.content == call.body
    else:
        assert json.loads(sent.content) == call.body
    for name, value in call.headers.items():
        assert sent.headers[name] == value
    assert sent.headers["authorization"] == f"Bearer {TOKEN}"
