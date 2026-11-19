"""Switch values read the same here as in the runtime.

``runtime/core/tests/bool_flags.rs`` reads the same cases from
``fixtures/bool_flags.json``.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from jamjet.flags import parse_flag

CASES = json.loads((Path(__file__).parent / "fixtures" / "bool_flags.json").read_text())


@pytest.mark.parametrize("value", CASES["on"])
def test_on_values(value: str) -> None:
    assert parse_flag("X", value) is True


@pytest.mark.parametrize("value", CASES["off"])
def test_off_values(value: str) -> None:
    assert parse_flag("X", value) is False


def test_unset_is_off() -> None:
    assert parse_flag("X", None) is False


@pytest.mark.parametrize("value", CASES["error"])
def test_anything_else_raises_naming_the_variable(value: str) -> None:
    with pytest.raises(ValueError, match="JAMJET_SOME_SWITCH"):
        parse_flag("JAMJET_SOME_SWITCH", value)
