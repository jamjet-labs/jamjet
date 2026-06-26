"""Tests for the default model-seam middleware factory."""

from jamjet.model import default_model_middleware
from jamjet.model.metering import MeteringMiddleware
from jamjet.model.middleware import ModelAllowlistMiddleware


def test_default_model_middleware_returns_two_elements() -> None:
    chain = default_model_middleware()
    assert len(chain) == 2


def test_default_model_middleware_types() -> None:
    chain = default_model_middleware()
    assert isinstance(chain[0], ModelAllowlistMiddleware)
    assert isinstance(chain[1], MeteringMiddleware)


def test_default_model_middleware_allowlist_is_allow_all() -> None:
    """ModelAllowlistMiddleware(None) means allow-all — the Track 1 default."""
    chain = default_model_middleware()
    allowlist_mw = chain[0]
    assert isinstance(allowlist_mw, ModelAllowlistMiddleware)
    # The internal _allowed attribute should be None (allow-all sentinel).
    assert allowlist_mw._allowed is None


def test_default_model_middleware_returns_fresh_instances() -> None:
    """Each call returns independent instances so middleware state doesn't leak."""
    a = default_model_middleware()
    b = default_model_middleware()
    assert a[0] is not b[0]
    assert a[1] is not b[1]
