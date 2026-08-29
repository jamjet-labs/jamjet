import pytest

from jamjet.model.middleware import BaseModelMiddleware, ModelDeniedError
from jamjet.model.seam import Model
from jamjet.model.types import ModelRequest, ModelResponse, StreamChunk, parse_model_ref


class FakeBackend:
    def __init__(self):
        self.completed: list[ModelRequest] = []
        # Snapshot the payload AS THE PROVIDER SAW IT. Appending the request
        # alone stores it by reference, and PII redaction mutates messages in
        # place, so a later read would show post-call state and could not tell
        # "redacted before the call" from "redacted after it".
        self.sent_text: list[str] = []

    async def complete(self, request):
        self.completed.append(request)
        self.sent_text.append(str(request.messages))
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


# --- Model() defaults to the governed chain -------------------------------
#
# A bare ``Model()`` used to build an EMPTY middleware chain, so it reached the
# provider with no PII redaction, no metering and no budget. The seam's own
# docstring calls it "the single governed path for every model call"; these
# tests hold it to that, and keep the escape hatch explicit.

_EMAIL = "alice@example.com"


def _pii_req():
    return ModelRequest(
        ref=parse_model_ref("anthropic/claude-opus-4-8"),
        messages=[{"role": "user", "content": f"email me at {_EMAIL}"}],
    )


def _sent_text(backend):
    return backend.sent_text[0]


async def test_bare_model_redacts_pii_before_the_backend_sees_it():
    backend = FakeBackend()
    await Model(backend=backend).complete(_pii_req())
    assert _EMAIL not in _sent_text(backend)
    assert "[REDACTED:EMAIL]" in _sent_text(backend)


async def test_bare_model_meters_the_completion():
    from jamjet.model.metering import MeteringMiddleware

    model = Model(backend=FakeBackend())
    await model.complete(_pii_req())
    meters = [mw for mw in model._middleware if isinstance(mw, MeteringMiddleware)]
    assert len(meters) == 1
    assert [(r.provider, r.input_tokens, r.output_tokens) for r in meters[0].records] == [("anthropic", 1, 2)]


async def test_explicit_middleware_is_not_silently_wrapped_in_the_default_chain():
    backend = FakeBackend()
    log: list[str] = []
    model = Model(middleware=[RecordingMiddleware(log, "1")], backend=backend)
    await model.complete(_pii_req())
    assert log == ["before:1", "after:1"]
    assert _EMAIL in _sent_text(backend)  # caller's chain, verbatim, nothing appended


async def test_empty_middleware_list_is_rejected():
    with pytest.raises(ValueError) as exc:
        Model(middleware=[], backend=FakeBackend())
    assert "ungoverned=True" in str(exc.value)


async def test_ungoverned_gives_an_empty_chain():
    backend = FakeBackend()
    model = Model(backend=backend, ungoverned=True)
    assert model._middleware == []
    await model.complete(_pii_req())
    assert _EMAIL in _sent_text(backend)  # the escape hatch really does bypass


async def test_ungoverned_with_explicit_middleware_is_rejected():
    with pytest.raises(ValueError):
        Model(middleware=[DenyMiddleware()], backend=FakeBackend(), ungoverned=True)


async def test_empty_generator_middleware_is_rejected_too():
    # An exhausted iterator is truthy, so an emptiness check on the argument
    # instead of the materialized list lets `(mw for mw in [])` through as a
    # silent ungoverned chain.
    with pytest.raises(ValueError) as exc:
        Model(middleware=(mw for mw in []), backend=FakeBackend())  # type: ignore[arg-type]
    assert "ungoverned=True" in str(exc.value)


async def test_non_middleware_chain_elements_are_rejected_at_construction():
    # Model(middleware="pii") used to iterate the string into ['p','i','i'] and
    # Model(middleware=[object()]) used to construct fine, both failing later
    # with a bare AttributeError at the provider boundary instead of here.
    for bad in ("pii", [object()], [DenyMiddleware(), object()]):
        with pytest.raises(TypeError) as exc:
            Model(middleware=bad, backend=FakeBackend())  # type: ignore[arg-type]
        assert "ModelMiddleware" in str(exc.value)


async def test_a_degenerate_default_chain_is_rejected_too(monkeypatch):
    # The emptiness guard used to live only on the caller-supplied branch, so a
    # default_model_middleware() that returned nothing produced a silent
    # ungoverned Model() with no error. In-tree it always returns three, so
    # this is defense in depth against a shadowed or patched symbol.
    import jamjet.model.defaults as defaults

    monkeypatch.setattr(defaults, "default_model_middleware", lambda *a, **k: [])
    with pytest.raises(ValueError):
        Model(backend=FakeBackend())
