"""B3 Task 10 — openai_guardrail integration with Path B direct-push."""

from __future__ import annotations

import json
from pathlib import Path

import httpx
import pytest
import respx

from jamjet.cloud.cloud_pusher import CloudPusher
from jamjet.integrations.openai_guardrail import jamjet_guardrail
from jamjet.integrations.openai_guardrail.guardrail import JamjetPolicyBlocked


def _write_block_policy(tmp_path: Path) -> str:
    policy_path = tmp_path / "policy.yaml"
    policy_path.write_text(
        'version: 1\nrules:\n  - match: "payments.refund"\n    action: block\n',
        encoding="utf-8",
    )
    return str(policy_path)


def _block_input() -> dict[str, object]:
    return {"toolName": "payments.refund", "toolArgs": {"customer_email": "alice@example.com", "amount": 50_000}}


@respx.mock
def test_guardrail_pushes_to_cloud_when_pusher_provided(tmp_path: Path) -> None:
    pushed: list[httpx.Request] = []

    def handler(req: httpx.Request) -> httpx.Response:
        pushed.append(req)
        return httpx.Response(200, json={"accepted": 1, "rejected": 0, "duplicates": 0, "errors": []})

    respx.post("https://api.example.com/v1/policy-audit/events").mock(side_effect=handler)

    pusher = CloudPusher(api_base="https://api.example.com", api_key="jj_test")
    guardrail = jamjet_guardrail(
        policy=_write_block_policy(tmp_path),
        audit_destination=str(tmp_path / "audit"),
        cloud_pusher=pusher,
    )
    with pytest.raises(JamjetPolicyBlocked):
        guardrail(_block_input())

    assert len(pushed) == 1
    body = json.loads(pushed[0].read().decode("utf-8"))
    assert body["path"] == "direct"
    event = body["events"][0]
    assert event["decision"] == "BLOCKED"
    assert event["tool"] == "payments.refund"
    pusher.close()


@respx.mock
def test_guardrail_redacts_args_on_push_by_default(tmp_path: Path) -> None:
    """R9: args content must not leave the host unless operator opts in."""
    pushed: list[httpx.Request] = []

    def handler(req: httpx.Request) -> httpx.Response:
        pushed.append(req)
        return httpx.Response(200, json={})

    respx.post("https://api.example.com/v1/policy-audit/events").mock(side_effect=handler)

    pusher = CloudPusher(api_base="https://api.example.com", api_key="jj_test")
    guardrail = jamjet_guardrail(
        policy=_write_block_policy(tmp_path),
        audit_destination=str(tmp_path / "audit"),
        cloud_pusher=pusher,
    )
    with pytest.raises(JamjetPolicyBlocked):
        guardrail(_block_input())

    body = json.loads(pushed[0].read().decode("utf-8"))
    event = body["events"][0]
    assert event["args"] == {"redacted": True}
    assert event["args_redaction"] == "full"
    # Customer email must not appear anywhere in the pushed body.
    assert "alice@example.com" not in pushed[0].read().decode("utf-8")
    pusher.close()


@respx.mock
def test_guardrail_skips_push_when_no_pusher_configured(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Without JAMJET_CLOUD_TOKEN, detect_path_mode returns local-only."""
    monkeypatch.delenv("JAMJET_CLOUD_TOKEN", raising=False)
    monkeypatch.delenv("VERCEL", raising=False)

    pushed: list[httpx.Request] = []
    respx.post("https://api.example.com/v1/policy-audit/events").mock(
        side_effect=lambda req: pushed.append(req) or httpx.Response(200, json={}),
    )

    guardrail = jamjet_guardrail(
        policy=_write_block_policy(tmp_path),
        audit_destination=str(tmp_path / "audit"),
    )
    with pytest.raises(JamjetPolicyBlocked):
        guardrail(_block_input())

    assert pushed == []


@respx.mock
def test_guardrail_auto_constructs_pusher_in_path_b(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Setting JAMJET_CLOUD_TOKEN + VERCEL=1 should auto-construct a pusher."""
    monkeypatch.setenv("JAMJET_CLOUD_TOKEN", "jj_auto")
    monkeypatch.setenv("VERCEL", "1")
    monkeypatch.setenv("JAMJET_API_BASE", "https://api.example.com")

    pushed: list[httpx.Request] = []
    respx.post("https://api.example.com/v1/policy-audit/events").mock(
        side_effect=lambda req: pushed.append(req) or httpx.Response(200, json={}),
    )

    guardrail = jamjet_guardrail(
        policy=_write_block_policy(tmp_path),
        audit_destination=str(tmp_path / "audit"),
    )
    with pytest.raises(JamjetPolicyBlocked):
        guardrail(_block_input())

    assert len(pushed) == 1
    assert pushed[0].headers.get("authorization") == "Bearer jj_auto"


@respx.mock
def test_guardrail_propagates_trace_id_from_headers(tmp_path: Path) -> None:
    pushed: list[httpx.Request] = []
    respx.post("https://api.example.com/v1/policy-audit/events").mock(
        side_effect=lambda req: pushed.append(req) or httpx.Response(200, json={}),
    )

    pusher = CloudPusher(api_base="https://api.example.com", api_key="jj_test")
    trace_id = "0af7651916cd43dd8448eb211c80319c"
    guardrail = jamjet_guardrail(
        policy=_write_block_policy(tmp_path),
        audit_destination=str(tmp_path / "audit"),
        cloud_pusher=pusher,
        headers={"traceparent": f"00-{trace_id}-b7ad6b7169203331-01"},
    )
    with pytest.raises(JamjetPolicyBlocked):
        guardrail(_block_input())

    body = json.loads(pushed[0].read().decode("utf-8"))
    assert body["events"][0]["trace_id"] == trace_id
    pusher.close()


@respx.mock
def test_guardrail_local_jsonl_still_has_unredacted_args(tmp_path: Path) -> None:
    """Local audit JSONL is full-fidelity; only Cloud-bound copy is redacted."""
    respx.post("https://api.example.com/v1/policy-audit/events").respond(200, json={})
    pusher = CloudPusher(api_base="https://api.example.com", api_key="jj_test")

    audit_root = tmp_path / "audit"
    guardrail = jamjet_guardrail(
        policy=_write_block_policy(tmp_path),
        audit_destination=str(audit_root),
        cloud_pusher=pusher,
    )
    with pytest.raises(JamjetPolicyBlocked):
        guardrail(_block_input())

    # Find the JSONL file under the date directory.
    jsonl_files = list(audit_root.rglob("openai-guardrail.jsonl"))
    assert len(jsonl_files) == 1
    line = jsonl_files[0].read_text().strip()
    record = json.loads(line)
    # Local JSONL keeps the full args verbatim.
    assert record["args"]["customer_email"] == "alice@example.com"
    pusher.close()
