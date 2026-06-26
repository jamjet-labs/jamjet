import pytest

from jamjet.model.middleware import BaseModelMiddleware, ModelDeniedError
from jamjet.model.seam import Model
from jamjet.model.types import ModelRequest, ModelResponse, StreamChunk, parse_model_ref


class FakeBackend:
    def __init__(self):
        self.completed: list[ModelRequest] = []

    async def complete(self, request):
        self.completed.append(request)
        return ModelResponse(message=object(), input_tokens=1, output_tokens=2)

    async def stream(self, request):
        for text in ["a", "b"]:
            yield StreamChunk(delta=text)


class RecordingMiddleware(BaseModelMiddleware):
    def __init__(self, log, name):
        self._log = log
        self._name = name

    async def before(self, request):
        self._log.append(f"before:{self._name}")
        return request

    async def after(self, request, response):
        self._log.append(f"after:{self._name}")
        return response


class DenyMiddleware(BaseModelMiddleware):
    async def before(self, request):
        raise ModelDeniedError("nope", code="blocked")


def _req():
    return ModelRequest(ref=parse_model_ref("anthropic/claude-opus-4-8"), messages=[])


async def test_runs_before_in_order_and_after_in_reverse():
    log: list[str] = []
    backend = FakeBackend()
    model = Model(middleware=[RecordingMiddleware(log, "1"), RecordingMiddleware(log, "2")], backend=backend)
    await model.complete(_req())
    assert log == ["before:1", "before:2", "after:2", "after:1"]
    assert len(backend.completed) == 1


async def test_denied_call_never_reaches_backend():
    backend = FakeBackend()
    model = Model(middleware=[DenyMiddleware()], backend=backend)
    with pytest.raises(ModelDeniedError) as exc:
        await model.complete(_req())
    assert exc.value.code == "blocked"
    assert backend.completed == []  # the moat: denial is before the provider call


async def test_stream_runs_before_hooks_then_yields(monkeypatch):
    log: list[str] = []
    backend = FakeBackend()
    model = Model(middleware=[RecordingMiddleware(log, "1")], backend=backend)
    out = [c.delta async for c in model.stream(_req())]
    assert out == ["a", "b"]
    assert log == ["before:1"]


async def test_stream_denied_yields_nothing():
    backend = FakeBackend()
    model = Model(middleware=[DenyMiddleware()], backend=backend)
    with pytest.raises(ModelDeniedError):
        _ = [c async for c in model.stream(_req())]
