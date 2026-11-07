"""Regression tests from the independent review's probes (each probe asserted
the buggy behaviour; these assert the fix). Real vendor clients run against
an httpx MockTransport; nothing leaves the process."""

from __future__ import annotations

import importlib
import json
from collections.abc import Callable, Iterator
from typing import Any

import httpx
import pytest

from jamjet.cloud.exceptions import JamJetPolicyBlocked, JamJetUnenforceableCall
from jamjet.cloud.policy import PolicyEvaluator

anthropic = pytest.importorskip("anthropic")
openai = pytest.importorskip("openai")

budget_mod = importlib.import_module("jamjet.cloud.budget")
policy_mod = importlib.import_module("jamjet.cloud.policy")
patcher = importlib.import_module("jamjet.cloud.patcher")


@pytest.fixture
def env(monkeypatch: pytest.MonkeyPatch) -> Iterator[dict[str, Any]]:
    spans: list[dict[str, Any]] = []
    evaluator = PolicyEvaluator()
    monkeypatch.setattr(policy_mod, "_evaluator", evaluator)
    monkeypatch.setattr(patcher, "emit", spans.append)
    budget_mod.set_budget(100.0)
    patcher.patch_openai()
    patcher.patch_anthropic()
    try:
        yield {"spans": spans, "policy": evaluator}
    finally:
        patcher.unpatch_openai()
        patcher.unpatch_anthropic()
        budget_mod.set_budget(None)


def _http(respond: Callable[[], httpx.Response], sent: list[dict[str, Any]]) -> httpx.Client:
    def handler(request: httpx.Request) -> httpx.Response:
        sent.append(json.loads(request.content or b"{}"))
        return respond()

    return httpx.Client(transport=httpx.MockTransport(handler))


def _openai(respond: Callable[[], httpx.Response], sent: list[dict[str, Any]]) -> Any:
    return openai.OpenAI(api_key="x", base_url="http://vendor.test/v1", max_retries=0, http_client=_http(respond, sent))


def _anthropic(respond: Callable[[], httpx.Response], sent: list[dict[str, Any]]) -> Any:
    return anthropic.Anthropic(
        api_key="x", base_url="http://vendor.test", max_retries=0, http_client=_http(respond, sent)
    )


def _completion(name: str, *, legacy: bool = False) -> httpx.Response:
    msg: dict[str, Any] = {"role": "assistant", "content": None}
    if legacy:
        msg["function_call"] = {"name": name, "arguments": "{}"}
    else:
        msg["tool_calls"] = [{"id": "call_9", "type": "function", "function": {"name": name, "arguments": "{}"}}]
    return httpx.Response(
        200,
        json={
            "id": "c1",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": msg, "finish_reason": "tool_calls", "logprobs": None}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        },
    )


def _message(name: str) -> httpx.Response:
    return httpx.Response(
        200,
        json={
            "id": "m",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [{"type": "tool_use", "id": "tu", "name": name, "input": {}}],
            "stop_reason": "tool_use",
            "stop_sequence": None,
            "usage": {"input_tokens": 1, "output_tokens": 1},
        },
    )


def _strict(name: str) -> dict[str, Any]:
    schema = {"type": "object", "properties": {}, "required": [], "additionalProperties": False}
    return {"type": "function", "function": {"name": name, "strict": True, "parameters": schema}}


USER = [{"role": "user", "content": "x"}]


def test_chat_completions_parse_is_enforced(env: dict[str, Any]) -> None:
    env["policy"].add("block", "delete_*")
    sent: list[dict[str, Any]] = []
    client = _openai(lambda: _completion("delete_all"), sent)
    with pytest.raises(JamJetPolicyBlocked):
        client.chat.completions.parse(model="gpt-4o", messages=USER, tools=[_strict("delete_all"), _strict("search")])
    assert [t["function"]["name"] for t in sent[0]["tools"]] == ["search"]
    assert env["spans"]


def test_with_raw_response_create_is_enforced(env: dict[str, Any]) -> None:
    env["policy"].add("block", "delete_*")
    client = _openai(lambda: _completion("delete_all"), [])
    with pytest.raises(JamJetPolicyBlocked):
        client.chat.completions.with_raw_response.create(model="gpt-4o", messages=USER)


def test_with_raw_response_stream_is_enforced(env: dict[str, Any]) -> None:
    env["policy"].add("block", "delete_*")
    base = {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "gpt-4o"}
    chunks = [
        {
            **base,
            "choices": [
                {
                    "index": 0,
                    "delta": {
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "c",
                                "type": "function",
                                "function": {"name": "delete_all", "arguments": "{}"},
                            }
                        ]
                    },
                    "finish_reason": None,
                }
            ],
        },
        {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
    ]
    body = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
    client = _openai(
        lambda: httpx.Response(200, content=body.encode(), headers={"content-type": "text/event-stream"}), []
    )
    raw = client.chat.completions.with_raw_response.create(model="gpt-4o", messages=USER, stream=True)
    with pytest.raises(JamJetPolicyBlocked):
        list(raw.parse())


def test_legacy_functions_are_stripped_and_refused(env: dict[str, Any]) -> None:
    env["policy"].add("block", "delete_*")
    sent: list[dict[str, Any]] = []
    client = _openai(lambda: _completion("delete_all", legacy=True), sent)
    with pytest.raises(JamJetPolicyBlocked):
        client.chat.completions.create(
            model="gpt-4o",
            messages=USER,
            functions=[
                {"name": "delete_all", "parameters": {"type": "object"}},
                {"name": "search", "parameters": {"type": "object"}},
            ],
        )
    assert [f["name"] for f in sent[0]["functions"]] == ["search"]


def test_streamed_function_call_is_held_and_refused(env: dict[str, Any]) -> None:
    env["policy"].add("block", "delete_*")
    base = {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "gpt-4o"}
    chunks = [
        {
            **base,
            "choices": [
                {"index": 0, "delta": {"function_call": {"name": "delete_all", "arguments": ""}}, "finish_reason": None}
            ],
        },
        {
            **base,
            "choices": [
                {"index": 0, "delta": {"function_call": {"arguments": "{}"}}, "finish_reason": "function_call"}
            ],
        },
    ]
    body = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
    client = _openai(
        lambda: httpx.Response(200, content=body.encode(), headers={"content-type": "text/event-stream"}), []
    )
    seen = []
    with pytest.raises(JamJetPolicyBlocked):
        for chunk in client.chat.completions.create(model="gpt-4o", messages=USER, stream=True):
            seen.append(chunk)
    assert seen == []


def test_anthropic_beta_messages_create_is_enforced(env: dict[str, Any]) -> None:
    env["policy"].add("block", "delete_*")
    sent: list[dict[str, Any]] = []
    client = _anthropic(lambda: _message("delete_all"), sent)
    tools = [
        {"name": "delete_all", "input_schema": {"type": "object"}},
        {"name": "search", "input_schema": {"type": "object"}},
    ]
    with pytest.raises(JamJetPolicyBlocked):
        client.beta.messages.create(model="claude-sonnet-4-6", max_tokens=10, messages=USER, tools=tools)
    assert [t["name"] for t in sent[0]["tools"]] == ["search"]


def test_anthropic_messages_parse_is_enforced(env: dict[str, Any]) -> None:
    env["policy"].add("block", "delete_*")
    client = _anthropic(lambda: _message("delete_all"), [])
    with pytest.raises(JamJetPolicyBlocked):
        client.messages.parse(model="claude-sonnet-4-6", max_tokens=10, messages=USER)


def test_unenforced_methods_refuse_under_restrictive_rules(env: dict[str, Any]) -> None:
    sent: list[dict[str, Any]] = []
    oai = _openai(lambda: httpx.Response(500), sent)
    ant = _anthropic(lambda: httpx.Response(500), sent)
    env["policy"].add("block", "delete_*")
    with pytest.raises(JamJetUnenforceableCall):
        oai.responses.create(model="gpt-4o", input="x")
    with pytest.raises(JamJetUnenforceableCall):
        ant.messages.stream(model="claude-sonnet-4-6", max_tokens=10, messages=USER)
    assert sent == []


def test_server_tools_and_mcp_are_stripped(env: dict[str, Any]) -> None:
    env["policy"].add("require_approval", "web_*")
    sent: list[dict[str, Any]] = []
    client = _anthropic(lambda: _message("lookup"), sent)
    client.beta.messages.create(
        model="claude-sonnet-4-6",
        max_tokens=10,
        messages=USER,
        tools=[
            {"type": "web_search_20250305", "name": "web_search"},
            {"type": "mcp_toolset", "mcp_server_name": "db"},
            {"name": "lookup", "input_schema": {"type": "object"}},
        ],
        mcp_servers=[{"type": "url", "url": "https://mcp.example", "name": "db"}],
    )
    assert [t.get("name") for t in sent[0]["tools"]] == ["lookup"]
    assert "mcp_servers" not in sent[0]


def test_parallel_tool_calls_dropped_with_the_last_tool(env: dict[str, Any]) -> None:
    env["policy"].add("block", "*")
    sent: list[dict[str, Any]] = []
    client = _openai(lambda: _completion("x"), sent)
    with pytest.raises(JamJetPolicyBlocked):
        client.chat.completions.create(model="gpt-4o", messages=USER, tools=[_strict("a")], parallel_tool_calls=False)
    assert "tools" not in sent[0] and "parallel_tool_calls" not in sent[0]
