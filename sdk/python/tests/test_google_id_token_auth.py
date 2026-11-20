"""The Google ID token helper.

No token is ever requested from Google here: ``fetch_id_token`` is replaced by
a fake for every test, and a test that reached the real one would fail.
"""

from __future__ import annotations

import asyncio
import base64
import json
import re
import subprocess
import sys
import threading
import tomllib
from pathlib import Path
from typing import Any

import google.auth.exceptions
import google.auth.transport.requests
import google.oauth2.id_token
import pytest
import respx

from jamjet.auth import GoogleIdTokenAuth, IdentityTokenError, token_source
from jamjet.client import JamjetClient
from jamjet.transport import CredentialScopeError

AUDIENCE = "https://engine.example"
JJ = "jj-test-token-9e04"
WALL = 1795168800  # 2026-11-20 10:00:00 UTC
_payload = base64.urlsafe_b64encode(json.dumps({"iat": WALL, "exp": WALL + 3600}).encode()).rstrip(b"=").decode()
ID_TOKEN = f"eyJhbGciOiJSUzI1NiJ9.{_payload}.Z29vZ2xl"


class FakeFetch:
    """Stands in for ``google.oauth2.id_token.fetch_id_token``."""

    def __init__(self, *outcomes: Any) -> None:
        self.outcomes = list(outcomes) or [ID_TOKEN]
        self.calls: list[tuple[Any, str, int]] = []

    def __call__(self, request: Any, audience: str) -> str:
        self.calls.append((request, audience, threading.get_ident()))
        outcome = self.outcomes[min(len(self.calls), len(self.outcomes)) - 1]
        if isinstance(outcome, BaseException):
            raise outcome
        return outcome


@pytest.fixture(autouse=True)
def _never_google(monkeypatch: pytest.MonkeyPatch) -> None:
    def refuse(*args: Any, **kwargs: Any) -> str:
        raise AssertionError("a test reached the real fetch_id_token")

    monkeypatch.setattr(google.oauth2.id_token, "fetch_id_token", refuse)
    monkeypatch.delenv("JAMJET_TOKEN", raising=False)


@pytest.fixture
def fake(monkeypatch: pytest.MonkeyPatch) -> FakeFetch:
    fetch = FakeFetch()
    monkeypatch.setattr(google.oauth2.id_token, "fetch_id_token", fetch)
    return fetch


@pytest.fixture(autouse=True)
def waits(monkeypatch: pytest.MonkeyPatch) -> list[float]:
    recorded: list[float] = []

    async def sleep(seconds: float) -> None:
        recorded.append(seconds)
        await asyncio.sleep(0)

    monkeypatch.setattr(token_source, "_sleep", sleep)
    monkeypatch.setattr(token_source, "_sleep_sync", recorded.append)
    return recorded


async def test_the_google_helper_rules(fake: FakeFetch, monkeypatch: pytest.MonkeyPatch) -> None:
    # The fake fetch_id_token receives the audience, through google-auth's own
    # transport, on a worker thread.
    with respx.mock(assert_all_called=False) as mock:
        route = mock.get(f"{AUDIENCE}/health").respond(200, json={"status": "ok"})
        async with JamjetClient(AUDIENCE, api_token=JJ, auth=GoogleIdTokenAuth(AUDIENCE)) as c:
            await c.health()
            await c.health()
    assert len(fake.calls) == 1
    request, audience, thread = fake.calls[0]
    assert audience == AUDIENCE
    assert isinstance(request, google.auth.transport.requests.Request)
    assert thread != threading.get_ident()
    sent = route.calls.last.request.headers
    assert sent.get_list("x-serverless-authorization") == [f"Bearer {ID_TOKEN}"]
    assert sent.get_list("authorization") == [f"Bearer {JJ}"]

    # https is required unless the host is loopback.
    for audience in ("http://engine.example", "http://engine.example:7700", "http://localhost.example"):
        with pytest.raises(ValueError, match="https"):
            GoogleIdTokenAuth(audience)
    with pytest.raises(ValueError, match="https"):
        GoogleIdTokenAuth(AUDIENCE, origin="http://engine.example")
    for audience in (AUDIENCE, "http://localhost:7700", "http://127.0.0.1:7700", "http://[::1]:7700"):
        GoogleIdTokenAuth(audience)

    # Without the extra, the error names it.
    with monkeypatch.context() as m:
        for name in ("google.auth.exceptions", "google.auth.transport.requests", "google.oauth2.id_token"):
            m.setitem(sys.modules, name, None)
        with pytest.raises(ImportError, match=re.escape("pip install 'jamjet[google-auth]'")):
            GoogleIdTokenAuth(AUDIENCE)


@pytest.mark.parametrize(
    ("error", "tries"),
    [
        (google.auth.exceptions.TransportError("metadata server unreachable"), 3),
        (google.auth.exceptions.RefreshError("the metadata server refused"), 1),
        (google.auth.exceptions.DefaultCredentialsError("no credentials found"), 1),
        # Both an OSError and a DefaultCredentialsError: a credentials problem.
        (google.auth.exceptions.OSError("credentials file unreadable"), 1),
    ],
    ids=["transport-error", "refresh-error", "default-credentials-error", "credentials-os-error"],
)
async def test_the_google_helper_tries_a_transport_error_again_and_a_credentials_error_never(
    monkeypatch: pytest.MonkeyPatch, waits: list[float], error: Exception, tries: int
) -> None:
    fetch = FakeFetch(error)
    monkeypatch.setattr(google.oauth2.id_token, "fetch_id_token", fetch)
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={})
        async with JamjetClient(AUDIENCE, api_token=JJ, auth=GoogleIdTokenAuth(AUDIENCE)) as c:
            with pytest.raises(IdentityTokenError) as exc:
                await c.health()
    assert len(fetch.calls) == tries
    assert waits == [0.2, 0.5][: tries - 1]
    assert exc.value.__cause__ is error
    assert not anywhere.called


async def test_the_origin_comes_from_the_audience_unless_given(fake: FakeFetch) -> None:
    assert GoogleIdTokenAuth("https://engine.example/").origin == "https://engine.example:443"
    assert GoogleIdTokenAuth("https://engine.example:8443").origin == "https://engine.example:8443"
    with pytest.raises(ValueError, match="origin"):
        GoogleIdTokenAuth("custom-audience")

    auth = GoogleIdTokenAuth("custom-audience", origin=AUDIENCE)
    assert auth.origin == "https://engine.example:443"
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json={"status": "ok"})
        async with JamjetClient(AUDIENCE, api_token=JJ, auth=auth) as c:
            await c.health()
            with pytest.raises(CredentialScopeError):
                await c.get("https://other.example/health")
    assert [audience for _, audience, _ in fake.calls] == ["custom-audience"]
    assert anywhere.call_count == 1


def test_the_extra_brings_requests() -> None:
    pyproject = tomllib.loads((Path(__file__).parents[1] / "pyproject.toml").read_text())
    assert pyproject["project"]["optional-dependencies"]["google-auth"] == ["google-auth[requests]>=2.0,<3"]


def test_jamjet_auth_imports_without_google_auth() -> None:
    code = (
        "import sys\n"
        "sys.modules['google'] = None\n"
        "import jamjet.auth, jamjet.client\n"
        "try:\n"
        "    jamjet.auth.GoogleIdTokenAuth('https://engine.example')\n"
        "except ImportError as exc:\n"
        "    print(exc)\n"
    )
    out = subprocess.run(
        [sys.executable, "-c", code], capture_output=True, text=True, timeout=120, cwd=Path(__file__).parents[1]
    )
    assert out.returncode == 0, out.stderr
    assert "pip install 'jamjet[google-auth]'" in out.stdout
