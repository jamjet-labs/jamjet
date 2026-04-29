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


def test_verify_from_files_handles_invalid_json_bundle(tmp_path):
    """Bundle that isn't JSON returns ok=False, doesn't raise."""
    from jamjet.cloud.audit_verify import verify_from_files
    bundle_path = tmp_path / "bad.json"
    bundle_path.write_bytes(b"not valid json{{")
    metadata_path = tmp_path / "metadata.json"
    metadata_path.write_text('{"signature_b64":"x","signing_key_id":"y"}')
    result = verify_from_files(bundle_path, metadata_path)
    assert result.ok is False
    assert "not valid JSON" in result.reason


def test_verify_from_files_handles_unreachable_well_known(tmp_path):
    """Failed HTTP call to well-known endpoint returns ok=False, doesn't raise."""
    from jamjet.cloud.audit_verify import verify_from_files
    bundle_path = tmp_path / "bundle.json"
    bundle_path.write_bytes(b'{"project":{"id":"abc"}}')
    metadata_path = tmp_path / "metadata.json"
    metadata_path.write_text('{"signature_b64":"AAAA","signing_key_id":"y"}')
    # Use an unroutable URL so httpx fails to connect.
    result = verify_from_files(bundle_path, metadata_path, api_url="http://127.0.0.1:1")
    assert result.ok is False
    assert "could not fetch public key" in result.reason or "fetch" in result.reason.lower()


def test_verify_from_files_handles_missing_package_file(tmp_path):
    """Nonexistent package path returns ok=False, doesn't raise."""
    from jamjet.cloud.audit_verify import verify_from_files
    metadata_path = tmp_path / "metadata.json"
    metadata_path.write_text('{"signature_b64":"x","signing_key_id":"y"}')
    result = verify_from_files(tmp_path / "no-such-file.json", metadata_path)
    assert result.ok is False
    assert "could not read package" in result.reason


def test_verify_from_files_handles_invalid_base64_signature(tmp_path, monkeypatch):
    """Invalid base64 in metadata signature returns ok=False, doesn't raise."""
    import jamjet.cloud.audit_verify as av

    bundle_path = tmp_path / "bundle.json"
    bundle_path.write_bytes(b'{"project":{"id":"abc"}}')
    metadata_path = tmp_path / "metadata.json"
    metadata_path.write_text('{"signature_b64":"!!!not-valid!!!","signing_key_id":"y"}')

    # Stub the well-known fetch so we get past the network step.
    import httpx
    def fake_get(url, params=None, timeout=None):
        class R:
            status_code = 200
            def raise_for_status(self): pass
            def json(self): return [{"key_id": "y", "public_key_b64": "AAAA"}]
        return R()
    monkeypatch.setattr(av.httpx, "get", fake_get)

    result = av.verify_from_files(bundle_path, metadata_path)
    assert result.ok is False
    assert "base64" in result.reason.lower() or "invalid" in result.reason.lower()


def test_verify_from_files_handles_non_dict_bundle(tmp_path):
    """Bundle that's a JSON array (not object) returns ok=False, doesn't raise."""
    from jamjet.cloud.audit_verify import verify_from_files
    bundle_path = tmp_path / "bundle.json"
    bundle_path.write_bytes(b'["not", "an", "object"]')
    metadata_path = tmp_path / "metadata.json"
    metadata_path.write_text('{"signature_b64":"AAAA","signing_key_id":"y"}')
    result = verify_from_files(bundle_path, metadata_path)
    assert result.ok is False
    assert "JSON object" in result.reason
