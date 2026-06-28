"""Deploy a finished agent to a runtime with ``agent.deploy(runtime=...)``.

``deploy`` compiles the SAME agent-loop IR that ``run_durable`` builds and
registers it on a ``jamjet-server`` engine over the existing
``create_workflow`` path. The three legs differ only by URL and token, so one IR
ships unchanged to all of them:

    local      -> http://127.0.0.1:7700        (the `jamjet dev` engine; the default)
    self-host  -> $JAMJET_RUNTIME_URL          (+ $JAMJET_RUNTIME_TOKEN if secured)
    cloud      -> $JAMJET_CLOUD_RUNTIME_URL     (+ $JAMJET_CLOUD_TOKEN / $JAMJET_API_KEY)

The honest cloud model: ``runtime="cloud"`` deploys to YOUR hosted jamjet-server
engine (a URL you run, for example on Fly) AND enables JamJet Cloud governance
span-push. JamJet Cloud (api.jamjet.dev) is the governance and observability
plane, not a managed execution service; there is no multi-tenant "cell" that runs
your workflows for you. Cloud governance is never load-bearing: a deploy succeeds
even if Cloud is unreachable.

A bare ``agent.deploy()`` with no runtime always targets LOCAL, so you never hit
a remote engine by accident; a remote URL is reached only when you pass one.

Run it (with `jamjet dev` already running so the local engine is up)::

    python deploy.py
"""

from __future__ import annotations

import asyncio

from status_agent import build_agent


async def main() -> None:
    agent = build_agent()

    # The dev default. Registers the workflow on the local engine and returns a
    # DeployResult; it does NOT start an execution (use run_durable for that).
    result = await agent.deploy(runtime="local")
    print(f"deployed '{result.workflow_id}' to {result.runtime} ({result.url})")
    print(f"  scheduled={result.scheduled}  cloud_governance={result.cloud_governance}")

    # ── Other legs (uncomment after setting the env vars) ────────────────────
    #
    # Self-host: your own jamjet-server, addressed by URL (+ token if secured).
    #   export JAMJET_RUNTIME_URL=https://engine.internal:8080
    #   export JAMJET_RUNTIME_TOKEN=...        # optional
    #   result = await agent.deploy(runtime="self-host")
    #
    # Cloud: your hosted engine URL + JamJet Cloud governance.
    #   export JAMJET_CLOUD_RUNTIME_URL=https://my-engine.fly.dev
    #   export JAMJET_CLOUD_TOKEN=...          # or JAMJET_API_KEY=jj_...
    #   result = await agent.deploy(runtime="cloud")
    #
    # Any explicit engine URL also works directly:
    #   result = await agent.deploy(runtime="https://my-engine.example.com")
    #
    # Install a cron schedule so the engine fires the workflow (needs the SQLite
    # backend + an embedded cron worker on the target engine):
    #   result = await agent.deploy(runtime="self-host", schedule="0 9 * * *")


if __name__ == "__main__":
    asyncio.run(main())
