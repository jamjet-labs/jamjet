"""Unit tests for jamjet.cloud.audit_verify."""
from __future__ import annotations

import hashlib

import pytest

cryptography = pytest.importorskip("cryptography")
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey


def _sign_fixture(bundle: bytes) -> tuple[bytes, bytes]:
    """Returns (public_key_bytes, signature_bytes)."""
    sk = Ed25519PrivateKey.generate()
    pk = sk.public_key()
    digest = hashlib.sha256(bundle).digest()
    sig = sk.sign(digest)
    pk_bytes = pk.public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw,
    )
    return pk_bytes, sig


def test_verify_ok_round_trip():
    from jamjet.cloud.audit_verify import verify_package
    bundle = b'{"schema_version":"1.0","payload":"x"}\n'
    pk_bytes, sig = _sign_fixture(bundle)
    result = verify_package(bundle, sig, pk_bytes)
    assert result.ok is True
    assert result.digest == hashlib.sha256(bundle).hexdigest()


def test_verify_fails_on_tampered_bundle():
    from jamjet.cloud.audit_verify import verify_package
    bundle = b'{"schema_version":"1.0","payload":"x"}\n'
    pk_bytes, sig = _sign_fixture(bundle)
    tampered = bundle.replace(b'"x"', b'"y"')
    result = verify_package(tampered, sig, pk_bytes)
    assert result.ok is False


def test_verify_fails_on_wrong_key():
    from jamjet.cloud.audit_verify import verify_package
    bundle = b'{}\n'
    _, sig = _sign_fixture(bundle)
    other_pk_bytes, _ = _sign_fixture(b"other")
    result = verify_package(bundle, sig, other_pk_bytes)
    assert result.ok is False


def test_verify_rejects_short_signature():
    from jamjet.cloud.audit_verify import verify_package
    pk_bytes, _ = _sign_fixture(b"x")
    result = verify_package(b"x", b"\x00" * 10, pk_bytes)
    assert result.ok is False
    assert "signature" in result.reason.lower()
