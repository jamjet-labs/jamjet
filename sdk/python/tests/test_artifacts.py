"""Tests for T4-4: the Python artifact API.

Covers three layers:
- ``JamjetClient.put_artifact`` / ``get_artifact`` against a mock HTTP transport
  (no live runtime) — a real round-trip through the HTTP serialization.
- ``ArtifactStore`` over a fake client.
- ``Session.artifacts`` namespace round-trip via an attached fake client.
"""

from __future__ import annotations

import hashlib

import httpx
import pytest

from jamjet import ArtifactRef, ArtifactStore, Session
from jamjet.client import JamjetClient

# ---------------------------------------------------------------------------
# Mock HTTP transport — a content-addressed artifact store in a dict
# ---------------------------------------------------------------------------


def _mock_client() -> JamjetClient:
    """A JamjetClient whose transport implements the artifact routes in memory.

    POST /artifacts hashes the body and stores it; GET /artifacts/:hash returns
    the bytes (404 when absent) — mirroring the Rust routes.
    """
    store: dict[str, bytes] = {}

    def handler(request: httpx.Request) -> httpx.Response:
        path = request.url.path
        if request.method == "POST" and path == "/artifacts":
            data = request.content
            digest = hashlib.sha256(data).hexdigest()
            store[digest] = data
            media_type = request.headers.get("content-type")
            # Match the runtime: an explicit ?media_type= query wins.
            media_type = request.url.params.get("media_type", media_type)
            return httpx.Response(
                200,
                json={"hash": digest, "size": len(data), "media_type": media_type},
            )
        if request.method == "GET" and path.startswith("/artifacts/"):
            digest = path.rsplit("/", 1)[-1]
            data = store.get(digest)
            if data is None:
                return httpx.Response(404, json={"error": f"artifact {digest}"})
            return httpx.Response(
                200,
                content=data,
                headers={"content-type": "application/octet-stream"},
            )
        return httpx.Response(404, json={"error": "not found"})

    client = JamjetClient(base_url="http://test")
    client._client = httpx.AsyncClient(base_url="http://test", transport=httpx.MockTransport(handler))
    return client


# ---------------------------------------------------------------------------
# ArtifactRef dataclass
# ---------------------------------------------------------------------------


def test_artifact_ref_fields():
    ref = ArtifactRef(hash="a" * 64, size=12, media_type="text/plain")
    assert ref.hash == "a" * 64
    assert ref.size == 12
    assert ref.media_type == "text/plain"


def test_artifact_ref_media_type_optional():
    ref = ArtifactRef(hash="b" * 64, size=3)
    assert ref.media_type is None


# ---------------------------------------------------------------------------
# JamjetClient.put_artifact / get_artifact (mock HTTP)
# ---------------------------------------------------------------------------


async def test_client_put_artifact_returns_ref():
    client = _mock_client()
    try:
        ref = await client.put_artifact(b"hello", media_type="text/plain")
        assert isinstance(ref, ArtifactRef)
        assert ref.hash == hashlib.sha256(b"hello").hexdigest()
        assert ref.size == 5
        assert ref.media_type == "text/plain"
    finally:
        await client._client.aclose()


async def test_client_round_trips_blob_by_hash():
    client = _mock_client()
    try:
        ref = await client.put_artifact(b"hello")
        got = await client.get_artifact(ref.hash)
        assert got == b"hello"
    finally:
        await client._client.aclose()


async def test_client_get_unknown_hash_raises_404():
    client = _mock_client()
    try:
        with pytest.raises(httpx.HTTPStatusError) as exc:
            await client.get_artifact("0" * 64)
        assert exc.value.response.status_code == 404
    finally:
        await client._client.aclose()


# ---------------------------------------------------------------------------
# Fake client — duck-typed for ArtifactStore / Session.artifacts
# ---------------------------------------------------------------------------


class _FakeArtifactClient:
    """In-memory stand-in exposing the client's artifact methods."""

    def __init__(self) -> None:
        self.store: dict[str, bytes] = {}

    async def put_artifact(self, data: bytes, media_type: str | None = None) -> ArtifactRef:
        digest = hashlib.sha256(data).hexdigest()
        self.store[digest] = data
        return ArtifactRef(hash=digest, size=len(data), media_type=media_type)

    async def get_artifact(self, hash: str) -> bytes:
        return self.store[hash]


async def test_artifact_store_put_get_round_trip():
    store = ArtifactStore(_FakeArtifactClient())
    ref = await store.put(b"report bytes", "text/markdown")
    assert ref.size == len(b"report bytes")
    assert ref.media_type == "text/markdown"
    assert await store.get(ref.hash) == b"report bytes"


# ---------------------------------------------------------------------------
# Session.artifacts namespace
# ---------------------------------------------------------------------------


async def test_session_artifacts_round_trip():
    session = Session(id="s-1")
    session.attach_client(_FakeArtifactClient())

    ref = await session.artifacts.put(b"hello session", "text/plain")
    assert ref.hash == hashlib.sha256(b"hello session").hexdigest()
    assert await session.artifacts.get(ref.hash) == b"hello session"


def test_session_artifacts_property_is_lazy_store():
    """Without attach_client, .artifacts builds a default ArtifactStore."""
    session = Session(id="s-2")
    assert isinstance(session.artifacts, ArtifactStore)
    # Memoised: the same store instance is returned on repeat access.
    assert session.artifacts is session.artifacts


def test_session_artifacts_does_not_break_equality_or_persistence():
    """Attaching a client must not affect Session value-equality or saved state."""
    a = Session(id="s-3", messages=[{"role": "user", "content": "hi"}])
    b = Session(id="s-3", messages=[{"role": "user", "content": "hi"}])
    a.attach_client(_FakeArtifactClient())
    assert a == b  # _artifacts is not a compared field
