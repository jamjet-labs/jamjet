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
