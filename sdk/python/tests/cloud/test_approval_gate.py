"""require_approval rules gate the tool calls a model returns.

The patcher opens a Cloud approval and waits for the decision before handing
the response to user code, and fails closed on rejection, timeout, or an
unreachable Cloud. The approval carries the SDK's timeout as ``timeout_ms``.

Cloud is stubbed with respx; the vendor SDKs run for real against an httpx
MockTransport.
"""

from __future__ import annotations

import importlib
import json
from collections.abc import Iterator
from typing import Any

import anthropic  # imported before conftest can stub the module
import httpx
import openai
import pytest
import respx

from jamjet.cloud.approvals import request_approval
from jamjet.cloud.exceptions import JamJetApprovalRejected, JamJetApprovalTimeout
from jamjet.cloud.policy import PolicyEvaluator

budget_mod = importlib.import_module("jamjet.cloud.budget")
config_mod = importlib.import_module("jamjet.cloud.config")
policy_mod = importlib.import_module("jamjet.cloud.policy")
patcher = importlib.import_module("jamjet.cloud.patcher")
redaction_mod = importlib.import_module("jamjet.cloud.redaction")

API = "https://api.jamjet.test"


@pytest.fixture
def env(monkeypatch: pytest.MonkeyPatch) -> Iterator[dict[str, Any]]:
    spans: list[dict[str, Any]] = []
    evaluator = PolicyEvaluator()
    cfg = config_mod.CloudConfig(api_key="jj_test", api_url=API)
    cfg.approval_poll_interval = 0.01
    monkeypatch.setattr(config_mod, "_config", cfg)
    monkeypatch.setattr(policy_mod, "_evaluator", evaluator)
    monkeypatch.setattr(patcher, "emit", spans.append)
    budget_mod.set_budget(100.0)
    patcher.patch_openai()
    patcher.patch_anthropic()
    try:
        yield {"spans": spans, "policy": evaluator, "config": cfg}
    finally:
        patcher.unpatch_openai()
        patcher.unpatch_anthropic()
        budget_mod.set_budget(None)


def _cloud(router: respx.Router, statuses: list[str], created: list[dict[str, Any]]) -> None:
    def create(request: httpx.Request) -> httpx.Response:
        created.append(json.loads(request.content))
        return httpx.Response(200, json={"id": f"apr_{len(created)}", "status": "pending"})

    replies = iter(statuses)

    def poll(request: httpx.Request) -> httpx.Response:
        status = next(replies, statuses[-1])
        return httpx.Response(200, json={"id": "apr_1", "status": status, "reason": "nope"})

    router.post(f"{API}/v1/approvals").mock(side_effect=create)
    router.get(url__regex=rf"{API}/v1/approvals/.+").mock(side_effect=poll)


def _openai_with_tool_call(name: str, arguments: str = '{"amount": 120}') -> openai.OpenAI:
    def respond(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            200,
            json={
                "id": "c1",
                "object": "chat.completion",
                "created": 1,
                "model": "gpt-4o",
                "choices": [
                    {
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": None,
                            "tool_calls": [
                                {"id": "call_9", "type": "function", "function": {"name": name, "arguments": arguments}}
                            ],
                        },
                        "finish_reason": "tool_calls",
                    }
                ],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            },
        )

    return openai.OpenAI(
        api_key="x",
        base_url="http://vendor.test/v1",
        max_retries=0,
        http_client=httpx.Client(transport=httpx.MockTransport(respond)),
    )


def _anthropic_stream(name: str) -> anthropic.Anthropic:
    events = [
        (
            "message_start",
            {
                "type": "message_start",
                "message": {
                    "id": "m",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-sonnet-4-6",
                    "content": [],
                    "stop_reason": None,
                    "stop_sequence": None,
                    "usage": {"input_tokens": 10, "output_tokens": 1},
                },
            },
        ),
        (
            "content_block_start",
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "tu_7", "name": name, "input": {}},
            },
        ),
        (
            "content_block_delta",
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": '{"amount": 5}'},
            },
        ),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        (
            "message_delta",
            {
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                "usage": {"output_tokens": 20},
            },
        ),
        ("message_stop", {"type": "message_stop"}),
    ]
    body = "".join(f"event: {e}\ndata: {json.dumps(d)}\n\n" for e, d in events).encode()

    def respond(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=body, headers={"content-type": "text/event-stream"})

    return anthropic.Anthropic(
        api_key="x",
        base_url="http://vendor.test",
        max_retries=0,
        http_client=httpx.Client(transport=httpx.MockTransport(respond)),
    )


def test_request_approval_sends_the_sdk_timeout_as_timeout_ms() -> None:
    created: list[dict[str, Any]] = []
    with respx.mock(assert_all_called=False) as router:
        _cloud(router, ["approved"], created)
        request_approval("jj_test", API, "x", timeout_seconds=120, poll_interval=0.01)
        request_approval("jj_test", API, "x", timeout_seconds=5, poll_interval=0.01)
        request_approval("jj_test", API, "x", timeout_seconds=3 * 86_400, poll_interval=0.01)
    assert [c["timeout_ms"] for c in created] == [120_000, 60_000, 86_400_000]


def test_request_approval_treats_unknown_decided_status_as_rejection() -> None:
    with respx.mock(assert_all_called=False) as router:
        _cloud(router, ["expired"], [])
        with pytest.raises(JamJetApprovalRejected):
            request_approval("jj_test", API, "x", timeout_seconds=60, poll_interval=0.01)


def test_approved_tool_call_is_returned(env: dict[str, Any]) -> None:
    env["policy"].add("require_approval", "payments.*")
    env["config"].approval_timeout_seconds = 120
    created: list[dict[str, Any]] = []
    with respx.mock(assert_all_called=False) as router:
        _cloud(router, ["pending", "approved"], created)
        resp = _openai_with_tool_call("payments.refund").chat.completions.create(
            model="gpt-4o", messages=[{"role": "user", "content": "refund"}]
        )
    assert resp.choices[0].message.tool_calls[0].function.name == "payments.refund"
    assert created[0]["action"] == "payments.refund"
    assert created[0]["timeout_ms"] == 120_000
    context = created[0]["context"]
    assert {k: context[k] for k in ("tool", "vendor", "model", "rule", "tool_call_id", "arguments")} == {
        "tool": "payments.refund",
        "vendor": "openai",
        "model": "gpt-4o",
        "rule": "payments.*",
        "tool_call_id": "call_9",
        "arguments": '{"amount": 120}',
    }
    assert env["spans"][-1]["status"] == "ok"
    assert env["spans"][-1]["payload"]["approval_ids"] == ["apr_1"]


def test_rejected_tool_call_raises(env: dict[str, Any]) -> None:
    env["policy"].add("require_approval", "payments.*")
    with respx.mock(assert_all_called=False) as router:
        _cloud(router, ["rejected"], [])
        with pytest.raises(JamJetApprovalRejected):
            _openai_with_tool_call("payments.refund").chat.completions.create(
                model="gpt-4o", messages=[{"role": "user", "content": "refund"}]
            )
    assert env["spans"][-1]["status"] == "error"


def test_undecided_tool_call_times_out(env: dict[str, Any]) -> None:
    env["policy"].add("require_approval", "payments.*")
    env["config"].approval_timeout_seconds = 0.05
    with respx.mock(assert_all_called=False) as router:
        _cloud(router, ["pending"], [])
        with pytest.raises(JamJetApprovalTimeout):
            _openai_with_tool_call("payments.refund").chat.completions.create(
                model="gpt-4o", messages=[{"role": "user", "content": "refund"}]
            )


def test_unreachable_cloud_fails_closed(env: dict[str, Any]) -> None:
    env["policy"].add("require_approval", "payments.*")
    with respx.mock(assert_all_called=False) as router:
        router.post(f"{API}/v1/approvals").mock(return_value=httpx.Response(503, json={"error": "down"}))
        with pytest.raises(JamJetApprovalTimeout):
            _openai_with_tool_call("payments.refund").chat.completions.create(
                model="gpt-4o", messages=[{"role": "user", "content": "refund"}]
            )


def test_ungated_tool_call_asks_for_nothing(env: dict[str, Any]) -> None:
    env["policy"].add("require_approval", "payments.*")
    with respx.mock(assert_all_called=False) as router:
        route = router.post(f"{API}/v1/approvals")
        _openai_with_tool_call("search").chat.completions.create(
            model="gpt-4o", messages=[{"role": "user", "content": "find"}]
        )
    assert not route.called


def test_approval_context_is_redacted_when_redaction_is_on(
    env: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setitem(redaction_mod._config, "enabled", True)
    env["policy"].add("require_approval", "email.*")
    created: list[dict[str, Any]] = []
    with respx.mock(assert_all_called=False) as router:
        _cloud(router, ["approved"], created)
        _openai_with_tool_call("email.send", '{"to": "jane.doe@example.com"}').chat.completions.create(
            model="gpt-4o", messages=[{"role": "user", "content": "mail"}]
        )
    assert "jane.doe@example.com" not in json.dumps(created[0]["context"])


def test_streamed_tool_call_is_held_until_approved(env: dict[str, Any]) -> None:
    env["policy"].add("require_approval", "payments.*")
    created: list[dict[str, Any]] = []
    with respx.mock(assert_all_called=False) as router:
        _cloud(router, ["approved"], created)
        events = list(
            _anthropic_stream("payments.refund").messages.create(
                model="claude-sonnet-4-6", max_tokens=64, stream=True, messages=[{"role": "user", "content": "x"}]
            )
        )
    assert [e.type for e in events][-1] == "message_stop"
    assert created[0]["context"]["arguments"] == {"amount": 5}
    assert created[0]["context"]["tool_call_id"] == "tu_7"


def test_streamed_tool_call_rejected_never_reaches_caller(env: dict[str, Any]) -> None:
    env["policy"].add("require_approval", "payments.*")
    seen: list[str] = []
    with respx.mock(assert_all_called=False) as router:
        _cloud(router, ["rejected"], [])
        stream = _anthropic_stream("payments.refund").messages.create(
            model="claude-sonnet-4-6", max_tokens=64, stream=True, messages=[{"role": "user", "content": "x"}]
        )
        with pytest.raises(JamJetApprovalRejected):
            for event in stream:
                seen.append(event.type)
    assert seen == ["message_start"]
    assert budget_mod.get_budget().spent == pytest.approx(10 * 3e-6 + 20 * 15e-6)  # counted once
    assert len(env["spans"]) == 1
    assert env["spans"][-1]["status"] == "error"
