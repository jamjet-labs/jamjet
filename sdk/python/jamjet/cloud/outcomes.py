"""Record trace outcomes against the JamJet Cloud API."""

from __future__ import annotations

from typing import Any

import httpx

VALID_OUTCOMES = frozenset(
    {"success", "failure", "approved", "rejected", "resolved", "unresolved"}
)


def record_outcome(
    api_key: str,
    api_url: str,
    trace_id: str,
    outcome: str,
    score: float | None = None,
    metadata: dict[str, Any] | None = None,
) -> None:
    """POST an outcome record to ``/v1/outcomes``.

    Args:
        api_key:  Bearer token for the JamJet Cloud API.
        api_url:  Base URL, e.g. ``https://api.jamjet.dev``.
        trace_id: ID of the trace whose outcome is being recorded.
        outcome:  One of ``success``, ``failure``, ``approved``, ``rejected``,
                  ``resolved``, or ``unresolved``.
        score:    Optional float in [0, 1] representing a quality score.
        metadata: Optional free-form dict attached to the record.

    Raises:
        ValueError: When *outcome* is not one of the six valid values or
                    *score* is outside the [0, 1] range.
        httpx.HTTPStatusError: When the API returns a non-2xx response.
    """
    if outcome not in VALID_OUTCOMES:
        raise ValueError(
            f"Invalid outcome {outcome!r}. Must be one of: "
            + ", ".join(sorted(VALID_OUTCOMES))
            + "."
        )
    if score is not None:
        if not isinstance(score, (int, float)) or score < 0 or score > 1:
            raise ValueError(
                f"score must be a number between 0 and 1 (inclusive), got {score!r}."
            )

    payload: dict[str, Any] = {"trace_id": trace_id, "outcome": outcome}
    if score is not None:
        payload["score"] = float(score)
    if metadata is not None:
        payload["metadata"] = metadata

    resp = httpx.post(
        f"{api_url.rstrip('/')}/v1/outcomes",
        json=payload,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
        timeout=10,
    )
    resp.raise_for_status()
