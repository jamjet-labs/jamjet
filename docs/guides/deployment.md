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
workers and the cron scheduler. Use it on your own machine only: the server
refuses to start in dev mode on any address but loopback unless
`JAMJET_DEV_MODE_ALLOW_REMOTE=1` is set. On any bind, dev mode answers only
requests addressed to `localhost`, a loopback address or a name in
`JAMJET_DEV_ALLOWED_HOSTS`, and refuses requests relayed by a proxy or tunnel
and requests from a page of another site (see
[RBAC](security.md#rbac)).

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
  Bearer token. Issue one with `jamjet-server token create --role <role>`, run
  with the same `DATABASE_URL`; see [Security](security.md#api-tokens).
- Set `JAMJET_EMBED_WORKERS=1`. The in-process worker pool is the only thing
  that runs `model`, `condition`, `human_approval`, `eval` and
  `limit_exceeded` nodes; external workers cannot claim them. Without it a run
  stops at its first such node and stays `running`.
- Set `JAMJET_EMBED_CRON=1` if you use cron schedules. There is no separate cron
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
image. By default it keeps state, the audit log and API tokens in SQLite at
`/data/runtime.db` (a volume), turns authentication on, runs the in-process
workers and the cron scheduler (`JAMJET_EMBED_WORKERS=1`, `JAMJET_EMBED_CRON=1`),
and listens on `0.0.0.0:8080`:

```bash
docker build -t jamjet-runtime .

docker run -d --name jamjet -p 7700:8080 -v jamjet-data:/data jamjet-runtime

# Wait until it answers, so the database and its schema exist.
until curl -sf localhost:7700/health; do sleep 1; done

# Issue a token; it is printed once, and its id goes to stderr.
docker exec jamjet jamjet-server token create --role operator

# Revoke one by id.
docker exec jamjet jamjet-server token revoke <token id>
```

Mount a volume on `/data`, or every container starts with an empty database
and the tokens you issued are gone. The audit log is hash-chained but not
signed until you give it a key: mount an Ed25519 seed and set
`JAMJET_AUDIT_SIGNING_MODE=ed25519` and `JAMJET_AUDIT_ED25519_KEY_FILE` (see
[Signing the audit log](security.md#signing-the-audit-log)).

For a throwaway demo with no state and no authentication, use memory mode and
dev mode. Dev mode refuses to start on the container's `0.0.0.0` unless you opt
in:

```bash
docker run --rm -p 127.0.0.1:7700:8080 \
  -e STORAGE_BACKEND=memory \
  -e JAMJET_DEV_MODE=1 -e JAMJET_DEV_MODE_ALLOW_REMOTE=1 \
  jamjet-runtime
```

Publishing on `127.0.0.1` keeps other machines from the published port, but
the server still has no authentication: any container on the same Docker
network can reach it directly. Requests must name `localhost` or a loopback
address; to call it as `host.docker.internal` (or by a WSL or LAN name), add
`-e JAMJET_DEV_ALLOWED_HOSTS=host.docker.internal`. Use this on your own machine only. In memory
mode state is lost when the container stops, no audit log is kept, and no
token can be issued.

Images built before this change default to `STORAGE_BACKEND=memory`, and their
`jamjet-server` has no `token` command (it ignores the arguments and starts a
second server). Build the image from this repo to get the setup above.

Images on Docker Hub (`jamjetdev/jamjet`) are built from crate release tags.

---

## Environment variables

These are the variables that change what the server does.

The on/off switches (`JAMJET_DEV_MODE`, `JAMJET_DEV_MODE_ALLOW_REMOTE`,
`JAMJET_EMBED_WORKERS`, `JAMJET_EMBED_CRON`, `JAMJET_ALLOW_SELF_APPROVAL`,
`JAMJET_TENANT_TOKENS_AS_DEFAULT`, `JAMJET_AUDIT_ALLOW_INSECURE_KEY`) all read
the same way: `1`, `true`, `yes` or `on` is on; `0`, `false`, `no`, `off`, empty
or unset is off, in any case. Any other value stops the server at startup with
the variable's name, instead of being read as one or the other. The Python SDK
reads `JAMJET_AUDIT_ALLOW_INSECURE_KEY` the same way and raises `ValueError` on
any other value.

| Variable | What it does | Default |
|----------|--------------|---------|
| `JAMJET_BIND` | Address to listen on | `127.0.0.1` |
| `JAMJET_PORT` | Port to listen on | `7700` |
| `JAMJET_DEV_MODE` | No authentication, in-process workers and cron. Refused on a non-loopback bind | off |
| `JAMJET_DEV_MODE_ALLOW_REMOTE` | Let dev mode listen on a non-loopback address (logged as a warning). Requests must still name an allowed host | off |
| `JAMJET_DEV_ALLOWED_HOSTS` | Host names dev mode answers besides `localhost` and loopback addresses, comma-separated, without scheme or port (`host.docker.internal`) | none |
| `DATABASE_URL` | SQLite database, as `sqlite://<path>` | `sqlite://.jamjet/runtime.db` in the working directory (`sqlite:///data/runtime.db` in the Docker image) |
| `STORAGE_BACKEND` | `memory` keeps state in memory and keeps no audit log; any other value uses SQLite | SQLite |
| `JAMJET_EMBED_WORKERS` | Runs the in-process worker pool outside dev mode | off (`1` in the Docker image) |
| `JAMJET_EMBED_CRON` | Runs the cron scheduler outside dev mode (not with `STORAGE_BACKEND=memory`) | off (`1` in the Docker image) |
| `JAMJET_STORE_TERM` | Failover generation for the lease fence, a non-negative integer. An invalid value fails startup | unset |
| `JAMJET_PUBLIC_URL` | Base URL used for the `did:web` id in `/.well-known/did.json` | `http://$JAMJET_BIND:$JAMJET_PORT` |
| `JAMJET_MODEL_SEAM_URL` | Send every model call through the model sidecar at this URL. With the in-process workers on, startup fails if it does not answer `/health` | unset |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY` or `GOOGLE_API_KEY` | Provider keys for direct model calls | unset |
| `OLLAMA_HOST` | Ollama server | `http://localhost:11434` |
| `JAMJET_ARTIFACT_THRESHOLD_BYTES` | Node outputs larger than this are stored as artifacts | `8192` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Export traces and metrics over OTLP | unset |
| `RUST_LOG` | Log filter. Logs are JSON outside dev mode | `info` |
| `JAMJET_ALLOW_SELF_APPROVAL` | The principal that started a run may approve it | off |
| `JAMJET_TENANT_TOKENS_AS_DEFAULT` | Tokens of other tenants act in `default` (no isolation) | off |
| `JAMJET_AUDIT_SIGNING_MODE` | `hmac` or `ed25519` | `hmac` |
| `JAMJET_AUDIT_SIGNING_KEY` | HMAC key for the audit log | unset (entries left unsigned) |
| `JAMJET_AUDIT_ALLOW_INSECURE_KEY` | Sign with the built-in public dev key when no HMAC key is set | off |
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
