"""Text guards on the release workflows and the image build."""

from __future__ import annotations

import re
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
DOCKER_PUBLISH = (REPO / ".github" / "workflows" / "docker-publish.yml").read_text()
RELEASE_WHEELS = (REPO / ".github" / "workflows" / "release-wheels.yml").read_text()
DOCKERFILE = (REPO / "Dockerfile").read_text()
CROSS_TOML = (REPO / "runtime" / "Cross.toml").read_text()

TAG_CONDITION = "startsWith(github.ref, 'refs/tags/crates-v')"


def _step(text: str, name: str) -> str:
    """The text of the workflow step called `name`, up to the next step."""
    start = text.index(f"- name: {name}")
    nxt = re.search(r"\n\s+- (name|uses):", text[start + 1 :])
    return text[start : start + 1 + nxt.start()] if nxt else text[start:]


def test_docker_publish_pushes_latest_only_for_release_tags() -> None:
    assert "is_default_branch" not in DOCKER_PUBLISH
    latest = [line for line in DOCKER_PUBLISH.splitlines() if "value=latest" in line]
    assert len(latest) == 1, latest
    assert f"enable=${{{{ {TAG_CONDITION} }}}}" in latest[0]


def test_a_branch_run_fails_before_building() -> None:
    guard = _step(DOCKER_PUBLISH, "Images are published for release tags only")
    assert f"if: ${{{{ !{TAG_CONDITION} }}}}" in guard
    assert "exit 1" in guard
    guard_at = DOCKER_PUBLISH.index("Images are published for release tags only")
    for later in ("docker/login-action", "docker/build-push-action", "docker/setup-buildx-action"):
        assert guard_at < DOCKER_PUBLISH.index(later), later


def test_images_and_binaries_carry_the_commit() -> None:
    assert re.search(r"^ARG JAMJET_BUILD_COMMIT=?$", DOCKERFILE.split("AS builder")[1].split("# ── Stage 3")[0], re.M)
    build_line = next(line for line in DOCKERFILE.splitlines() if "cargo build" in line)
    assert 'JAMJET_BUILD_COMMIT="${JAMJET_BUILD_COMMIT}"' in build_line

    assert "build-args: JAMJET_BUILD_COMMIT=${{ github.sha }}" in DOCKER_PUBLISH

    for step in ("Build binary (native)", "Build binary (cross)"):
        assert re.search(r"^\s+JAMJET_BUILD_COMMIT: \$\{\{ github\.sha \}\}$", _step(RELEASE_WHEELS, step), re.M), step
    # `cross` runs the build in a container, which sees only passed-through variables.
    assert '"JAMJET_BUILD_COMMIT"' in CROSS_TOML
