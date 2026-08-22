"""The model allowlist must mean the same thing in-process and durable.

The SAME allowlist is evaluated by `ModelAllowlistMiddleware` for an in-process
run and by `PolicyEvaluator` (runtime/policy/src/lib.rs) for a durable one. When
they disagree, a policy means different things depending on which transport
happened to run it — and because development usually stays in-process, the
disagreement surfaces in production.

Two divergences existed, in opposite directions:

  * `["anthropic"]` — the built-in "strict" policy's entire allowlist — was
    allowed here and BLOCKED durable-side, because the Rust matcher globbed the
    full `provider/model` ref and a wildcard-free entry compared literally.
  * `["anthropic/*"]` and `["*"]` were allowed durable-side and BLOCKED here,
    because membership was exact and did not glob at all. `["*"]` — the natural
    way to write "allow everything" — denied every model.

The table below is the contract. Keep it in step with
`runtime/policy/src/lib.rs`; a row that changes on one side and not the other is
the bug this file exists to catch.
"""

from __future__ import annotations

import asyncio

import pytest

from jamjet.model.middleware import ModelAllowlistMiddleware, model_allowlist_matches
from jamjet.model.types import ModelRef, ModelRequest

OPUS = ModelRef(
    provider="anthropic",
    model="claude-opus-4-8",
    litellm_model="anthropic/claude-opus-4-8",
)

# (allowlist entry, model ref, allowed?) — mirrored by the Rust tests.
PARITY_TABLE = [
    # A provider-only entry admits that provider's whole catalogue.
    ("anthropic", OPUS, True),
    # ...and still denies another provider. The provider rule must not widen
    # into an allow-all.
    ("openai", OPUS, False),
    # An explicit provider glob.
    ("anthropic/*", OPUS, True),
    ("openai/*", OPUS, False),
    # A full ref pins exactly that model — pinning one must NOT silently admit
    # the provider's whole catalogue.
    ("anthropic/claude-opus-4-8", OPUS, True),
    ("anthropic/claude-haiku-4-5", OPUS, False),
    # The natural "allow everything".
    ("*", OPUS, True),
    # `?` matches exactly one character.
    ("anthropi?", OPUS, True),
    ("anthropi??", OPUS, False),
]


@pytest.mark.parametrize(("pattern", "ref", "expected"), PARITY_TABLE)
def test_matcher_parity(pattern: str, ref: ModelRef, expected: bool) -> None:
    assert model_allowlist_matches(pattern, ref.provider, ref.litellm_model) is expected, (
        f"{pattern!r} vs {ref.litellm_model!r} must be {expected}"
    )


@pytest.mark.parametrize(("pattern", "ref", "expected"), PARITY_TABLE)
def test_middleware_enforces_the_same_table(pattern: str, ref: ModelRef, expected: bool) -> None:
    """The matcher is only useful if the middleware actually applies it."""
    mw = ModelAllowlistMiddleware({pattern})

    async def call() -> bool:
        try:
            await mw.before(ModelRequest(ref=ref, messages=[]))
            return True
        except Exception:
            return False

    assert asyncio.run(call()) is expected


def test_a_character_class_is_literal_not_a_class() -> None:
    """`[seq]` must NOT be honoured — the Rust matcher treats it literally.

    This is why the matcher is hand-written rather than `fnmatch`, which would
    fix one divergence by quietly introducing another.
    """
    assert not model_allowlist_matches("anthropic/[abc]laude-opus-4-8", "anthropic", "anthropic/claude-opus-4-8")


def test_an_empty_allowlist_denies() -> None:
    """An empty set is not the same as `None`.

    `None` means "no allowlist configured, allow everything"; an empty set means
    a policy was resolved and admits nothing.
    """
    mw = ModelAllowlistMiddleware(set())

    async def call() -> bool:
        try:
            await mw.before(ModelRequest(ref=OPUS, messages=[]))
            return True
        except Exception:
            return False

    assert asyncio.run(call()) is False


def test_none_allows_everything() -> None:
    mw = ModelAllowlistMiddleware(None)

    async def call() -> bool:
        await mw.before(ModelRequest(ref=OPUS, messages=[]))
        return True

    assert asyncio.run(call()) is True
