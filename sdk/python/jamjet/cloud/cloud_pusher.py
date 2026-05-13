"""CloudPusher — Path B fire-and-forget push for the Python SDK.

Mirror of the TS @jamjet/cloud CloudPusher contract:
  - Never raises. push() returns bool.
  - 500ms default timeout (httpx.Client(timeout=...)).
  - Circuit breaker: 5 consecutive failures → open for 60s.
  - 4xx and 5xx both count as failures. Direct-push has no outbox to retry
    from, so anything other than 2xx is dropped.

The agent's tool-call latency budget must not be affected by Cloud
reachability — that's the whole point of Path B's separation from the
local audit JSONL write.
"""

from __future__ import annotations

import os
import time
from typing import Any

import httpx


class CloudPusher:
    """Fire-and-forget Cloud push for Path B (serverless / CI)."""

    def __init__(
        self,
        api_base: str,
        api_key: str,
        *,
        timeout_seconds: float = 0.5,
        circuit_breaker_threshold: int = 5,
        circuit_breaker_reset_seconds: float = 60.0,
        user_agent: str = "jamjet python direct-push",
    ) -> None:
        self.api_base = api_base.rstrip("/")
        self.api_key = api_key
        self.timeout_seconds = timeout_seconds
        self.circuit_breaker_threshold = circuit_breaker_threshold
        self.circuit_breaker_reset_seconds = circuit_breaker_reset_seconds
        self.user_agent = user_agent
        self.consecutive_failures = 0
        self._circuit_opened_at: float | None = None
        self._client = httpx.Client(timeout=self.timeout_seconds)

    def is_circuit_open(self) -> bool:
        if self._circuit_opened_at is None:
            return False
        if time.time() - self._circuit_opened_at > self.circuit_breaker_reset_seconds:
            self._circuit_opened_at = None
            self.consecutive_failures = 0
            return False
        return True

    def push(self, event: dict[str, Any]) -> bool:
        """POST one audit event to /v1/policy-audit/events. Returns True on 2xx."""
        if self.is_circuit_open():
            return False
        try:
            response = self._client.post(
                f"{self.api_base}/v1/policy-audit/events",
                json={"events": [event], "path": "direct"},
                headers={
                    "authorization": f"Bearer {self.api_key}",
                    "user-agent": self.user_agent,
                },
            )
            if 200 <= response.status_code < 300:
                self.consecutive_failures = 0
                return True
            self._record_failure()
            return False
        except Exception:
            self._record_failure()
            return False

    def _record_failure(self) -> None:
        self.consecutive_failures += 1
        if self.consecutive_failures >= self.circuit_breaker_threshold:
            self._circuit_opened_at = time.time()

    def close(self) -> None:
        """Close the underlying httpx client. Idempotent."""
        try:
            self._client.close()
        except Exception:
            pass


SERVERLESS_ENV_VARS = (
    "VERCEL",
    "CF_PAGES",
    "AWS_LAMBDA_FUNCTION_NAME",
    "GITHUB_ACTIONS",
    "NETLIFY",
)


def detect_path_mode() -> str:
    """'local-only' or 'direct'. Mirror of the TS detectPathMode logic.

    Selection rules:
      1. No JAMJET_CLOUD_TOKEN → local-only.
      2. JAMJET_CLOUD_MODE=direct → direct.
      3. JAMJET_CLOUD_MODE=daemon → local-only.
      4. Any serverless indicator env var set → direct.
      5. Otherwise → local-only.
    """
    if not os.environ.get("JAMJET_CLOUD_TOKEN"):
        return "local-only"
    explicit = os.environ.get("JAMJET_CLOUD_MODE")
    if explicit == "direct":
        return "direct"
    if explicit == "daemon":
        return "local-only"
    for v in SERVERLESS_ENV_VARS:
        if os.environ.get(v):
            return "direct"
    return "local-only"
