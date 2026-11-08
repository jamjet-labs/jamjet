"""Model prices for cost estimates and budget enforcement.

USD per million tokens: (input, output, cache_read). Must equal
``sdk/conformance/cloud-sdk-vectors.json`` ``prices.models`` (the tests
compare them, and the TypeScript SDK's table, entry by entry); that file also
records where the prices come from. ``cache_read`` is None where no price is
published.
"""

from __future__ import annotations

import re

PRICES_PER_MTOK: dict[str, tuple[float, float, float | None]] = {
    "gpt-4o": (2.5, 10.0, None),
    "gpt-4o-2024-05-13": (5.0, 15.0, None),
    "gpt-4o-mini": (0.15, 0.6, None),
    "gpt-4-turbo": (10.0, 30.0, None),
    "gpt-4": (30.0, 60.0, None),
    "gpt-3.5-turbo": (0.5, 1.5, None),
    "gpt-5": (1.25, 10.0, None),
    "gpt-5-mini": (0.25, 2.0, None),
    "gpt-5-nano": (0.05, 0.4, None),
    "gpt-4.1": (2.0, 8.0, None),
    "gpt-4.1-mini": (0.4, 1.6, None),
    "o3": (2.0, 8.0, None),
    "o3-pro": (20.0, 80.0, None),
    "claude-fable-5-1": (10.0, 50.0, 0.25),
    "claude-fable-5": (10.0, 50.0, None),
    "claude-opus-5-5": (4.0, 20.0, 0.2),
    "claude-opus-5": (5.0, 25.0, None),
    "claude-opus-4-8": (5.0, 25.0, None),
    "claude-opus-4-7": (5.0, 25.0, 0.5),
    "claude-opus-4-6": (5.0, 25.0, 0.5),
    "claude-opus-4-5": (5.0, 25.0, 0.5),
    "claude-opus-4-20250514": (15.0, 75.0, None),
    "claude-sonnet-5-5": (2.0, 10.0, 0.2),
    "claude-sonnet-5": (2.0, 10.0, None),
    "claude-sonnet-4-6": (3.0, 15.0, 0.3),
    "claude-sonnet-4-20250514": (3.0, 15.0, None),
    "claude-haiku-4-5": (1.0, 5.0, 0.1),
    "claude-haiku-4-5-20251001": (1.0, 5.0, 0.1),
    "claude-3-5-haiku-20241022": (0.8, 4.0, None),
    "claude-3-haiku-20240307": (0.25, 1.25, None),
}

# Rate for a model missing from the table.
FALLBACK_PER_MTOK: tuple[float, float] = (3.0, 15.0)


def normalize_model(model: str) -> str:
    """The table key for a model ID: the ID itself when listed, else the ID with
    a Bedrock cross-region prefix (us., eu., ...), a provider prefix, a Bedrock
    version (-v1:0), a Vertex @YYYYMMDD, an Anthropic -YYYYMMDD and an OpenAI
    -YYYY-MM-DD or -MMDD date removed. Each step is checked against the table,
    so a dated key that is listed keeps its own price."""
    m = model.strip()
    for step in (
        r"^(us|eu|apac|global|jp|au|ca)\.",
        r"^(openai|anthropic)[/.]",
        r"-v\d+(:\d+)?$",
        r"@\d{8}$",
        r"-\d{4}-\d{2}-\d{2}$",
        r"-\d{8}$",
        r"-\d{4}$",
    ):
        if m in PRICES_PER_MTOK:
            return m
        m = re.sub(step, "", m)
    return m


def _prices_for(model: str) -> tuple[float, float, float | None] | None:
    return PRICES_PER_MTOK.get(model) or PRICES_PER_MTOK.get(normalize_model(model))


def estimate_cost(
    model: str,
    input_tokens: int,
    output_tokens: int,
    *,
    cache_read: int = 0,
    cache_write: int = 0,
    cache_write_1h: int = 0,
) -> float:
    """Estimated USD for a call. ``cache_read`` / ``cache_write`` /
    ``cache_write_1h`` are Anthropic's prompt-cache tokens, billed on top of
    ``input_tokens``: reads at the listed cache-read price (0.1x input when none
    is listed), 5-minute writes at 1.25x input, 1-hour writes at 2x."""
    prices = _prices_for(model)
    input_rate, output_rate = (prices[0], prices[1]) if prices else FALLBACK_PER_MTOK
    read_rate = prices[2] if prices and prices[2] is not None else input_rate * 0.1
    cached = cache_read * read_rate + cache_write * input_rate * 1.25 + cache_write_1h * input_rate * 2
    return (input_tokens * input_rate + output_tokens * output_rate + cached) / 1_000_000


def cache_read_savings_usd(model: str, cache_read_tokens: int) -> float:
    """USD saved by reading tokens from the prompt cache instead of as fresh input."""
    prices = _prices_for(model)
    if prices is None or prices[2] is None:
        return 0.0
    return cache_read_tokens * (prices[0] - prices[2]) / 1_000_000
