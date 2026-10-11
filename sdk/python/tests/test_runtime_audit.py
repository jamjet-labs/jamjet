"""Offline verification of the runtime's Ed25519-signed audit log.

The vector is written by the Rust test
``runtime/audit/tests/ed25519_signing.rs::the_cross_language_vector_is_current``
in the shape ``GET /audit`` returns, so these tests check the Python verifier
against what the runtime actually produces.
"""

from __future__ import annotations

import base64
import copy
import json
from pathlib import Path

import pytest

pytest.importorskip("cryptography")

from jamjet.runtime_audit import (  # noqa: E402
    ChainError,
    _Number,
    chain_order,
    key_id,
    load_entries,
    public_key_from_did_document,
    verify_entries,
)

VECTOR = Path(__file__).parent / "fixtures" / "runtime_audit_ed25519.json"


def _vector() -> tuple[list[dict], bytes, str]:
    text = VECTOR.read_text()
    doc = json.loads(text)
    entries = load_entries(_items_text(text))
    return entries, bytes.fromhex(doc["public_key_hex"]), doc["kid"]


def _items_text(text: str) -> str:
    # Hand the verifier the `{"items": [...]}` object as the runtime wrote it,
    # number spellings included.
    start = text.index('"audit": ') + len('"audit": ')
    depth = 0
    for i in range(start, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return text[start : i + 1]
    raise AssertionError("unbalanced vector")


def test_the_runtime_chain_verifies_with_only_the_public_key():
    entries, pk, kid = _vector()
    assert key_id(pk) == kid
    verify_entries(entries, pk)
    verify_entries(entries, pk, linked=False)


def test_a_tampered_event_fails():
    entries, pk, _ = _vector()
    bad = copy.deepcopy(entries)
    bad[1]["raw_event"]["node_id"] = "n2"
    with pytest.raises(ChainError) as e:
        verify_entries(bad, pk)
    assert (e.value.index, e.value.reason) == (1, "hash_mismatch")


def test_a_tampered_timestamp_fails():
    entries, pk, _ = _vector()
    bad = copy.deepcopy(entries)
    bad[0]["created_at"] = bad[0]["created_at"].replace(".123456789", ".123456788")
    with pytest.raises(ChainError) as e:
        verify_entries(bad, pk)
    assert e.value.reason == "hash_mismatch"


def test_a_tampered_signature_fails():
    entries, pk, _ = _vector()
    bad = copy.deepcopy(entries)
    sig = bad[2]["signature"]
    bad[2]["signature"] = sig[:-2] + ("00" if sig[-2:] != "00" else "11")
    with pytest.raises(ChainError) as e:
        verify_entries(bad, pk)
    assert (e.value.index, e.value.reason) == (2, "bad_signature")


def test_a_wrong_key_fails():
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    entries, _, _ = _vector()
    other = (
        Ed25519PrivateKey.generate()
        .public_key()
        .public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    )
    with pytest.raises(ChainError) as e:
        verify_entries(entries, other)
    assert e.value.reason == "wrong_key"


def test_a_signature_relabelled_with_the_right_kid_still_fails():
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    entries, _, _ = _vector()
    other = (
        Ed25519PrivateKey.generate()
        .public_key()
        .public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    )
    bad = copy.deepcopy(entries)
    for e in bad:
        e["signature"] = f"ed25519:{key_id(other)}:" + e["signature"].rsplit(":", 1)[1]
    with pytest.raises(ChainError) as e:
        verify_entries(bad, other)
    assert e.value.reason == "bad_signature"


def test_an_hmac_tag_is_not_accepted():
    entries, pk, _ = _vector()
    bad = copy.deepcopy(entries)
    bad[0]["signature"] = "ab" * 32
    with pytest.raises(ChainError) as e:
        verify_entries(bad, pk)
    assert e.value.reason == "bad_signature"


def test_an_unsigned_entry_fails():
    entries, pk, _ = _vector()
    bad = copy.deepcopy(entries)
    del bad[0]["signature"]
    with pytest.raises(ChainError) as e:
        verify_entries(bad, pk)
    assert e.value.reason == "unsigned"


def test_a_dropped_entry_breaks_the_link():
    entries, pk, _ = _vector()
    with pytest.raises(ChainError) as e:
        verify_entries([entries[0], entries[2]], pk)
    assert (e.value.index, e.value.reason) == (1, "broken_link")
    # Each entry still stands on its own.
    verify_entries([entries[0], entries[2]], pk, linked=False)


def test_chain_order_restores_order_and_refuses_a_gap():
    entries, pk, _ = _vector()
    shuffled = [entries[2], entries[0], entries[1]]
    verify_entries(chain_order(shuffled), pk)
    with pytest.raises(ChainError):
        chain_order([entries[0], entries[2]])


def test_floats_parsed_as_python_floats_are_refused_not_misread():
    _, pk, _ = _vector()
    reparsed = json.loads(_items_text(VECTOR.read_text()))["items"]
    with pytest.raises(ChainError) as e:
        verify_entries(reparsed, pk)
    assert e.value.reason == "malformed"


def test_the_did_document_key_is_read():
    _, pk, kid = _vector()
    x = base64.urlsafe_b64encode(pk).rstrip(b"=").decode()
    doc = {
        "id": "did:web:localhost%3A7700",
        "verificationMethod": [
            {
                "id": "did:web:localhost%3A7700#audit-key",
                "type": "JsonWebKey2020",
                "publicKeyJwk": {"kty": "OKP", "crv": "Ed25519", "x": x, "kid": kid},
            }
        ],
    }
    assert public_key_from_did_document(doc) == pk
    with pytest.raises(ValueError):
        public_key_from_did_document({"id": "did:web:x"})


def test_a_valid_signature_without_its_prefix_is_refused():
    entries, pk, _ = _vector()
    bad = copy.deepcopy(entries)
    bad[0]["signature"] = bad[0]["signature"][len("ed25519:") :]
    with pytest.raises(ChainError) as e:
        verify_entries(bad, pk)
    assert e.value.reason == "bad_signature"


_SEED = bytes.fromhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
_L = 2**252 + 27742317777372353535851937790883648493
_IDENTITY = bytes([1]) + bytes(31)


def _small_order_r_signature(entry_hash: str, kid: str, pk: bytes) -> str:
    """S = k*a with R the identity: valid under the plain Ed25519 equation,
    made with the vector's private key (review I3/T1)."""
    import hashlib

    from jamjet.runtime_audit import signing_statement

    h = bytearray(hashlib.sha512(_SEED).digest()[:32])
    h[0] &= 248
    h[31] &= 127
    h[31] |= 64
    a = int.from_bytes(h, "little")
    msg = signing_statement(entry_hash, kid)
    k = int.from_bytes(hashlib.sha512(_IDENTITY + pk + msg).digest(), "little") % _L
    s = (k * a) % _L
    return f"ed25519:{kid}:" + (_IDENTITY + s.to_bytes(32, "little")).hex()


def test_a_small_order_r_is_refused():
    entries, pk, kid = _vector()
    bad = copy.deepcopy(entries)
    bad[0]["signature"] = _small_order_r_signature(bad[0]["entry_hash"], kid, pk)
    with pytest.raises(ChainError) as e:
        verify_entries(bad, pk)
    assert e.value.reason == "bad_signature"


def test_a_small_order_public_key_is_refused():
    entries, _, _ = _vector()
    bad = copy.deepcopy(entries)
    forged = f"ed25519:{key_id(_IDENTITY)}:" + (_IDENTITY + bytes(32)).hex()
    for e in bad:
        e["signature"] = forged
    with pytest.raises(ValueError):
        verify_entries(bad, _IDENTITY)
    x = base64.urlsafe_b64encode(_IDENTITY).rstrip(b"=").decode()
    doc = {"verificationMethod": [{"id": "d#audit-key", "publicKeyJwk": {"kty": "OKP", "crv": "Ed25519", "x": x}}]}
    with pytest.raises(ValueError):
        public_key_from_did_document(doc)


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("sequence", "1"),
        ("sequence", True),
        ("redacted", 0),
        ("redacted", []),
        ("tenant_id", None),
        ("id", "urn:uuid:A1A1A1A1-A1A1-A1A1-A1A1-A1A1A1A1A1A1"),
        ("id", "A1A1A1A1-A1A1-A1A1-A1A1-A1A1A1A1A1A1"),
        ("event_id", "{01010101010101010101010101010101}"),
    ],
)
def test_a_retyped_field_is_malformed(field, value):
    """Rust would not read these; a verified entry must be one it would
    (review M3)."""
    entries, pk, _ = _vector()
    bad = copy.deepcopy(entries)
    bad[0][field] = value
    with pytest.raises(ChainError) as e:
        verify_entries(bad, pk)
    assert e.value.reason == "malformed"


@pytest.mark.parametrize("mangle", [str.upper, lambda h: h[:10] + " " + h[10:]])
def test_signature_hex_must_be_canonical(mangle):
    entries, pk, _ = _vector()
    bad = copy.deepcopy(entries)
    prefix, _, hexsig = bad[0]["signature"].rpartition(":")
    bad[0]["signature"] = f"{prefix}:{mangle(hexsig)}"
    with pytest.raises(ChainError) as e:
        verify_entries(bad, pk)
    assert e.value.reason == "bad_signature"


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("sequence", _Number("99999999999999999999")),
        ("sequence", _Number("-9223372036854775809")),
        ("prev_hash", ["not", "a", "string"]),
        ("signature", {"not": "a string"}),
        ("signature", 7),
        ("entry_hash", ["not a string"]),
    ],
)
def test_an_untyped_or_out_of_range_field_is_malformed_not_a_crash(field, value):
    """The export is untrusted input: every bad entry is a ChainError, never a
    raw exception a caller catching ChainError would not expect."""
    entries, pk, _ = _vector()
    bad = copy.deepcopy(entries)
    bad[0][field] = value
    with pytest.raises(ChainError) as e:
        verify_entries(bad, pk)
    assert e.value.reason == "malformed"
