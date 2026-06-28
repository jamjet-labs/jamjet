"""Thin artifact store over the JamJet runtime CAS (Track 4 / T4-4).

``ArtifactStore`` wraps a :class:`~jamjet.client.JamjetClient` (or any object
exposing the same async ``put_artifact``/``get_artifact`` methods) and gives a
session a small store/fetch namespace surfaced as ``Session.artifacts``
(attach a client first — ``Session.artifacts`` fails loud without one):

    >>> session.attach_client(JamjetClient("http://127.0.0.1:7700"))
    >>> ref = await session.artifacts.put(b"report bytes", "text/plain")
    >>> data = await session.artifacts.get(ref.hash)

Keep it thin: the store adds no state of its own — artifacts are
content-addressed by the runtime and isolated per tenant by the server's
tenant-scoped backend.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Protocol

if TYPE_CHECKING:
    from jamjet.client import ArtifactRef


class _ArtifactClient(Protocol):
    """The subset of :class:`~jamjet.client.JamjetClient` an ArtifactStore needs."""

    async def put_artifact(self, data: bytes, media_type: str | None = ...) -> ArtifactRef: ...

    async def get_artifact(self, hash: str) -> bytes: ...


class ArtifactStore:
    """Store and fetch content-addressed artifacts over a JamjetClient.

    Args:
        client: A :class:`~jamjet.client.JamjetClient` (or compatible object)
                used for the underlying HTTP calls.
    """

    def __init__(self, client: _ArtifactClient) -> None:
        self._client = client

    async def put(self, data: bytes, media_type: str | None = None) -> ArtifactRef:
        """Store *data* and return its :class:`~jamjet.client.ArtifactRef`."""
        return await self._client.put_artifact(data, media_type)

    async def get(self, hash: str) -> bytes:
        """Fetch the bytes for a previously-stored artifact *hash*."""
        return await self._client.get_artifact(hash)
