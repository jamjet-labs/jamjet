"""A stand-in for the Anthropic Messages API, for CI runs with no real key.

Answers every POST .../messages with one fixed text reply, so an agent built
from `jamjet create` can run end to end on a fresh install: the request goes
through litellm exactly as it would to the real API.

    python stub_model_provider.py 8765 &
    ANTHROPIC_API_BASE=http://127.0.0.1:8765 ANTHROPIC_API_KEY=stub python agent.py
"""

from __future__ import annotations

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

REPLY = "7 plus 35 is 42."


class Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:  # noqa: N802 (http.server API)
        length = int(self.headers.get("content-length", 0))
        request = json.loads(self.rfile.read(length) or b"{}")
        if not self.path.rstrip("/").endswith("/messages"):
            self.send_error(404)
            return
        body = json.dumps(
            {
                "id": "msg_stub",
                "type": "message",
                "role": "assistant",
                "model": request.get("model", "stub"),
                "content": [{"type": "text", "text": REPLY}],
                "stop_reason": "end_turn",
                "stop_sequence": None,
                "usage": {"input_tokens": 10, "output_tokens": 8},
            }
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt: str, *args: object) -> None:
        sys.stderr.write(f"stub provider: {self.command} {self.path}\n")


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
