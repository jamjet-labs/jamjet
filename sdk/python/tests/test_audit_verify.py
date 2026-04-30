"""Unit tests for jamjet.cloud.audit_verify."""
from __future__ import annotations

import base64
import hashlib
import json

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


# --- 4.E.β cross-check tests ---------------------------------------------
from jamjet.cloud.audit_verify import verify_from_files  # noqa: E402


def _make_signed_bundle(tmp_path):
    """Helper: build a signed bundle + metadata + return (paths, public-key-bytes, digest_hex)."""
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat
    bundle = b'{"project":{"id":"p1","name":"t"},"warnings":[]}\n'
    sk = Ed25519PrivateKey.generate()
    pk = sk.public_key()
    digest = hashlib.sha256(bundle).digest()
    digest_hex = digest.hex()
    sig = sk.sign(digest)

    bundle_path = tmp_path / "bundle.json"
    bundle_path.write_bytes(bundle)
    metadata_path = tmp_path / "metadata.json"
    metadata_path.write_text(json.dumps({
        "id": "x",
        "sha256_digest": digest_hex,
        "signature_b64": base64.b64encode(sig).decode(),
        "signing_key_id": "k1",
    }))
    pk_bytes = (pk.public_bytes_raw() if hasattr(pk, "public_bytes_raw")
                else pk.public_bytes(encoding=Encoding.Raw, format=PublicFormat.Raw))
    return bundle_path, metadata_path, pk_bytes, digest_hex


def _patch_well_known(monkeypatch, pk_bytes):
    def fake_get(url, *a, **kw):
        class R:
            status_code = 200
            def raise_for_status(self_): pass
            def json(self_): return [{"key_id": "k1", "public_key_b64": base64.b64encode(pk_bytes).decode()}]
        return R()
    monkeypatch.setattr("httpx.get", fake_get)


def test_verify_with_pdf_metadata_match(tmp_path, monkeypatch):
    """End-to-end: signed bundle + matching PDF -> OK."""
    bundle_path, metadata_path, pk_bytes, digest_hex = _make_signed_bundle(tmp_path)
    _patch_well_known(monkeypatch, pk_bytes)

    pdf_path = tmp_path / "report.pdf"
    pdf_path.write_bytes(b"%PDF-1.4\n" + digest_hex.encode() + b"\n%%EOF\n")

    result = verify_from_files(bundle_path, metadata_path, pdf_path=pdf_path)
    assert result.ok, result.reason


def test_verify_pdf_bundle_sha256_mismatch_fails(tmp_path, monkeypatch):
    """PDF whose embedded digest does not match the canonical bundle should fail."""
    bundle_path, metadata_path, pk_bytes, _digest_hex = _make_signed_bundle(tmp_path)
    _patch_well_known(monkeypatch, pk_bytes)

    pdf_path = tmp_path / "report.pdf"
    pdf_path.write_bytes(b"%PDF-1.4\n" + (b"0" * 64) + b"\n%%EOF\n")

    result = verify_from_files(bundle_path, metadata_path, pdf_path=pdf_path)
    assert not result.ok
    assert "pdf metadata sha256" in result.reason.lower(), result.reason


def test_verify_otlp_resource_attribute_match(tmp_path, monkeypatch):
    """OTLP file with matching _jamjet_audit.bundle_sha256 should verify OK."""
    bundle_path, metadata_path, pk_bytes, digest_hex = _make_signed_bundle(tmp_path)
    _patch_well_known(monkeypatch, pk_bytes)

    otlp_path = tmp_path / "report.otlp.json"
    otlp_path.write_text(json.dumps({
        "resourceSpans": [],
        "_jamjet_audit": {"bundle_sha256": digest_hex, "scope_type": "trace", "scope_ref": "x", "generated_at": "now"},
    }))

    result = verify_from_files(bundle_path, metadata_path, otlp_path=otlp_path)
    assert result.ok, result.reason
