# Deployment Guide

The runtime is one binary, `jamjet-server`. It keeps state in a SQLite file, or
in memory for throwaway runs. There is no Postgres backend: the server opens
any `DATABASE_URL` as SQLite, so a `postgresql://` URL fails at startup with
`failed to open state backend`.

The runtime is single-tenant. Tokens of any tenant but `default` are refused;
see [Security](security.md#rbac).

---

## Local development

```bash
jamjet dev
```

This starts the model sidecar, the engine and a Python tool worker. The engine
runs in dev mode, on port 7700, with SQLite at `.jamjet/runtime.db` in the
current directory. Dev mode turns off authentication and runs the in-process
workers and the cron scheduler. Use it on your own machine only.

---

## Running the server

### 1. Build it

Release binaries and Docker images are built from release tags and can lag
`main`. To run the code on `main`, build from the repo. The binary embeds the
web UI from `web/dist`, so build that first:

```bash
(cd web && npm ci && npm run build)
(cd runtime && cargo build --release --bin jamjet-server)
```

To build without the UI, put a placeholder `web/dist/index.html` in its place
first.

### 2. Start it

```bash
mkdir -p /var/lib/jamjet
openssl rand -hex 32 > /var/lib/jamjet/audit-ed25519.key
chmod 600 /var/lib/jamjet/audit-ed25519.key

DATABASE_URL=sqlite:///var/lib/jamjet/runtime.db \
JAMJET_BIND=0.0.0.0 \
JAMJET_PORT=7700 \
JAMJET_EMBED_WORKERS=1 \
JAMJET_AUDIT_SIGNING_MODE=ed25519 \
JAMJET_AUDIT_ED25519_KEY_FILE=/var/lib/jamjet/audit-ed25519.key \
ANTHROPIC_API_KEY=sk-ant-... \
runtime/target/release/jamjet-server
```

Outside dev mode:

- Every route except `GET /health` and `GET /.well-known/did.json` needs a
  Bearer token. [Security](security.md#api-tokens) shows how to create one.
- Set `JAMJET_EMBED_WORKERS`. The in-process worker pool is the only thing
  that runs `model`, `condition`, `human_approval`, `eval` and
  `limit_exceeded` nodes; external workers cannot claim them. Without it a run
  stops at its first such node and stays `running`.
- Set `JAMJET_EMBED_CRON` if you use cron schedules. There is no separate cron
  process.
- Approvals need the action digest the reviewer saw, and the person who
  started a run cannot approve it. See [Human-in-the-Loop](hitl.md).

### 3. Start a Python worker

`python_fn` steps (Python `@tool` and `@workflow.step` functions) run in a
`jamjet worker` process, which needs a `developer` token:

```bash
JAMJET_TOKEN=jj_... jamjet worker --runtime http://your-runtime:7700 --modules myapp.tools
```

Java tool methods (`java_fn`) run on the Java SDK's worker, and `external_fn`
handlers on any worker that drains the queue the node names.

### 4. Register workflows

```bash
# A fleet file (agents: / workflows:), with its cron schedules
JAMJET_TOKEN=jj_... jamjet deploy fleet.yaml --runtime http://your-runtime:7700

# A single workflow file: registered when you start a run
JAMJET_TOKEN=jj_... jamjet run workflow.yaml --runtime http://your-runtime:7700
```

`POST /workflows` refuses a workflow with a node kind nothing executes, or with
a node that nothing leads to. See [Node types](yaml-reference.md#node-types).

---

## Docker

The repo's `Dockerfile` builds the UI, the server and the Python SDK into one
image. It sets `STORAGE_BACKEND=memory`, `JAMJET_BIND=0.0.0.0` and
`JAMJET_PORT=8080`. In memory mode state is lost when the container stops,
the audit log is not kept, and no token can be created, so outside dev mode
every request is refused. Switch it to SQLite on a volume:

```bash
docker build -t jamjet-runtime .

docker run -p 7700:8080 -v jamjet-data:/data \
  -e STORAGE_BACKEND=sqlite \
  -e DATABASE_URL=sqlite:///data/runtime.db \
  -e JAMJET_EMBED_WORKERS=1 \
  jamjet-runtime
```

Images on Docker Hub (`jamjetdev/jamjet`) are built from crate release tags.

---

## Environment variables

These are the variables that change what the server does.

| Variable | What it does | Default |
|----------|--------------|---------|
| `JAMJET_BIND` | Address to listen on | `127.0.0.1` |
| `JAMJET_PORT` | Port to listen on | `7700` |
| `JAMJET_DEV_MODE` | `1` or `true`: no authentication, in-process workers and cron | off |
| `DATABASE_URL` | SQLite database, as `sqlite://<path>` | `sqlite://.jamjet/runtime.db` in the working directory |
| `STORAGE_BACKEND` | `memory` keeps state in memory and keeps no audit log; any other value uses SQLite | SQLite |
| `JAMJET_EMBED_WORKERS` | If set, to any value, runs the in-process worker pool outside dev mode | unset |
| `JAMJET_EMBED_CRON` | If set, to any value, runs the cron scheduler outside dev mode (not with `STORAGE_BACKEND=memory`) | unset |
| `JAMJET_STORE_TERM` | Failover generation for the lease fence, a non-negative integer. An invalid value fails startup | unset |
| `JAMJET_PUBLIC_URL` | Base URL used for the `did:web` id in `/.well-known/did.json` | `http://$JAMJET_BIND:$JAMJET_PORT` |
| `JAMJET_MODEL_SEAM_URL` | Send every model call through the model sidecar at this URL. With the in-process workers on, startup fails if it does not answer `/health` | unset |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY` or `GOOGLE_API_KEY` | Provider keys for direct model calls | unset |
| `OLLAMA_HOST` | Ollama server | `http://localhost:11434` |
| `JAMJET_DISCOVERY_ALLOW_PRIVATE` | `1`/`true`/`yes`/`on`: let `POST /agents/discover` reach private and loopback addresses and plain `http`. For local development | off |
| `JAMJET_ARTIFACT_THRESHOLD_BYTES` | Node outputs larger than this are stored as artifacts | `8192` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Export traces and metrics over OTLP | unset |
| `RUST_LOG` | Log filter. Logs are JSON outside dev mode | `info` |
| `JAMJET_ALLOW_SELF_APPROVAL` | `1` or `true`: the principal that started a run may approve it | off |
| `JAMJET_TENANT_TOKENS_AS_DEFAULT` | `1` or `true`: tokens of other tenants act in `default` (no isolation) | off |
| `JAMJET_AUDIT_SIGNING_MODE` | `hmac` or `ed25519` | `hmac` |
| `JAMJET_AUDIT_SIGNING_KEY` | HMAC key for the audit log | unset (entries left unsigned) |
| `JAMJET_AUDIT_ALLOW_INSECURE_KEY` | Sign with the built-in public dev key when no HMAC key is set | unset |
| `JAMJET_AUDIT_ED25519_KEY_FILE` or `JAMJET_AUDIT_ED25519_KEY` | Ed25519 seed, 64 hex digits | unset |

---

## Health check

```bash
curl http://localhost:7700/health
# {"status":"ok","version":"0.5.0"}
```

`/health` is the only status route. A path the API does not serve answers
`200` with the web UI's HTML for `GET`, so check the status code and the
content type when you probe a route.
