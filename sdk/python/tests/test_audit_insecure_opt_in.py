"""``JAMJET_AUDIT_ALLOW_INSECURE_KEY`` is a yes/no flag, read the same way as
the runtime reads it (``jamjet.flags.parse_flag``, ``runtime/core/src/flag.rs``).

Any non-empty value used to opt in, so ``=0`` signed the audit log with the
built-in key that everyone knows.
"""

from __future__ import annotations

import warnings

import pytest

from jamjet.agents.audit import insecure_key_opt_in, resolve_signer


@pytest.mark.parametrize("value", ["1", "true", "TRUE", "yes", "on", " 1 "])
def test_yes_opts_in(value: str) -> None:
    assert insecure_key_opt_in(value)


@pytest.mark.parametrize("value", ["0", "false", "no", "off", "", None])
def test_no_does_not(value: str | None) -> None:
    assert not insecure_key_opt_in(value)


@pytest.mark.parametrize("value", ["maybe", "\x1c1", "2"])
def test_anything_else_is_an_error(value: str) -> None:
    with pytest.raises(ValueError, match="JAMJET_AUDIT_ALLOW_INSECURE_KEY"):
        insecure_key_opt_in(value)


def test_a_typo_stops_signing_instead_of_guessing(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("JAMJET_AUDIT_SIGNING_KEY", raising=False)
    monkeypatch.setenv("JAMJET_AUDIT_ALLOW_INSECURE_KEY", "yes please")
    with pytest.raises(ValueError, match="JAMJET_AUDIT_ALLOW_INSECURE_KEY"):
        resolve_signer()


def test_zero_leaves_the_log_unsigned(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("JAMJET_AUDIT_SIGNING_KEY", raising=False)
    monkeypatch.setenv("JAMJET_AUDIT_ALLOW_INSECURE_KEY", "0")
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        signer = resolve_signer()
    assert not signer.is_dev_key
