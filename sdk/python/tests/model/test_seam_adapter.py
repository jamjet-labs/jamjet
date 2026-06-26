from jamjet.model.seam import Model
from jamjet.model.types import ModelResponse, StreamChunk
from jamjet.runtime.local.llm_adapters import get_adapter
from jamjet.runtime.local.llm_adapters.base import LLMAdapter
from jamjet.runtime.local.llm_adapters.seam_adapter import SeamAdapter
from jamjet.spec import LLMConfig


class _Msg:
    content = "ok"
    tool_calls = None


class StubBackend:
    def __init__(self):
        self.calls: list = []

    async def complete(self, request):
        self.calls.append(request)
        return ModelResponse(message=_Msg(), input_tokens=3, output_tokens=4)

    async def stream(self, request):  # pragma: no cover - not used here
        yield StreamChunk(delta="x")


def test_get_adapter_returns_seam_adapter_for_any_provider():
    cfg = LLMConfig(provider="anthropic", model="anthropic/claude-opus-4-8")
    adapter = get_adapter(cfg)
    assert isinstance(adapter, SeamAdapter)
    assert isinstance(adapter, LLMAdapter)  # runtime_checkable Protocol


async def test_generate_returns_openai_shaped_message():
    backend = StubBackend()
    cfg = LLMConfig(provider="anthropic", model="anthropic/claude-opus-4-8", max_tokens=32)
    adapter = SeamAdapter(cfg, model=Model(backend=backend))
    msg = await adapter.generate([{"role": "user", "content": "hi"}], tools=None)
    assert msg.content == "ok"
    assert hasattr(msg, "tool_calls")
    sent = backend.calls[0]
    assert sent.ref.litellm_model == "anthropic/claude-opus-4-8"
    assert sent.max_tokens == 32
