"""
MCP server for JamJet Python SDK.

Expose @tool-decorated functions as an MCP-compatible server using the
Streamable HTTP transport (POST /mcp for JSON-RPC, GET /mcp/sse for SSE).

Usage::

    from jamjet import tool
    from jamjet.protocols import serve_tools

    @tool
    async def add(a: int, b: int) -> int:
        return a + b

    app = serve_tools([add])
    # As ASGI app:  uvicorn my_module:app
    # Standalone:   anyio.run(app.run, "0.0.0.0", 9000)
"""

from __future__ import annotations

import inspect
import json
from collections.abc import Callable
from typing import Any

import anyio

from jamjet.tools.decorators import ToolDefinition


def serve_tools(
    tools: list[Callable[..., Any]],
    *,
    server_name: str = "jamjet",
    server_version: str = "0.1.0",
    resources: list[dict[str, Any]] | None = None,
) -> McpAsgiApp:
    """Build an MCP server app from a list of @tool-decorated functions."""
    defs: dict[str, ToolDefinition] = {}
    for fn in tools:
        defn: ToolDefinition | None = getattr(fn, "_jamjet_tool", None)
        if defn is None:
            raise ValueError(f"{fn!r} is not a @tool-decorated function")
        defs[defn.name] = defn
    return McpAsgiApp(
        tools=defs,
        server_name=server_name,
        server_version=server_version,
        resources=resources or [],
    )


def _normalize_schema(schema: dict[str, Any]) -> dict[str, Any]:
    """Normalize an input_schema so property values are proper JSON Schema objects.

    The @tool decorator's _type_to_schema returns bare strings like "string"
    for simple types.  MCP requires {"type": "string"}.
    """
    schema = dict(schema)  # shallow copy
    if "properties" in schema:
        props = {}
        for key, val in schema["properties"].items():
            if isinstance(val, str):
                props[key] = {"type": val}
            else:
                props[key] = val
        schema["properties"] = props
    return schema


class McpAsgiApp:
    """ASGI application implementing MCP Streamable HTTP transport."""

    def __init__(
        self,
        tools: dict[str, ToolDefinition],
        server_name: str,
        server_version: str,
        resources: list[dict[str, Any]],
    ) -> None:
        self._tools = tools
        self._server_name = server_name
        self._server_version = server_version
        self._resources = resources

    # ── ASGI interface ────────────────────────────────────────────────────

    async def __call__(
        self,
        scope: dict[str, Any],
        receive: Callable[..., Any],
        send: Callable[..., Any],
    ) -> None:
        if scope["type"] != "http":
            return

        method = scope.get("method", "GET")
        path = scope.get("path", "")

        if method == "POST" and path == "/mcp":
            body = await _read_body(receive)
            try:
                request = json.loads(body)
            except (json.JSONDecodeError, UnicodeDecodeError):
                await _send_json(send, 400, {"error": "invalid JSON"})
                return
            response = await self._handle_rpc(request)
            await _send_json(send, 200, response)

        elif method == "GET" and path == "/mcp/sse":
            # Minimal SSE keepalive — send a ping comment every 15s.
            await _send_response(
                send,
                200,
                [(b"content-type", b"text/event-stream"), (b"cache-control", b"no-cache")],
                b": keepalive\n\n",
            )

        else:
            await _send_json(send, 404, {"error": "not found"})

    # ── JSON-RPC dispatch ─────────────────────────────────────────────────

    async def _handle_rpc(self, body: dict[str, Any]) -> dict[str, Any]:
        rpc_id = body.get("id", 1)
        method = body.get("method", "")
        params = body.get("params") or {}

        result: Any
        error: dict[str, Any] | None = None

        if method == "initialize":
            result = {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {"listChanged": False},
                    "resources": {"subscribe": False, "listChanged": False},
                    "prompts": {"listChanged": False},
                },
                "serverInfo": {
                    "name": self._server_name,
                    "version": self._server_version,
                },
            }
        elif method == "initialized":
            result = {}
        elif method == "tools/list":
            result = {"tools": [self._tool_info(d) for d in self._tools.values()]}
        elif method == "tools/call":
            result = await self._call_tool(params)
            if result is None:
                error = {"code": -32601, "message": f"tool not found: {params.get('name', '')}"}
        elif method == "resources/list":
            result = {"resources": self._resources}
        elif method == "prompts/list":
            result = {"prompts": []}
        elif method == "ping":
            result = {}
        else:
            error = {"code": -32601, "message": f"method not found: {method}"}

        resp: dict[str, Any] = {"jsonrpc": "2.0", "id": rpc_id}
        if error is not None:
            resp["error"] = error
        else:
            resp["result"] = result
        return resp

    def _tool_info(self, defn: ToolDefinition) -> dict[str, Any]:
        return {
            "name": defn.name,
            "description": defn.description or "",
            "inputSchema": _normalize_schema(defn.input_schema),
        }

    async def _call_tool(self, params: dict[str, Any]) -> dict[str, Any] | None:
        name = params.get("name", "")
        arguments = params.get("arguments") or {}
        defn = self._tools.get(name)
        if defn is None:
            return None
        try:
            result = defn.fn(**arguments)
            if inspect.isawaitable(result):
                result = await result
            # Serialize the result
            text = json.dumps(result, default=str)
            return {
                "content": [{"type": "text", "text": text}],
                "isError": False,
            }
        except Exception as exc:
            return {
                "content": [{"type": "text", "text": str(exc)}],
                "isError": True,
            }

    # ── Standalone HTTP server ────────────────────────────────────────────

    async def run(self, host: str = "0.0.0.0", port: int = 9000) -> None:
        """Run a minimal HTTP server (for local dev / MCP integration)."""
        listener = await anyio.create_tcp_listener(
            local_host=host,
            local_port=port,
        )
        async with listener:
            await listener.serve(self._handle_connection)

    async def _handle_connection(self, stream: anyio.abc.SocketStream) -> None:
        try:
            data = await stream.receive(65536)
            request_text = data.decode("utf-8", errors="replace")

            # Parse HTTP request line
            lines = request_text.split("\r\n")
            if not lines:
                return
            request_line = lines[0].split(" ")
            if len(request_line) < 2:
                return
            method = request_line[0]
            path = request_line[1]

            # Parse headers
            headers: dict[str, str] = {}
            body_start = request_text.find("\r\n\r\n")
            for line in lines[1:]:
                if not line:
                    break
                if ":" in line:
                    k, v = line.split(":", 1)
                    headers[k.strip().lower()] = v.strip()

            # Read body
            body = b""
            if body_start >= 0:
                body = data[body_start + 4 :]
                content_length = int(headers.get("content-length", "0"))
                while len(body) < content_length:
                    chunk = await stream.receive(65536)
                    body += chunk

            # Build ASGI-like scope
            scope = {"type": "http", "method": method, "path": path}

            response_body = b""
            response_status = 200
            response_headers: list[tuple[bytes, bytes]] = []

            async def receive_fn() -> dict[str, Any]:
                return {"type": "http.request", "body": body}

            async def send_fn(message: dict[str, Any]) -> None:
                nonlocal response_body, response_status, response_headers
                if message["type"] == "http.response.start":
                    response_status = message["status"]
                    response_headers = message.get("headers", [])
                elif message["type"] == "http.response.body":
                    response_body = message.get("body", b"")

            await self(scope, receive_fn, send_fn)

            # Build HTTP response
            status_text = {200: "OK", 400: "Bad Request", 404: "Not Found"}.get(response_status, "OK")
            resp_lines = [f"HTTP/1.1 {response_status} {status_text}"]
            for k, v in response_headers:
                resp_lines.append(f"{k.decode()}: {v.decode()}")
            resp_lines.append(f"Content-Length: {len(response_body)}")
            resp_lines.append("")
            resp_lines.append("")
            resp = "\r\n".join(resp_lines).encode() + response_body
            await stream.send(resp)
        except Exception:
            pass
        finally:
            await stream.aclose()


# ── ASGI helpers ──────────────────────────────────────────────────────────────


async def _read_body(receive: Callable[..., Any]) -> bytes:
    body = b""
    while True:
        message = await receive()
        body += message.get("body", b"")
        if not message.get("more_body", False):
            break
    return body


async def _send_json(send: Callable[..., Any], status: int, data: Any) -> None:
    payload = json.dumps(data).encode()
    await _send_response(
        send,
        status,
        [(b"content-type", b"application/json"), (b"content-length", str(len(payload)).encode())],
        payload,
    )


async def _send_response(
    send: Callable[..., Any],
    status: int,
    headers: list[tuple[bytes, bytes]],
    body: bytes,
) -> None:
    await send({"type": "http.response.start", "status": status, "headers": headers})
    await send({"type": "http.response.body", "body": body})
