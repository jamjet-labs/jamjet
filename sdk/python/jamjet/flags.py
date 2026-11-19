"""Boolean switches in the environment, read as the runtime reads them.

The same rule as ``parse_flag`` in ``runtime/core/src/flag.rs``:

* unset is off;
* ``1``, ``true``, ``yes``, ``on`` are on and ``0``, ``false``, ``no``,
  ``off`` and the empty string are off, in any ASCII case;
* anything else raises :class:`ValueError` naming the variable, so a typo is
  an error instead of being read as one or the other.

Only space, tab, LF, FF and CR are trimmed, the five Rust's
``char::is_ascii_whitespace`` names. A bare ``str.strip()`` would also trim
U+001C-U+001F and the Unicode spaces, which the runtime does not.
``tests/fixtures/bool_flags.json`` holds the cases both sides test against.
"""

from __future__ import annotations

import os

_TRIMMED = " \t\n\f\r"
_ON = frozenset({"1", "true", "yes", "on"})
_OFF = frozenset({"0", "false", "no", "off", ""})


def parse_flag(name: str, raw: str | None) -> bool:
    """Read a switch's value. ``None`` (unset) is ``False``."""
    if raw is None:
        return False
    value = raw.strip(_TRIMMED)
    if value.isascii():
        lowered = value.lower()
        if lowered in _ON:
            return True
        if lowered in _OFF:
            return False
    raise ValueError(f"{name}={raw!r} is not a boolean; use 1/true/yes/on or 0/false/no/off")


def env_flag(name: str) -> bool:
    """:func:`parse_flag` on ``os.environ``."""
    return parse_flag(name, os.environ.get(name))
