"""Tool policy in the jamjet.cloud patchers, against the real openai and
anthropic clients with an httpx MockTransport in place of the network.

Covers both vendors' tool shapes, the check on tool calls the model returns,
and streaming calls (tool calls held until checked, usage recorded).
"""

from __future__ import annotations

import importlib
import json
from collections.abc import Callable, Iterator
from typing import Any

import httpx
import pytest

from jamjet.cloud.exceptions import JamJetBudgetExceeded, JamJetPolicyBlocked
from jamjet.cloud.policy import PolicyEvaluator

# Both vendor SDKs are optional extras (`pip install jamjet[cloud-all]`); CI
# installs them. Imported before conftest can stub the openai module.
anthropic = pytest.importorskip("anthropic")
openai = pytest.importorskip("openai")

# `jamjet.cloud` defines functions named `budget` and `policy`, which shadow
# the submodules on attribute access; fetch the modules themselves.
budget_mod = importlib.import_module("jamjet.cloud.budget")
policy_mod = importlib.import_module("jamjet.cloud.policy")
patcher = importlib.import_module("jamjet.cloud.patcher")

Handler = Callable[[dict[str, Any]], httpx.Response]


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


def _transport(handler: Handler, sent: list[dict[str, Any]]) -> httpx.Client:
    def respond(request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content or b"{}")
        sent.append(body)
        return handler(body)

    return httpx.Client(transport=httpx.MockTransport(respond))


def _anthropic(handler: Handler, sent: list[dict[str, Any]]) -> anthropic.Anthropic:
    return anthropic.Anthropic(
        api_key="x", base_url="http://test", max_retries=0, http_client=_transport(handler, sent)
    )


def _openai(handler: Handler, sent: list[dict[str, Any]]) -> openai.OpenAI:
    return openai.OpenAI(api_key="x", base_url="http://test/v1", max_retries=0, http_client=_transport(handler, sent))


def _sse(events: list[tuple[str | None, Any]]) -> httpx.Response:
    text = "".join(
        (f"event: {name}\n" if name else "") + f"data: {data if isinstance(data, str) else json.dumps(data)}\n\n"
        for name, data in events
    )
    return httpx.Response(200, content=text.encode(), headers={"content-type": "text/event-stream"})


def _anthropic_message(content: list[dict[str, Any]]) -> httpx.Response:
    return httpx.Response(
        200,
        json={
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": content,
            "stop_reason": "end_turn",
            "stop_sequence": None,
            "usage": {"input_tokens": 1000, "output_tokens": 1000},
        },
    )


def _openai_completion(message: dict[str, Any]) -> httpx.Response:
    return httpx.Response(
        200,
        json={
            "id": "c1",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", **message}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        },
    )


def _anthropic_stream(tool_name: str) -> httpx.Response:
    return _sse(
        [
            (
                "message_start",
                {
                    "type": "message_start",
                    "message": {
                        "id": "msg_1",
                        "type": "message",
                        "role": "assistant",
                        "model": "claude-sonnet-4-6",
                        "content": [],
                        "stop_reason": None,
                        "stop_sequence": None,
                        "usage": {"input_tokens": 2000, "output_tokens": 1},
                    },
                },
            ),
            (
                "content_block_start",
                {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
            ),
            (
                "content_block_delta",
                {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "On it."}},
            ),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            (
                "content_block_start",
                {
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {"type": "tool_use", "id": "tu_1", "name": tool_name, "input": {}},
                },
            ),
            (
                "content_block_delta",
                {
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": {"type": "input_json_delta", "partial_json": '{"amount": 1}'},
                },
            ),
            ("content_block_stop", {"type": "content_block_stop", "index": 1}),
            (
                "message_delta",
                {
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                    "usage": {"output_tokens": 1000},
                },
            ),
            ("message_stop", {"type": "message_stop"}),
        ]
    )


def _openai_stream(tool_name: str) -> httpx.Response:
    base = {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "gpt-4o"}

    def chunk(delta: dict[str, Any], finish: str | None = None) -> tuple[None, dict[str, Any]]:
        return None, {**base, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}

    return _sse(
        [
            chunk({"role": "assistant", "content": "Sending it."}),
            chunk(
                {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": tool_name, "arguments": ""},
                        }
                    ]
                }
            ),
            chunk({"tool_calls": [{"index": 0, "function": {"arguments": "{}"}}]}),
            chunk({}, "tool_calls"),
            (None, "[DONE]"),
        ]
    )


TOOLS_ANTHROPIC = [
    {"name": "search", "input_schema": {"type": "object"}},
    {"name": "wire_money", "input_schema": {"type": "object"}},
]
TOOLS_OPENAI = [
    {"type": "function", "function": {"name": "search", "parameters": {"type": "object"}}},
    {"type": "function", "function": {"name": "wire_money", "parameters": {"type": "object"}}},
]


def test_configure_succeeds_with_anthropic_installed(monkeypatch: pytest.MonkeyPatch) -> None:
    """patch_anthropic read anthropic.Anthropic.messages on the class, which is a
    cached_property, and configure() died with AttributeError."""
    import jamjet.cloud as jc

    config_mod = importlib.import_module("jamjet.cloud.config")
    monkeypatch.setattr(config_mod, "_config", config_mod.CloudConfig())
    monkeypatch.setattr(jc, "init_queue", lambda **_: None)  # no flush thread, no network
    try:
        jc.configure(api_key="jj_test", project="p", telemetry=False)
        assert "anthropic" in patcher._originals
    finally:
        patcher.unpatch_all()


def test_filter_tools_reads_anthropic_and_custom_tool_names() -> None:
    ev = PolicyEvaluator()
    ev.add("block", "wire_*")
    tools = [
        {"name": "search", "input_schema": {}},
        {"name": "wire_money", "input_schema": {}},
        {"type": "custom", "custom": {"name": "wire_cash"}},
    ]
    allowed, blocked = ev.filter_tools(tools)
    assert [t["name"] for t in allowed] == ["search"]
    assert len(blocked) == 2


def test_anthropic_strips_blocked_tool(env: dict[str, Any]) -> None:
    env["policy"].add("block", "wire_*")
    sent: list[dict[str, Any]] = []
    client = _anthropic(lambda _: _anthropic_message([{"type": "text", "text": "ok"}]), sent)
    client.messages.create(
        model="claude-sonnet-4-6",
        max_tokens=64,
        messages=[{"role": "user", "content": "hi"}],
        tools=TOOLS_ANTHROPIC,  # type: ignore[arg-type]
    )
    assert [t["name"] for t in sent[0]["tools"]] == ["search"]


def test_anthropic_refuses_blocked_tool_use_and_counts_cost(env: dict[str, Any]) -> None:
    env["policy"].add("block", "wire_*")
    client = _anthropic(
        lambda _: _anthropic_message([{"type": "tool_use", "id": "tu_1", "name": "wire_money", "input": {}}]),
        [],
    )
    with pytest.raises(JamJetPolicyBlocked):
        client.messages.create(model="claude-sonnet-4-6", max_tokens=64, messages=[{"role": "user", "content": "x"}])
    assert budget_mod.get_budget().spent > 0
    assert env["spans"][-1]["status"] == "error"
    assert env["spans"][-1]["payload"]["policy_blocked_tool_calls"] == [{"name": "wire_money", "id": "tu_1"}]


def test_openai_refuses_blocked_tool_call(env: dict[str, Any]) -> None:
    env["policy"].add("block", "wire_*")
    client = _openai(
        lambda _: _openai_completion(
            {
                "content": None,
                "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "wire_money", "arguments": "{}"}}
                ],
            }
        ),
        [],
    )
    with pytest.raises(JamJetPolicyBlocked):
        client.chat.completions.create(model="gpt-4o", messages=[{"role": "user", "content": "x"}])


def test_openai_strips_blocked_tool_and_sends_no_empty_tools(env: dict[str, Any]) -> None:
    env["policy"].add("block", "wire_*")
    sent: list[dict[str, Any]] = []
    client = _openai(lambda _: _openai_completion({"content": "ok"}), sent)
    client.chat.completions.create(
        model="gpt-4o",
        messages=[{"role": "user", "content": "x"}],
        tools=TOOLS_OPENAI,  # type: ignore[arg-type]
    )
    client.chat.completions.create(model="gpt-4o", messages=[{"role": "user", "content": "x"}])
    assert [t["function"]["name"] for t in sent[0]["tools"]] == ["search"]
    assert "tools" not in sent[1]


def test_forced_tool_choice_on_blocked_tool_is_refused(env: dict[str, Any]) -> None:
    env["policy"].add("block", "wire_*")
    sent: list[dict[str, Any]] = []
    client = _anthropic(lambda _: _anthropic_message([]), sent)
    with pytest.raises(JamJetPolicyBlocked):
        client.messages.create(
            model="claude-sonnet-4-6",
            max_tokens=64,
            messages=[{"role": "user", "content": "x"}],
            tools=TOOLS_ANTHROPIC,  # type: ignore[arg-type]
            tool_choice={"type": "tool", "name": "wire_money"},
        )
    assert sent == []


def test_openai_stream_refuses_blocked_tool_call(env: dict[str, Any]) -> None:
    env["policy"].add("block", "wire_*")
    client = _openai(lambda _: _openai_stream("wire_money"), [])
    stream = client.chat.completions.create(model="gpt-4o", stream=True, messages=[{"role": "user", "content": "pay"}])
    seen = []
    with pytest.raises(JamJetPolicyBlocked):
        for chunk in stream:
            seen.append(chunk)
    assert [c.choices[0].delta.content for c in seen] == ["Sending it."]
    assert all(not c.choices[0].delta.tool_calls for c in seen)


def test_openai_stream_releases_allowed_tool_calls(env: dict[str, Any]) -> None:
    env["policy"].add("block", "shell.*")
    client = _openai(lambda _: _openai_stream("search"), [])
    chunks = list(
        client.chat.completions.create(model="gpt-4o", stream=True, messages=[{"role": "user", "content": "x"}])
    )
    assert len(chunks) == 4
    assert chunks[1].choices[0].delta.tool_calls[0].function.name == "search"
    assert env["spans"][-1]["status"] == "ok"


def test_anthropic_stream_stops_at_blocked_tool_use(env: dict[str, Any]) -> None:
    env["policy"].add("block", "wire_*")
    client = _anthropic(lambda _: _anthropic_stream("wire_money"), [])
    stream = client.messages.create(
        model="claude-sonnet-4-6", max_tokens=64, stream=True, messages=[{"role": "user", "content": "pay"}]
    )
    seen = []
    with pytest.raises(JamJetPolicyBlocked):
        for event in stream:
            seen.append(event.type)
    assert seen == ["message_start", "content_block_start", "content_block_delta", "content_block_stop"]
    assert budget_mod.get_budget().spent > 0


def test_anthropic_stream_records_cost_and_budget_applies(env: dict[str, Any]) -> None:
    budget_mod.set_budget(0.02)
    client = _anthropic(lambda _: _anthropic_stream("search"), [])
    events = list(
        client.messages.create(
            model="claude-sonnet-4-6", max_tokens=64, stream=True, messages=[{"role": "user", "content": "x"}]
        )
    )
    assert events[-1].type == "message_stop"
    # 2000 in * $3/MTok + 1000 out * $15/MTok
    assert budget_mod.get_budget().spent == pytest.approx(0.021)
    assert env["spans"][-1]["input_tokens"] == 2000
    assert env["spans"][-1]["output_tokens"] == 1000
    with pytest.raises(JamJetBudgetExceeded):
        client.messages.create(
            model="claude-sonnet-4-6", max_tokens=64, stream=True, messages=[{"role": "user", "content": "x"}]
        )


def test_tool_name_split_across_deltas_is_checked_joined(env: dict[str, Any]) -> None:
    env["policy"].add("block", "wire_money")
    base = {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "gpt-4o"}

    def delta(d: dict[str, Any], finish: str | None = None) -> tuple[None, dict[str, Any]]:
        return None, {**base, "choices": [{"index": 0, "delta": d, "finish_reason": finish}]}

    body = _sse(
        [
            delta(
                {
                    "tool_calls": [
                        {"index": 0, "id": "c1", "type": "function", "function": {"name": "wire_", "arguments": ""}}
                    ]
                }
            ),
            delta({"tool_calls": [{"index": 0, "function": {"name": "money", "arguments": "{}"}}]}),
            delta({}, "tool_calls"),
            (None, "[DONE]"),
        ]
    )
    client = _openai(lambda _: body, [])
    stream = client.chat.completions.create(model="gpt-4o", stream=True, messages=[{"role": "user", "content": "x"}])
    with pytest.raises(JamJetPolicyBlocked):
        list(stream)


def test_stream_failure_still_records_usage(env: dict[str, Any]) -> None:
    def broken(_self: Any, **_kwargs: Any) -> Iterator[dict[str, Any]]:
        yield {
            "type": "message_start",
            "message": {"model": "claude-sonnet-4-6", "usage": {"input_tokens": 4000, "output_tokens": 1}},
        }
        raise ConnectionError("connection reset")

    stream = patcher._run_enforced(
        "anthropic", broken, None, (), {"model": "claude-sonnet-4-6", "stream": True, "messages": []}
    )
    with pytest.raises(ConnectionError):
        list(stream)
    assert budget_mod.get_budget().spent > 0
    assert env["spans"][-1]["status"] == "error"
    assert env["spans"][-1]["input_tokens"] == 4000


def test_closing_a_stream_early_records_it(env: dict[str, Any]) -> None:
    client = _anthropic(lambda _: _anthropic_stream("search"), [])
    stream = client.messages.create(
        model="claude-sonnet-4-6", max_tokens=64, stream=True, messages=[{"role": "user", "content": "x"}]
    )
    next(iter(stream))
    stream.close()
    assert env["spans"][-1]["payload"]["stream_incomplete"] is True
    assert budget_mod.get_budget().spent > 0


def test_tools_passed_as_a_generator_are_kept(env: dict[str, Any]) -> None:
    env["policy"].add("block", "shell.*")
    sent: list[dict[str, Any]] = []
    client = _openai(lambda _: _openai_completion({"content": "ok"}), sent)
    client.chat.completions.create(
        model="gpt-4o",
        messages=[{"role": "user", "content": "x"}],
        tools=(t for t in TOOLS_OPENAI),  # type: ignore[arg-type]
    )
    assert [t["function"]["name"] for t in sent[0]["tools"]] == ["search", "wire_money"]
