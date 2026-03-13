"""Tests for the MCP server (Python SDK)."""

from __future__ import annotations

import json

import httpx
import pytest

from jamjet import tool
from jamjet.protocols.mcp_server import serve_tools


# ── Test tools ────────────────────────────────────────────────────────────────


@tool
async def add(a: int, b: int) -> int:
    """Add two numbers."""
    return a + b


@tool
async def greet(name: str) -> str:
    """Say hello."""
    return f"Hello, {name}!"


@tool
async def failing_tool(x: str) -> str:
    """Always fails."""
    raise ValueError("boom")


# ── Fixtures ──────────────────────────────────────────────────────────────────


@pytest.fixture
def app():
    return serve_tools([add, greet, failing_tool], server_name="test-server", server_version="0.0.1")


@pytest.fixture
def client(app):
    transport = httpx.ASGITransport(app=app)
    return httpx.AsyncClient(transport=transport, base_url="http://testserver")


# ── Helpers ───────────────────────────────────────────────────────────────────


def rpc(method: str, params: dict | None = None, rpc_id: int = 1) -> dict:
    body: dict = {"jsonrpc": "2.0", "id": rpc_id, "method": method}
    if params is not None:
        body["params"] = params
    return body


# ── Tests ─────────────────────────────────────────────────────────────────────


async def test_initialize(client):
    resp = await client.post("/mcp", json=rpc("initialize"))
    assert resp.status_code == 200
    data = resp.json()
    assert data["result"]["serverInfo"]["name"] == "test-server"
    assert data["result"]["protocolVersion"] == "2024-11-05"


async def test_initialized(client):
    resp = await client.post("/mcp", json=rpc("initialized"))
    assert resp.status_code == 200
    assert resp.json()["result"] == {}


async def test_tools_list(client):
    resp = await client.post("/mcp", json=rpc("tools/list"))
    assert resp.status_code == 200
    tools = resp.json()["result"]["tools"]
    names = {t["name"] for t in tools}
    assert names == {"add", "greet", "failing_tool"}
    # Verify schema normalization — properties should be objects, not bare strings
    add_tool = next(t for t in tools if t["name"] == "add")
    assert add_tool["inputSchema"]["properties"]["a"] == {"type": "integer"}
    assert add_tool["inputSchema"]["properties"]["b"] == {"type": "integer"}


async def test_tools_call_add(client):
    resp = await client.post(
        "/mcp",
        json=rpc("tools/call", {"name": "add", "arguments": {"a": 3, "b": 4}}),
    )
    assert resp.status_code == 200
    result = resp.json()["result"]
    assert result["isError"] is False
    assert json.loads(result["content"][0]["text"]) == 7


async def test_tools_call_greet(client):
    resp = await client.post(
        "/mcp",
        json=rpc("tools/call", {"name": "greet", "arguments": {"name": "World"}}),
    )
    assert resp.status_code == 200
    result = resp.json()["result"]
    assert json.loads(result["content"][0]["text"]) == "Hello, World!"


async def test_tools_call_not_found(client):
    resp = await client.post(
        "/mcp",
        json=rpc("tools/call", {"name": "nonexistent", "arguments": {}}),
    )
    assert resp.status_code == 200
    assert "error" in resp.json()
    assert resp.json()["error"]["code"] == -32601


async def test_tools_call_error(client):
    resp = await client.post(
        "/mcp",
        json=rpc("tools/call", {"name": "failing_tool", "arguments": {"x": "test"}}),
    )
    assert resp.status_code == 200
    result = resp.json()["result"]
    assert result["isError"] is True
    assert "boom" in result["content"][0]["text"]


async def test_resources_list(client):
    resp = await client.post("/mcp", json=rpc("resources/list"))
    assert resp.status_code == 200
    assert resp.json()["result"]["resources"] == []


async def test_prompts_list(client):
    resp = await client.post("/mcp", json=rpc("prompts/list"))
    assert resp.status_code == 200
    assert resp.json()["result"]["prompts"] == []


async def test_ping(client):
    resp = await client.post("/mcp", json=rpc("ping"))
    assert resp.status_code == 200
    assert resp.json()["result"] == {}


async def test_unknown_method(client):
    resp = await client.post("/mcp", json=rpc("unknown/method"))
    assert resp.status_code == 200
    assert resp.json()["error"]["code"] == -32601


async def test_not_found_path(client):
    resp = await client.get("/nonexistent")
    assert resp.status_code == 404


async def test_invalid_json(client):
    resp = await client.post("/mcp", content=b"not json", headers={"content-type": "application/json"})
    assert resp.status_code == 400


async def test_resources_provided():
    resources = [{"uri": "file:///tmp/a.txt", "name": "a.txt", "description": "A file", "mimeType": "text/plain"}]
    app = serve_tools([add], resources=resources)
    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
        resp = await client.post("/mcp", json=rpc("resources/list"))
        assert resp.json()["result"]["resources"] == resources


async def test_serve_tools_rejects_non_tool():
    with pytest.raises(ValueError, match="not a @tool-decorated"):
        serve_tools([lambda x: x])
