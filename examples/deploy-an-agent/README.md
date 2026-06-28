# deploy-an-agent

Ship a finished agent to a runtime with `agent.deploy(runtime=...)`. The same
compiled IR that `run_durable` builds is registered on a `jamjet-server` engine
over the existing `create_workflow` path, so one agent deploys unchanged to local,
self-host, or a hosted cloud engine.

## Files

- `status_agent.py` - the `@tool` function (`service_health`) and the
  `build_agent()` factory. Imported by both `deploy.py` and the `jamjet worker`.
- `deploy.py` - calls `await agent.deploy(runtime="local")` and prints the
  `DeployResult`, with commented self-host / cloud / schedule variants.

## The three legs (one IR, three URLs)

| `runtime=` | Engine URL | Auth | Notes |
| --- | --- | --- | --- |
| `None` / `"local"` | `http://127.0.0.1:7700` | none | the `jamjet dev` engine; the default |
| `"self-host"` | `$JAMJET_RUNTIME_URL` | `$JAMJET_RUNTIME_TOKEN` (optional) | your own `jamjet-server` |
| `"cloud"` | `$JAMJET_CLOUD_RUNTIME_URL` | `$JAMJET_CLOUD_TOKEN` or `$JAMJET_API_KEY` | your hosted engine + Cloud governance |
| a URL string | used directly | none | any engine endpoint |

A bare `agent.deploy()` with no `runtime` always targets **local**, so you never
hit a remote engine by accident. A remote URL is reached only when you pass one.

## The honest cloud model

`runtime="cloud"` deploys the same IR to **your** hosted `jamjet-server` engine (a
URL you run, for example on Fly) and **also** turns on JamJet Cloud governance
span-push. JamJet Cloud (`api.jamjet.dev`) is the governance and observability
plane, not a workflow execution engine. There is no managed multi-tenant "cell"
that runs your workflows for you. Cloud governance is never load-bearing: the
deploy succeeds even if Cloud is unreachable. Cloud is the governance plane,
independent of the runtime. See `docs/adk/consistency.md` for the engine's exact
guarantees.

## Governance travels with the deploy

The agent's governance (PII redaction, signed audit, receipts, plus any budget or
policy you set) is compiled into the IR and ships unchanged. Deploy never strips
those knobs.

## Run it

`deploy.py` registers the workflow; it does not start an execution (use
`run_durable` for that). With the local dev engine running:

```bash
jamjet dev            # brings up the local engine on 7700
python deploy.py
```

Expected output:

```text
deployed 'status-reporter' to local (http://127.0.0.1:7700)
  scheduled=False  cloud_governance=False
```

To deploy to self-host or cloud, set the env vars shown in `deploy.py` and switch
the `runtime=` argument. To install a recurring schedule, pass a cron expression:
`agent.deploy(runtime="self-host", schedule="0 9 * * *")` (the target engine needs
the SQLite backend and an embedded cron worker).
