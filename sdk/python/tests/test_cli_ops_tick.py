"""`client.tick()`, `client.engine_features()` and `jamjet ops tick`."""

from __future__ import annotations

import json
import socket
from typing import Any

import httpx
import pytest
import respx
from typer.testing import CliRunner

from jamjet import transport
from jamjet.cli.main import app
from jamjet.client import JamjetClient
from jamjet.transport import EngineFeatureMissing

BASE = "http://engine.test:7700"
TOKEN = "jj-test-token-77aa"
runner = CliRunner()

RAN = {"ran": True, "complete": True, "elapsed_ms": 40, "budget_ms": 5000}
PARTIAL = {"ran": True, "complete": False, "elapsed_ms": 5000, "budget_ms": 5000}
BUSY = {"ran": False, "reason": "busy", "held_by": "i-1", "lease_until": "2026-11-20T10:00:00Z"}


@pytest.fixture(autouse=True)
def _env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("JAMJET_TOKEN", TOKEN)


def _ops(*args: str) -> Any:
    return runner.invoke(app, ["ops", "tick", "--runtime", BASE, *args])


async def test_tick_posts_its_budget() -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/ops/tick").respond(200, json=RAN)
        async with JamjetClient(BASE) as client:
            assert await client.tick(budget_ms=5000) == RAN
    assert json.loads(route.calls.last.request.content) == {"budget_ms": 5000}
    assert route.calls.last.request.headers["authorization"] == f"Bearer {TOKEN}"


async def test_tick_without_a_budget_posts_an_empty_object() -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/ops/tick").respond(200, json=RAN)
        async with JamjetClient(BASE) as client:
            await client.tick()
    assert json.loads(route.calls.last.request.content) == {}


async def test_tick_against_an_engine_without_the_route_is_a_clear_error() -> None:
    with respx.mock(assert_all_called=False) as mock:
        mock.post(f"{BASE}/ops/tick").respond(404, json={"error": "not found"})
        async with JamjetClient(BASE) as client:
            with pytest.raises(EngineFeatureMissing) as exc:
                await client.tick()
    assert exc.value.feature == "ops_tick"
    assert "ops_tick" in str(exc.value)


def test_the_cli_exits_2_when_the_engine_has_no_tick_route() -> None:
    with respx.mock(assert_all_called=False) as mock:
        mock.post(f"{BASE}/ops/tick").respond(404, json={"error": "not found"})
        result = _ops()
    assert result.exit_code == 2
    assert "ops_tick" in result.output
    assert TOKEN not in result.output


async def test_engine_features_reads_health_once_and_is_empty_without_a_list() -> None:
    with respx.mock(assert_all_called=False) as mock:
        health = mock.get(f"{BASE}/health").respond(200, json={"status": "ok", "features": ["ops_tick", "run_key"]})
        async with JamjetClient(BASE) as client:
            assert await client.engine_features() == frozenset({"ops_tick", "run_key"})
            assert await client.engine_features() == frozenset({"ops_tick", "run_key"})
    assert health.call_count == 1


async def test_an_engine_that_lists_no_features_gives_an_empty_set() -> None:
    with respx.mock(assert_all_called=False) as mock:
        health = mock.get(f"{BASE}/health").respond(200, json={"status": "ok", "version": "0.12.0"})
        async with JamjetClient(BASE) as client:
            assert await client.engine_features() == frozenset()
            assert await client.engine_features() == frozenset()
    assert health.call_count == 1


def test_a_single_tick_prints_the_answer_and_exits_0() -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/ops/tick").respond(200, json=RAN)
        result = _ops("--budget-ms", "5000", "--json")
    assert result.exit_code == 0
    assert json.loads(result.output)["ran"] is True
    assert route.call_count == 1
    assert json.loads(route.calls.last.request.content) == {"budget_ms": 5000}


def test_a_busy_engine_is_not_an_error() -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/ops/tick").respond(200, json=BUSY)
        result = _ops("--until-complete")
    assert result.exit_code == 0
    assert "busy" in result.output
    assert route.call_count == 1


def test_until_complete_stops_when_the_engine_says_complete() -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/ops/tick").mock(
            side_effect=[httpx.Response(200, json=PARTIAL), httpx.Response(200, json=RAN)]
        )
        result = _ops("--until-complete", "--json")
    assert result.exit_code == 0
    assert route.call_count == 2
    assert json.loads(result.output)["complete"] is True


def test_until_complete_stops_at_max_calls() -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/ops/tick").respond(200, json=PARTIAL)
        result = _ops("--until-complete", "--max-calls", "3", "--json")
    assert result.exit_code == 0
    assert route.call_count == 3
    assert json.loads(result.output)["complete"] is False


def test_without_until_complete_one_call_is_made() -> None:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/ops/tick").respond(200, json=PARTIAL)
        result = _ops()
    assert result.exit_code == 0
    assert route.call_count == 1


def test_ops_tick_without_a_token_sends_nothing(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("JAMJET_TOKEN", raising=False)
    with respx.mock(assert_all_called=False) as mock:
        anywhere = mock.route().respond(200, json=RAN)
        result = _ops()
    assert result.exit_code == 1
    assert "JAMJET_TOKEN" in result.output
    assert not anywhere.called


def test_an_engine_error_exits_1_and_shows_no_token() -> None:
    with respx.mock(assert_all_called=False) as mock:
        mock.post(f"{BASE}/ops/tick").respond(403, json={"error": "operator role required"})
        result = _ops()
    assert result.exit_code == 1
    assert "operator role required" in result.output
    assert TOKEN not in result.output


def test_a_token_ending_in_a_line_break_is_refused_and_not_printed(monkeypatch: pytest.MonkeyPatch) -> None:
    # A token read from a file or a mounted secret often ends in a line break.
    secret = "operator-secret-4c19e2"
    monkeypatch.setenv("JAMJET_TOKEN", f"{secret}\n")

    async def no_sleep(_: float) -> None:
        return None

    monkeypatch.setattr(transport, "_sleep", no_sleep)
    attempts: list[httpx.Request] = []
    real = httpx.AsyncHTTPTransport.handle_async_request

    async def counting(self: httpx.AsyncHTTPTransport, request: httpx.Request) -> httpx.Response:
        attempts.append(request)
        return await real(self, request)

    monkeypatch.setattr(httpx.AsyncHTTPTransport, "handle_async_request", counting)
    # A real listening socket, so h11 sees the header as it would in production.
    with socket.socket() as engine:
        engine.bind(("127.0.0.1", 0))
        engine.listen(8)
        result = runner.invoke(app, ["ops", "tick", "--runtime", f"http://127.0.0.1:{engine.getsockname()[1]}"])
    assert secret not in result.output
    assert result.exit_code == 1
    assert "line break" in result.output
    assert attempts == []


def test_a_lost_connection_exits_1() -> None:
    with respx.mock(assert_all_called=False) as mock:
        mock.post(f"{BASE}/ops/tick").mock(side_effect=httpx.ConnectError("refused"))
        result = _ops()
    assert result.exit_code == 1


def test_the_google_audience_puts_the_identity_token_in_its_own_header(monkeypatch: pytest.MonkeyPatch) -> None:
    pytest.importorskip("google.auth")
    import jamjet.auth.google as google_mod

    class FakeAuth(httpx.Auth):
        def __init__(self, audience: str, **_: Any) -> None:
            self.audience = audience

        def auth_flow(self, request: httpx.Request) -> Any:
            request.headers["X-Serverless-Authorization"] = "Bearer id-fake"
            yield request

    seen: list[str] = []

    def factory(audience: str, **kwargs: Any) -> FakeAuth:
        seen.append(audience)
        return FakeAuth(audience, **kwargs)

    monkeypatch.setattr(google_mod, "GoogleIdTokenAuth", factory)
    monkeypatch.setattr("jamjet.auth.GoogleIdTokenAuth", factory)
    with respx.mock(assert_all_called=False) as mock:
        route = mock.post(f"{BASE}/ops/tick").respond(200, json=RAN)
        result = _ops("--google-id-token-audience", BASE)
    assert result.exit_code == 0
    assert seen == [BASE]
    sent = route.calls.last.request.headers
    assert sent["x-serverless-authorization"] == "Bearer id-fake"
    assert sent["authorization"] == f"Bearer {TOKEN}"
