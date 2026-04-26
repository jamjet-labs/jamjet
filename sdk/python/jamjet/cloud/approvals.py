from __future__ import annotations

import time
from typing import Any

import httpx

from .exceptions import JamJetApprovalRejected, JamJetApprovalTimeout


def request_approval(
    api_key: str,
    api_url: str,
    action: str,
    context: dict[str, Any] | None = None,
    timeout_seconds: float = 3600,
) -> str:
    """Create an approval request and poll until resolved.

    Returns the approval_id on success.
    Raises JamJetApprovalRejected or JamJetApprovalTimeout on failure.
    """
    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
    }
    payload: dict[str, Any] = {"action": action}
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
    deadline = time.monotonic() + timeout_seconds
    poll_interval = 5.0
    while time.monotonic() < deadline:
        time.sleep(poll_interval)
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
        if status == "rejected":
            raise JamJetApprovalRejected(approval_id, reason=status_data.get("reason"))

    raise JamJetApprovalTimeout(approval_id, timeout_seconds)
