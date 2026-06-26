import sys
import types

import pytest

from jamjet.model.litellm_backend import LiteLLMBackend
from jamjet.model.types import ModelRequest, parse_model_ref


class _Msg:
    def __init__(self, content):
        self.content = content
        self.tool_calls = None


class _Choice:
    def __init__(self, message):
        self.message = message


class _Delta:
    def __init__(self, content):
        self.content = content


class _StreamChoice:
    def __init__(self, content):
        self.delta = _Delta(content)


class _Usage:
    prompt_tokens = 12
    completion_tokens = 5


class _Resp:
    def __init__(self):
        self.choices = [_Choice(_Msg("hello from fake"))]
        self.usage = _Usage()


def _install_fake_litellm(monkeypatch, *, stream_parts=None):
    captured = {}

    async def acompletion(**kwargs):
        captured.update(kwargs)
        if kwargs.get("stream"):

            async def gen():
                for text in stream_parts or []:
                    yield types.SimpleNamespace(choices=[_StreamChoice(text)])

            return gen()
        return _Resp()

    def completion_cost(*, completion_response):
        return 0.0009

    fake = types.ModuleType("litellm")
    fake.acompletion = acompletion
    fake.completion_cost = completion_cost
    monkeypatch.setitem(sys.modules, "litellm", fake)
    return captured


def _req(model="anthropic/claude-opus-4-8", **kw):
    return ModelRequest(ref=parse_model_ref(model), messages=[{"role": "user", "content": "hi"}], **kw)


async def test_complete_returns_openai_shaped_message_with_usage(monkeypatch):
    captured = _install_fake_litellm(monkeypatch)
    resp = await LiteLLMBackend().complete(_req(max_tokens=64))
    assert resp.message.content == "hello from fake"
    assert resp.input_tokens == 12
    assert resp.output_tokens == 5
    assert resp.cost_usd == 0.0009
    assert captured["model"] == "anthropic/claude-opus-4-8"
    assert captured["max_tokens"] == 64


async def test_complete_passes_tools_through(monkeypatch):
    captured = _install_fake_litellm(monkeypatch)
    tools = [{"type": "function", "function": {"name": "x"}}]
    await LiteLLMBackend().complete(_req(tools=tools))
    assert captured["tools"] == tools


async def test_stream_yields_text_deltas(monkeypatch):
    _install_fake_litellm(monkeypatch, stream_parts=["he", "llo"])
    chunks = [c.delta async for c in LiteLLMBackend().stream(_req())]
    assert chunks == ["he", "llo"]


async def test_missing_litellm_raises_helpful_error(monkeypatch):
    monkeypatch.setitem(sys.modules, "litellm", None)
    with pytest.raises(ImportError, match="jamjet\\[model\\]"):
        await LiteLLMBackend().complete(_req())
