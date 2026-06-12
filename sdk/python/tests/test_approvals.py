"""Tests for approval client methods and CLI commands."""

from __future__ import annotations

import json
from unittest.mock import AsyncMock, patch

import httpx
import pytest
import respx
from typer.testing import CliRunner

from jamjet.cli.main import app
from jamjet.client import JamjetClient

runner = CliRunner()

BASE = "http://localhost:7700"

# ---------------------------------------------------------------------------
# Client tests
# ---------------------------------------------------------------------------


@respx.mock
@pytest.mark.asyncio
async def test_approve_sends_node_id_when_given() -> None:
    """approve() includes node_id in the request body when provided."""
    route = respx.post(f"{BASE}/executions/exec-1/approve").respond(
        200,
        json={"execution_id": "exec-1", "node_id": "gated", "accepted": True},
    )

    async with JamjetClient(base_url=BASE) as c:
        result = await c.approve("exec-1", "approved", node_id="gated")

    assert route.called
    body = json.loads(route.calls[0].request.content)
    assert body["decision"] == "approved"
    assert body["node_id"] == "gated"
    assert result["node_id"] == "gated"


@respx.mock
@pytest.mark.asyncio
async def test_approve_omits_node_id_when_not_given() -> None:
    """approve() omits node_id from the body when not supplied."""
    route = respx.post(f"{BASE}/executions/exec-1/approve").respond(
        200,
        json={"execution_id": "exec-1", "node_id": "inferred", "accepted": True},
    )

    async with JamjetClient(base_url=BASE) as c:
        await c.approve("exec-1", "approved")

    body = json.loads(route.calls[0].request.content)
    assert "node_id" not in body
    assert body["decision"] == "approved"


@respx.mock
@pytest.mark.asyncio
async def test_list_approvals_returns_parsed_dict() -> None:
    """list_approvals() GETs /executions/{id}/approvals and returns the body."""
    payload = {
        "pending": [
            {
                "node_id": "gated",
                "tool_name": "payments.transfer",
                "approver": "human",
                "context": {},
                "sequence": 3,
            }
        ],
        "decided": [
            {
                "node_id": "old",
                "status": "approved",
                "user_id": "u1",
                "sequence": 1,
            }
        ],
    }
    route = respx.get(f"{BASE}/executions/exec-1/approvals").respond(200, json=payload)

    async with JamjetClient(base_url=BASE) as c:
        result = await c.list_approvals("exec-1")

    assert route.called
    assert result["pending"][0]["node_id"] == "gated"
    assert result["decided"][0]["status"] == "approved"


# ---------------------------------------------------------------------------
# CLI tests — jamjet approvals
# ---------------------------------------------------------------------------


def _approvals_client(pending: list, decided: list):
    """Return a patched _client() whose list_approvals returns the given data."""
    fake = AsyncMock()
    fake.list_approvals = AsyncMock(return_value={"pending": pending, "decided": decided})
    fake.__aenter__ = AsyncMock(return_value=fake)
    fake.__aexit__ = AsyncMock(return_value=None)
    return fake


def test_approvals_renders_pending_and_decided() -> None:
    """jamjet approvals renders both pending and decided entries."""
    pending = [
        {
            "node_id": "gated",
            "tool_name": "payments.transfer",
            "approver": "human",
            "context": {},
            "sequence": 3,
        }
    ]
    decided = [
        {
            "node_id": "old",
            "status": "approved",
            "user_id": "u1",
            "sequence": 1,
        }
    ]
    fake = _approvals_client(pending, decided)

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    assert "gated" in result.output
    assert "payments.transfer" in result.output
    assert "old" in result.output
    assert "approved" in result.output


def test_approvals_shows_empty_state_message() -> None:
    """jamjet approvals shows a friendly message when there is nothing pending."""
    fake = _approvals_client([], [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    # Should not raise and should print something useful
    assert result.output.strip() != ""


# ---------------------------------------------------------------------------
# CLI tests — jamjet approve
# ---------------------------------------------------------------------------


def _approve_client(response: dict):
    fake = AsyncMock()
    fake.approve = AsyncMock(return_value=response)
    fake.__aenter__ = AsyncMock(return_value=fake)
    fake.__aexit__ = AsyncMock(return_value=None)
    return fake


def test_approve_without_node_id_prints_resolved_node() -> None:
    """jamjet approve prints resolved node_id on success."""
    fake = _approve_client({"execution_id": "exec-1", "node_id": "gated", "accepted": True})

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approve", "exec-1", "--decision", "approved"])

    assert result.exit_code == 0, result.output
    assert "gated" in result.output
    fake.approve.assert_awaited_once()
    call_kwargs = fake.approve.call_args
    # node_id should not be passed when not specified
    assert call_kwargs.kwargs.get("node_id") is None


def test_approve_with_node_id_and_comment_passes_both() -> None:
    """jamjet approve --node-id X --comment Y sends both to client."""
    fake = _approve_client({"execution_id": "exec-1", "node_id": "gated", "accepted": True})

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(
            app,
            ["approve", "exec-1", "--decision", "approved", "--node-id", "gated", "--comment", "no"],
        )

    assert result.exit_code == 0, result.output
    call_kwargs = fake.approve.call_args
    assert call_kwargs.kwargs.get("node_id") == "gated"
    assert call_kwargs.kwargs.get("comment") == "no"


def test_approve_409_exits_nonzero_with_error_text() -> None:
    """jamjet approve exits 1 on 409 and prints the server error, not a traceback."""
    error_body = {"error": "no pending approval on this execution"}

    # Build a real httpx.HTTPStatusError the CLI handler will catch
    request = httpx.Request("POST", f"{BASE}/executions/exec-1/approve")
    response = httpx.Response(409, json=error_body, request=request)
    http_error = httpx.HTTPStatusError("409", request=request, response=response)

    fake = AsyncMock()
    fake.approve = AsyncMock(side_effect=http_error)
    fake.__aenter__ = AsyncMock(return_value=fake)
    fake.__aexit__ = AsyncMock(return_value=None)

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approve", "exec-1", "--decision", "approved"])

    assert result.exit_code != 0
    assert "no pending approval" in result.output
    # Must not be a traceback dump
    assert "Traceback" not in result.output


def test_approve_invalid_decision_fails_cleanly() -> None:
    """jamjet approve --decision maybe produces a parameter error, not a traceback."""
    result = runner.invoke(app, ["approve", "exec-1", "--decision", "maybe"])
    assert result.exit_code != 0
    assert "Traceback" not in result.output
    # Should mention the invalid value or expected values
    output = result.output + (result.stderr or "")
    assert "maybe" in output or "approved" in output or "rejected" in output
