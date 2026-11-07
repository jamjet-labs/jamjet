from __future__ import annotations

import time
from typing import Any

import httpx

from .exceptions import JamJetApprovalRejected, JamJetApprovalTimeout

# The deadline range Cloud accepts for an approval (POST /v1/approvals
# ``timeout_ms``, clamped server-side to [1 minute, 24 hours]). Cloud refuses a
# decision after the deadline, so waiting longer than it holds is pointless.
_CLOUD_MIN_TIMEOUT_MS = 60_000
_CLOUD_MAX_TIMEOUT_MS = 86_400_000


def request_approval(
    api_key: str,
    api_url: str,
    action: str,
    context: dict[str, Any] | None = None,
    timeout_seconds: float = 3600,
    poll_interval: float = 5.0,
) -> str:
    """Create an approval request and poll until resolved.

    The timeout is sent as the approval's ``timeout_ms``, so Cloud holds the
    request as long as the SDK waits for it. Values outside the range Cloud
    accepts are clamped in what is sent, and the wait is capped at Cloud's
    24-hour maximum. Any decided status other than ``approved`` is a rejection.

    Returns the approval_id on success.
    Raises JamJetApprovalRejected or JamJetApprovalTimeout on failure.
    """
    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
    }
    timeout_ms = min(int(timeout_seconds * 1000), _CLOUD_MAX_TIMEOUT_MS)
    payload: dict[str, Any] = {"action": action, "timeout_ms": max(timeout_ms, _CLOUD_MIN_TIMEOUT_MS)}
    if context:
        payload["context"] = context

    # Create the approval request
    resp = httpx.post(
        f"{api_url}/v1/approvals",
        json=payload,
        headers=headers,
        timeout=10,
    )
    resp.raise_for_status()
    data = resp.json()
    approval_id: str = data["id"]

    # Poll for resolution
    deadline = time.monotonic() + timeout_ms / 1000
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        time.sleep(min(poll_interval, remaining))
        poll_resp = httpx.get(
            f"{api_url}/v1/approvals/{approval_id}",
            headers=headers,
            timeout=10,
        )
        poll_resp.raise_for_status()
        status_data = poll_resp.json()
        status = status_data.get("status", "pending")
        if status == "approved":
            return approval_id
        if status != "pending":
            # Fail closed: rejected, expired, or a status this SDK does not know.
            reason = status_data.get("reason")
            raise JamJetApprovalRejected(approval_id, reason=reason if status == "rejected" else reason or status)

    raise JamJetApprovalTimeout(approval_id, timeout_seconds)
