from jamjet.cloud.middleware import (
    PreCallMiddleware,
    CallContext,
    MiddlewareOutcome,
)


def test_protocol_and_outcome_types_importable():
    assert PreCallMiddleware is not None
    assert CallContext is not None
    assert MiddlewareOutcome.PASSTHROUGH.value == "passthrough"
    assert MiddlewareOutcome.BLOCKED.value == "blocked"
    assert MiddlewareOutcome.CACHE_HIT.value == "cache_hit"
    assert MiddlewareOutcome.FALLBACK.value == "fallback"
