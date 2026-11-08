"""``EngramClient`` reads ``ENGRAM_AUTH_TOKEN``, the variable the Engram
servers and the Java SDK use. It read only ``ENGRAM_TOKEN``, so a client set
up like its server sent no token and got 401s."""

from __future__ import annotations

import pytest

from jamjet.engram.client import EngramClient


async def _token(monkeypatch: pytest.MonkeyPatch, env: dict[str, str]) -> str | None:
    for key in ("ENGRAM_AUTH_TOKEN", "ENGRAM_TOKEN"):
        monkeypatch.delenv(key, raising=False)
    for key, value in env.items():
        monkeypatch.setenv(key, value)
    client = EngramClient()
    try:
        return client._token
    finally:
        await client._client.aclose()


async def test_reads_engram_auth_token(monkeypatch: pytest.MonkeyPatch) -> None:
    assert await _token(monkeypatch, {"ENGRAM_AUTH_TOKEN": "a"}) == "a"


async def test_still_reads_engram_token(monkeypatch: pytest.MonkeyPatch) -> None:
    assert await _token(monkeypatch, {"ENGRAM_TOKEN": "b"}) == "b"


async def test_engram_auth_token_wins(monkeypatch: pytest.MonkeyPatch) -> None:
    assert await _token(monkeypatch, {"ENGRAM_AUTH_TOKEN": "a", "ENGRAM_TOKEN": "b"}) == "a"


async def test_forget_stats_and_delete_user_send_the_scope() -> None:
    """The server scopes forget, stats and delete-user by org (and user); the
    client had no way to say which, so other orgs got 404s and wrong counts."""
    import httpx

    seen: list[tuple[str, str, dict[str, str]]] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append((request.method, request.url.path, dict(request.url.params)))
        return httpx.Response(200, json={})

    client = EngramClient(base_url="http://test")
    await client._client.aclose()
    client._client = httpx.AsyncClient(base_url="http://test", transport=httpx.MockTransport(handler))
    try:
        await client.forget("f1", org_id="acme", user_id="u1")
        await client.stats(org_id="acme")
        await client.delete_user("u1", org_id="acme")
        await client.stats()
    finally:
        await client._client.aclose()
    assert seen == [
        ("DELETE", "/v1/memory/facts/f1", {"org_id": "acme", "user_id": "u1"}),
        ("GET", "/v1/memory/stats", {"org_id": "acme"}),
        ("DELETE", "/v1/memory/users/u1", {"org_id": "acme"}),
        ("GET", "/v1/memory/stats", {}),
    ]
