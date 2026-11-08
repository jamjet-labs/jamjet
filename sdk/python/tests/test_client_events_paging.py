"""``JamjetClient.get_events`` follows the runtime's ``next_after`` cursor.

The runtime returns at most 10000 events per page. A client that read one page
would show a long run's timeline cut short.
"""

from __future__ import annotations

import httpx

from jamjet.client import JamjetClient


async def _client(handler) -> JamjetClient:
    client = JamjetClient(base_url="http://test")
    await client._client.aclose()
    client._client = httpx.AsyncClient(base_url="http://test", transport=httpx.MockTransport(handler))
    return client


async def test_get_events_reads_every_page():
    pages = {
        None: {"events": [{"sequence": 1}, {"sequence": 2}], "next_after": 2},
        "2": {"events": [{"sequence": 3}, {"sequence": 4}], "next_after": 4},
        "4": {"events": [{"sequence": 5}], "next_after": None},
    }
    seen: list[str | None] = []

    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/executions/exec_1/events"
        after = request.url.params.get("after")
        seen.append(after)
        return httpx.Response(200, json=pages[after])

    client = await _client(handler)
    try:
        out = await client.get_events("exec_1")
    finally:
        await client._client.aclose()
    assert [e["sequence"] for e in out["events"]] == [1, 2, 3, 4, 5]
    assert seen == [None, "2", "4"]


async def test_get_events_without_a_cursor_reads_one_page():
    """An older runtime sends no ``next_after``: one request, as before."""
    calls = 0

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        return httpx.Response(200, json={"events": [{"sequence": 1}]})

    client = await _client(handler)
    try:
        out = await client.get_events("exec_1")
    finally:
        await client._client.aclose()
    assert out == {"events": [{"sequence": 1}]}
    assert calls == 1
