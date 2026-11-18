"""Tests for versioned server binary cache logic in _find_server_binary."""

from __future__ import annotations

import os
import stat
from pathlib import Path

import pytest

from jamjet.cli.main import _find_server_binary, _server_cache_dir

# ---------------------------------------------------------------------------
# _server_cache_dir
# ---------------------------------------------------------------------------


def test_server_cache_dir_includes_sdk_version(monkeypatch: pytest.MonkeyPatch) -> None:
    """_server_cache_dir() returns ~/.jamjet/bin/<sdk_version>."""
    monkeypatch.setattr(
        "jamjet.cli.main._sdk_version",
        lambda: "0.10.2",
    )
    monkeypatch.setenv("HOME", "/fakehome")
    result = _server_cache_dir()
    # Path ends with the version segment
    assert result.endswith("0.10.2")
    assert "/.jamjet/bin/" in result


def test_server_cache_dir_falls_back_to_dev(monkeypatch: pytest.MonkeyPatch) -> None:
    """When importlib.metadata raises, _server_cache_dir uses 'dev'."""
    monkeypatch.setattr(
        "jamjet.cli.main._sdk_version",
        lambda: "dev",
    )
    result = _server_cache_dir()
    assert result.endswith("dev")


# ---------------------------------------------------------------------------
# _find_server_binary — versioned path is used, legacy flat path is skipped
# ---------------------------------------------------------------------------


def test_find_server_binary_skips_legacy_flat_binary(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """
    If ~/.jamjet/bin/jamjet-server (flat legacy) exists but the versioned path
    does NOT exist, the downloader is called instead of returning the flat path.
    """
    # Create a fake home under tmp_path so expanduser is isolated.
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    bin_dir = fake_home / ".jamjet" / "bin"
    bin_dir.mkdir(parents=True)

    # Write a flat legacy binary that should NOT be picked up.
    legacy = bin_dir / "jamjet-server"
    legacy.write_bytes(b"old-binary")
    legacy.chmod(legacy.stat().st_mode | stat.S_IEXEC)

    # Monkeypatch expanduser so ~/.jamjet resolves into fake_home.
    original_expanduser = os.path.expanduser

    def fake_expanduser(path: str) -> str:
        if path.startswith("~"):
            return str(fake_home) + path[1:]
        return original_expanduser(path)

    monkeypatch.setattr(os.path, "expanduser", fake_expanduser)

    # Pin SDK version so the versioned dir is predictable.
    monkeypatch.setattr("jamjet.cli.main._sdk_version", lambda: "0.10.2")

    # Monkeypatch shutil.which to return None (no system install).
    monkeypatch.setattr("shutil.which", lambda _name: None)

    # Capture whether _download_server_binary was called and with which dir.
    download_calls: list[str] = []

    def fake_download(cache_dir: str) -> str:
        download_calls.append(cache_dir)
        return str(bin_dir / "0.10.2" / "jamjet-server")

    monkeypatch.setattr("jamjet.cli.main._download_server_binary", fake_download)

    # Override os.getcwd so repo-relative candidates don't accidentally match.
    monkeypatch.setattr(os, "getcwd", lambda: str(tmp_path))

    _find_server_binary()

    # The downloader must have been called (flat legacy was not returned).
    assert len(download_calls) == 1, "Expected downloader to be called; flat legacy was returned instead."

    # The cache_dir passed to the downloader must be the versioned path.
    expected_suffix = os.path.join(".jamjet", "bin", "0.10.2")
    assert download_calls[0].endswith(expected_suffix), (
        f"Downloader called with {download_calls[0]!r}, expected suffix {expected_suffix!r}"
    )


def test_find_server_binary_uses_versioned_cache_when_present(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """
    When the versioned binary exists and is executable, it is returned directly
    without calling the downloader.
    """
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    versioned_dir = fake_home / ".jamjet" / "bin" / "0.10.2"
    versioned_dir.mkdir(parents=True)

    versioned_bin = versioned_dir / "jamjet-server"
    versioned_bin.write_bytes(b"versioned-binary")
    versioned_bin.chmod(versioned_bin.stat().st_mode | stat.S_IEXEC)

    original_expanduser = os.path.expanduser

    def fake_expanduser(path: str) -> str:
        if path.startswith("~"):
            return str(fake_home) + path[1:]
        return original_expanduser(path)

    monkeypatch.setattr(os.path, "expanduser", fake_expanduser)
    monkeypatch.setattr("jamjet.cli.main._sdk_version", lambda: "0.10.2")
    monkeypatch.setattr("shutil.which", lambda _name: None)
    monkeypatch.setattr(os, "getcwd", lambda: str(tmp_path))

    download_calls: list[str] = []

    def fake_download(cache_dir: str) -> str:
        download_calls.append(cache_dir)
        return str(versioned_bin)

    monkeypatch.setattr("jamjet.cli.main._download_server_binary", fake_download)

    result = _find_server_binary()

    assert download_calls == [], "Downloader should NOT have been called."
    assert result == str(versioned_bin)


# ---------------------------------------------------------------------------
# _download_server_binary: version pin, JAMJET_SERVER_PATH, checksum (B15)
# ---------------------------------------------------------------------------

import hashlib  # noqa: E402
import io  # noqa: E402
import json  # noqa: E402
import urllib.error  # noqa: E402

_BINARY = b"engine-bytes"
_SUMS_MSG = "This release publishes no checksum; the engine was not verified."


class _Resp(io.BytesIO):
    def __init__(self, data: bytes) -> None:
        super().__init__(data)
        self.headers = {"Content-Length": str(len(data))}

    def __enter__(self) -> _Resp:
        return self

    def __exit__(self, *a: object) -> None:
        return None


def _setup_download(monkeypatch: pytest.MonkeyPatch, *, sums: str | None, status: int = 200):
    """Mock platform and urlopen. Returns the list of requested URLs."""
    import platform

    monkeypatch.setattr(platform, "system", lambda: "Linux")
    monkeypatch.setattr(platform, "machine", lambda: "x86_64")
    monkeypatch.setattr("jamjet.cli.main._sdk_version", lambda: "0.12.0")
    name = "jamjet-server-linux-x86_64"
    assets = [{"name": name}]
    if sums is not None:
        assets.append({"name": "SHA256SUMS"})
    urls: list[str] = []

    def fake_urlopen(req, timeout=None):  # noqa: ANN001
        url = req if isinstance(req, str) else req.full_url
        urls.append(url)
        if "/releases/tags/" in url or url.endswith("/releases/latest"):
            if status == 404:
                raise urllib.error.HTTPError(url, 404, "Not Found", {}, None)  # type: ignore[arg-type]
            return _Resp(json.dumps({"tag_name": "python-sdk-v0.12.0", "assets": assets}).encode())
        if url.endswith("/SHA256SUMS"):
            assert sums is not None
            return _Resp(sums.encode())
        return _Resp(_BINARY)

    monkeypatch.setattr("urllib.request.urlopen", fake_urlopen)
    return urls


def test_download_requests_the_sdk_version_tag(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from jamjet.cli.main import _download_server_binary

    digest = hashlib.sha256(_BINARY).hexdigest()
    urls = _setup_download(monkeypatch, sums=f"{digest}  jamjet-server-linux-x86_64\n")
    _download_server_binary(str(tmp_path / "c"))
    assert urls[0] == "https://api.github.com/repos/jamjet-labs/jamjet/releases/tags/python-sdk-v0.12.0"
    assert not any(u.endswith("/releases/latest") for u in urls)


def test_download_404_names_the_tag_and_never_asks_for_latest(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from jamjet.cli.main import _download_server_binary

    urls = _setup_download(monkeypatch, sums=None, status=404)
    with pytest.raises(FileNotFoundError) as exc:
        _download_server_binary(str(tmp_path / "c"))
    assert str(exc.value) == (
        "No engine release for jamjet 0.12.0 (tag python-sdk-v0.12.0). "
        "Build one with: jamjet dev --build, or set JAMJET_SERVER_PATH."
    )
    assert len(urls) == 1
    assert not any("latest" in u for u in urls)


def test_server_path_env_skips_the_network(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    binary = tmp_path / "my-server"
    binary.write_bytes(b"x")
    binary.chmod(0o755)
    monkeypatch.setenv("JAMJET_SERVER_PATH", str(binary))

    def boom(*a: object, **k: object) -> None:
        raise AssertionError("network used")

    monkeypatch.setattr("urllib.request.urlopen", boom)
    monkeypatch.setattr("jamjet.cli.main._download_server_binary", boom)
    assert _find_server_binary() == str(binary)


def test_server_path_env_missing_fails_without_download(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    missing = tmp_path / "nope"
    monkeypatch.setenv("JAMJET_SERVER_PATH", str(missing))

    def boom(*a: object, **k: object) -> None:
        raise AssertionError("download attempted")

    monkeypatch.setattr("jamjet.cli.main._download_server_binary", boom)
    with pytest.raises(FileNotFoundError) as exc:
        _find_server_binary()
    assert str(missing) in str(exc.value)
    assert "\n" not in str(exc.value)


def test_download_matching_checksum_passes(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from jamjet.cli.main import _download_server_binary

    digest = hashlib.sha256(_BINARY).hexdigest()
    _setup_download(monkeypatch, sums=f"{digest}  jamjet-server-linux-x86_64\n")
    dest = _download_server_binary(str(tmp_path / "c"))
    assert Path(dest).read_bytes() == _BINARY


def test_download_mismatched_checksum_leaves_no_binary(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from jamjet.cli.main import _download_server_binary

    _setup_download(monkeypatch, sums=f"{'0' * 64}  jamjet-server-linux-x86_64\n")
    cache = tmp_path / "c"
    with pytest.raises(FileNotFoundError, match="Checksum mismatch"):
        _download_server_binary(str(cache))
    assert not cache.exists() or list(cache.iterdir()) == []


def test_download_without_sums_warns_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    from jamjet.cli.main import _download_server_binary

    _setup_download(monkeypatch, sums=None)
    _download_server_binary(str(tmp_path / "c"))
    out = " ".join(capsys.readouterr().out.split())
    assert out.count(_SUMS_MSG) == 1
