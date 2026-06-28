"""Deploy — ship a compiled agent IR to a runtime (Track 7a).

``agent.deploy(runtime=...)`` registers the SAME compiled IR that
``agent.run_durable`` builds onto a ``jamjet-server`` engine over the existing
``JamjetClient.create_workflow`` / ``create_cron_job`` path. The only thing that
distinguishes the three legs is the engine URL (+ an optional bearer token):

- ``local`` / ``None`` — the dev default, ``http://127.0.0.1:7700`` (``jamjet dev``).
- ``self-host`` — your own ``jamjet-server`` at ``JAMJET_RUNTIME_URL``.
- ``cloud`` — YOUR hosted ``jamjet-server`` at ``JAMJET_CLOUD_RUNTIME_URL``, with
  JamJet Cloud span-push governance layered on (``cloud_governance=True``).

The honest model (2026-06-28 decision): ``runtime="cloud"`` is **a hosted engine
URL + Cloud governance**, NOT a managed multi-tenant execution "cell". JamJet
Cloud (``api.jamjet.dev``) is a ``/v1/*`` governance/observability span API, not a
workflow execution engine — the deploy client is NEVER pointed at it. "Cloud is
the governance plane, independent of the runtime."

A bare URL string (``resolve_runtime_target("https://my-engine...")``) is used
directly, so any engine endpoint is reachable without a named leg.
"""

from __future__ import annotations

import os
from dataclasses import dataclass

# The dev default — matches Agent.run_durable's runtime_url and the Team default.
LOCAL_RUNTIME_URL = "http://127.0.0.1:7700"


@dataclass(frozen=True)
class RuntimeTarget:
    """A resolved deploy destination: an engine URL + how to reach/govern it.

    Attributes:
        url: The ``jamjet-server`` base URL the IR is registered against
            (``POST /workflows``). All three legs are the same engine API.
        token: Optional bearer token for a secured engine. ``None`` lets
            :class:`~jamjet.client.JamjetClient` fall back to ``JAMJET_TOKEN``.
        cloud_governance: ``True`` only for the ``cloud`` leg — records that
            JamJet Cloud span-push governance is wired alongside the run. It is
            never load-bearing: deploy succeeds even if Cloud is unreachable.
        name: A stable label for the leg (``local`` / ``self-host`` / ``cloud``)
            or the bare URL when one was passed directly. Surfaced on
            :class:`~jamjet.agents.agent.DeployResult.runtime`.
    """

    url: str
    token: str | None
    cloud_governance: bool
    name: str


def _is_url(runtime: str) -> bool:
    return runtime.lower().startswith(("http://", "https://"))


def resolve_runtime_target(runtime: str | None = None) -> RuntimeTarget:
    """Resolve a friendly runtime name (or a bare URL) to a :class:`RuntimeTarget`.

    Mapping:

    - ``None`` / ``"local"`` -> ``http://127.0.0.1:7700``, no token, no governance.
    - ``"self-host"`` -> ``JAMJET_RUNTIME_URL`` (+ optional ``JAMJET_RUNTIME_TOKEN``);
      raises a clear error if the URL is unset.
    - ``"cloud"`` -> ``JAMJET_CLOUD_RUNTIME_URL`` (+ ``JAMJET_CLOUD_TOKEN`` or
      ``JAMJET_API_KEY``), ``cloud_governance=True``; raises a clear error naming
      the honest model if the URL is unset.
    - a bare ``http(s)://`` URL -> used directly, no token, no governance.

    Raises:
        ValueError: if a named remote leg has no configured URL, or the runtime
            name is neither a known leg nor a URL.
    """
    if runtime is None:
        runtime = "local"

    # A bare URL is taken at face value (any engine endpoint, no named leg).
    if _is_url(runtime):
        return RuntimeTarget(url=runtime, token=None, cloud_governance=False, name=runtime)

    leg = runtime.strip().lower()

    if leg == "local":
        return RuntimeTarget(url=LOCAL_RUNTIME_URL, token=None, cloud_governance=False, name="local")

    if leg == "self-host":
        url = os.environ.get("JAMJET_RUNTIME_URL")
        if not url:
            raise ValueError(
                "runtime='self-host' needs a self-hosted engine URL: set "
                "JAMJET_RUNTIME_URL to your jamjet-server (e.g. "
                "https://engine.internal:8080). Add JAMJET_RUNTIME_TOKEN if it is secured."
            )
        token = os.environ.get("JAMJET_RUNTIME_TOKEN")
        return RuntimeTarget(url=url, token=token, cloud_governance=False, name="self-host")

    if leg == "cloud":
        url = os.environ.get("JAMJET_CLOUD_RUNTIME_URL")
        if not url:
            # The honest model: cloud == your hosted engine + Cloud governance.
            # NEVER name api.jamjet.dev here — that is the span/governance API,
            # not a workflow execution engine.
            raise ValueError(
                "runtime='cloud' deploys to YOUR hosted jamjet-server engine AND "
                "enables JamJet Cloud governance — it is not a managed execution cell. "
                "Set JAMJET_CLOUD_RUNTIME_URL to your hosted engine endpoint (e.g. on "
                "Fly), plus JAMJET_CLOUD_TOKEN or JAMJET_API_KEY for governance."
            )
        token = os.environ.get("JAMJET_CLOUD_TOKEN") or os.environ.get("JAMJET_API_KEY")
        return RuntimeTarget(url=url, token=token, cloud_governance=True, name="cloud")

    raise ValueError(f"unknown runtime {runtime!r}; use 'local', 'self-host', 'cloud', or a base URL.")


__all__ = ["LOCAL_RUNTIME_URL", "RuntimeTarget", "resolve_runtime_target"]
