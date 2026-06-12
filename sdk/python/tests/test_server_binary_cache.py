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
