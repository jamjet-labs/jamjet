"""Issue jamjet-cloud#14 (server side) → mirror on the SDK side.

Constructing a Span with a kind outside the cloud's events_kind_check
constraint must raise ValueError immediately, not at flush time. The
server's allowed set lives in ALLOWED_KINDS (single source of truth via
typing.get_args(EventKind)).
"""

from __future__ import annotations

from typing import get_args

import pytest

from jamjet.cloud.models import ALLOWED_KINDS, EventKind, Span


def _span(kind: str) -> Span:
    return Span(trace_id="tr_x", span_id="sp_x", kind=kind, name="probe")  # type: ignore[arg-type]


@pytest.mark.parametrize("ok_kind", list(get_args(EventKind)))
def test_long_form_kinds_construct_cleanly(ok_kind: str) -> None:
    s = _span(ok_kind)
    assert s.kind == ok_kind


@pytest.mark.parametrize("bad_kind", ["llm", "tool", "agent", "step", ""])
def test_short_form_kinds_raise_value_error_with_allowed_list(bad_kind: str) -> None:
    with pytest.raises(ValueError) as exc:
        _span(bad_kind)
    msg = str(exc.value)
    assert "Invalid event kind" in msg
    assert "llm_call" in msg, f"expected allowed list in error, got {msg!r}"


def test_allowed_kinds_matches_event_kind_literal() -> None:
    """ALLOWED_KINDS and EventKind must stay in sync — one is the runtime
    counterpart of the other. The cloud server-side constraint is the
    upstream source; this test catches local drift."""
    assert tuple(ALLOWED_KINDS) == get_args(EventKind)
