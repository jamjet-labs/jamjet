import jamjet
from jamjet.model.seam import Model
from jamjet.model.types import StreamChunk


@jamjet.tool
async def noop(x: str) -> str:
    return x


class StubBackend:
    async def complete(self, request):  # pragma: no cover - not used here
        raise AssertionError("stream should not call complete")

    async def stream(self, request):
        self.seen = request
        for text in ["Tok", "yo"]:
            yield StreamChunk(delta=text)


async def test_agent_stream_yields_token_deltas():
    backend = StubBackend()
    agent = jamjet.Agent("a", model="anthropic/claude-opus-4-8", tools=[noop], instructions="be brief")
    out = [chunk.delta async for chunk in agent.stream("hello", model=Model(backend=backend))]
    assert out == ["Tok", "yo"]
    assert backend.seen.ref.litellm_model == "anthropic/claude-opus-4-8"
    assert backend.seen.messages[0]["role"] == "system"
    assert backend.seen.messages[0]["content"] == "be brief"
