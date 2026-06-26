import pytest

from jamjet.model.middleware import (
    BaseModelMiddleware,
    ModelAllowlistMiddleware,
    ModelDeniedError,
)
from jamjet.model.types import ModelRequest, parse_model_ref


def _req(model: str) -> ModelRequest:
    return ModelRequest(ref=parse_model_ref(model), messages=[{"role": "user", "content": "hi"}])


async def test_base_middleware_is_passthrough():
    mw = BaseModelMiddleware()
    req = _req("openai/gpt-4o")
    assert await mw.before(req) is req


async def test_allowlist_none_allows_everything():
    mw = ModelAllowlistMiddleware(None)
    req = _req("anthropic/claude-opus-4-8")
    assert await mw.before(req) is req


async def test_allowlist_allows_by_provider():
    mw = ModelAllowlistMiddleware({"anthropic"})
    req = _req("anthropic/claude-opus-4-8")
    assert await mw.before(req) is req


async def test_allowlist_allows_by_full_model():
    mw = ModelAllowlistMiddleware({"anthropic/claude-opus-4-8"})
    assert await mw.before(_req("anthropic/claude-opus-4-8"))


async def test_allowlist_denies_unlisted_model():
    mw = ModelAllowlistMiddleware({"anthropic"})
    with pytest.raises(ModelDeniedError) as exc:
        await mw.before(_req("openai/gpt-4o"))
    assert exc.value.code == "model_not_allowed"
