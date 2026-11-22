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
[RBAC](security.md#rbac)). It trusts every page served from this machine, on
any port, and a plain TCP tunnel to the port (`ssh -R`, `socat`) passes as
local, so don't expose a dev-mode port that way.

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

A worker claims with `POST /work-items/claim` and
`{"worker_id": "...", "queue_types": ["..."]}`, which hands out the oldest
claimable item on those queues, or `{"claimed": false}`.

A worker that drives one execution at a time on its own queue (the Spring
integration's recorded calls) claims that execution's work instead:

```json
{ "worker_id": "...", "queue_types": ["app-calls"],
  "execution_id": "exec_...", "node_id": "call-3", "wait_ms": 5000 }
```

- `node_id` (optional) narrows the claim to one node of the execution.
- Only the run's initiator may claim by `execution_id` (anyone in dev mode).
  A run that recorded no initiator (started in dev mode, or before initiators
  were recorded) can be claimed by an `operator`. The token's tenant applies.
- The policy gate is the same as for any claim.
- When nothing comes back the answer says why:
  `{"claimed": false, "reason": "..."}` with `blocked` (the node will not
  run: policy refused it, the engine cannot evaluate it, or it failed for
  good), `approval_required` (it waits for an approval), `claimed_by_other`
  (another worker holds it), `not_found` (no live run with that id that you
  may claim), `refused` (the item was withheld as not the engine's: its
  payload names another workflow than its run; the node is not failed) or
  `not_ready` (not queued yet, or the engine could not decide now; try
  again). `blocked`, `approval_required` and `refused` come from the run's
  log, so a retry, or another worker, gets the same answer. Without
  `node_id` they are about the nodes the run has open now, not a node it has
  moved past. A run that has ended answers `not_found` with its `status`
  (`"completed"`, `"failed"`, `"cancelled"`, ...).
- `wait_ms` waits up to that many milliseconds, at most 10000, for a
  `not_ready` item to be queued, so a client can claim right after starting
  the run. A run that ends during the wait answers `not_found` then. One
  principal holds at most 16 waiting claims on one server process; a claim
  past that, with nothing to hand out, answers `429` with `Retry-After`.
- The same `worker_id` claiming again gets the item it already holds, with
  the same `lease_fence`, after the policy is checked again; a lost claim
  response does not lose the item. The lease is renewed, and `lease_secs`
  gives the length it was granted. A retry while the first claim is still
  deciding answers `not_ready`. The decision runs to its end even when the
  client gives up on the request, so the retry after it gets the lease. A
  lease whose claim withheld the item for a reason to try again is decided
  again as a new claim, with a new `lease_fence`. A decision that outlives
  the lease hands nothing out; one that hands the item out renews the lease
  for its full length first. A lease that expires is charged an attempt
  like any other, whatever its decision had reached, so an item whose
  decision fails every time ends in the dead-letter queue.
- A heartbeat that re-attaches a lease after an expiry keeps it handed out:
  the same worker claiming again gets it under the new `lease_fence`. A
  heartbeat the store could not answer before renewing is a `503` with
  `Retry-After`, worth retrying; a `409` means the lease is lost. Once the
  renewal is made the heartbeat answers with it, and `cancel_requested` is
  `false` if the run could not be read; the next heartbeat asks again.

Runtimes before this change ignore `execution_id` and `node_id` on a claim,
and `claim` on a node, without refusing them: they hand out the oldest item
on the queues, to any worker that polls. A client that may meet one should
check that the item it got is the one it asked for (its `execution_id` and
`node_id`), or check the runtime version first.

An `external_fn` or `python_fn` node can require this: with
`"claim": "by_execution"` in its kind, its items are handed out only to a claim that
names the execution, never to the oldest-first claim, whoever polls the queue. A worker
for such a step must claim by execution. Such an item is also
completed, failed and heartbeated only by a caller who may claim it; anyone
else is answered `404`, as for an item that does not exist (a `lease_fence` is
a counter, not a credential). `POST /work-items` marks the item from the
node's kind, whatever the payload says. `claim` on any other kind is refused
at registration. An item of such a node that lost its mark (enqueued straight
into the store) is still not handed out oldest-first: it is put back, marked,
for the claim that names its execution. One whose payload is not a JSON
object cannot carry the mark, and its node is failed. A runtime from before
this change refuses `claim` on a `python_fn` node at registration. In Python,
`compile_agent_to_ir(agent, prompt, claim_by_execution=True)` sets it on an
agent's tool-dispatch nodes. `jamjet worker` and `JamjetClient.claim_work_item`
do not claim by execution yet. They claim oldest-first, so they never get these
items: the step stays pending and the run waits, with no error. The run's own
worker has to call `POST /work-items/claim` with the run's `execution_id`.

This decides who runs the node and reports its result, not who can read the
run: `GET /executions/:id` shows its state to any token of the tenant.

### 4. Register workflows

```bash
# A fleet file (agents: / workflows:), with its cron schedules
JAMJET_TOKEN=jj_... jamjet deploy fleet.yaml --runtime http://your-runtime:7700

# A single workflow file: registered when you start a run
JAMJET_TOKEN=jj_... jamjet run workflow.yaml --runtime http://your-runtime:7700
```

`POST /workflows` refuses a workflow with a node kind nothing executes, or with
a node that nothing leads to. See [Node types](yaml-reference.md#node-types).

### 5. Start a run by key

A start request can carry a `run_key`, so a retry returns the same run instead of a
second one.

```bash
curl -X POST $RUNTIME/executions -H "Authorization: Bearer $JAMJET_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"workflow_id":"w","workflow_version":"1.0.0","input":{"n":1},"run_key":"nightly:2026-11-20","reuse":"never"}'
# 201 {"execution_id":"exec_...","run_key":"nightly:2026-11-20","created":true}
# again: 200 {"execution_id":"exec_...","current_execution_id":"exec_...","run_key":"...","created":false,"status":"running"}
```

#### When a key may be reused

- `reuse: "never"` (the default): never. A key names one run for good. The same key with the same workflow, version and input always answers 200 with that run, whatever its status, finished runs included.
- `reuse: "failed_only"`: only after the run the key names ended without success (its latest segment is `failed`, `cancelled`, or `limit_exceeded` with no next segment), and only with the same workflow, version and input. A new run starts and the key names it from then on. The old run keeps its id, its log and the key in its `WorkflowStarted`.
- In every mode: another workflow, version or input under the same key is 409, for good. Keys are per tenant. Input is compared by `content_hash` of its canonical JSON with the engine's keys dropped (`action_state`), so key order in the input does not matter. An omitted version is `1.0.0` before hashing.

A key is 1 to 200 bytes of `A-Z a-z 0-9 . _ : / @ + = -`. It is compared as sent, with
no trimming or case folding. `reuse` without a `run_key`, or a value other than `never`
or `failed_only`, is 400.

`GET /executions?run_key=<key>` finds the run: `{"run_key":"<key>","executions":[...]}`,
with an empty list when the tenant holds no such key. It takes no other filter. In the
row, `execution_id` is the first run's id in hyphenated form and `current_execution_id`
is the `exec_` form of the segment running now (a run that continues as new has
several). Compare them after converting one form to the other. `current_status` is that
segment's status.

A runtime from before this change ignores `run_key` and starts a second run. The Python
client checks for the echo of the key in the answer and raises `JamjetAPIError` when it
is missing.

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

## On a serverless container platform

A platform that sets `K_SERVICE` (Cloud Run and others like it) keeps an
instance's files in memory and drops them when the instance stops. A SQLite
file there is lost with the instance, and a network volume is no safe home for
SQLite either. So on such a platform `jamjet-server` refuses to start with
SQLite, and the message names `DATABASE_URL` and the switch below. A
`postgres://` URL passes the check, but this server build cannot open a
Postgres store yet (see the top of this guide), so today the choices there are
the switch below or nothing. `STORAGE_BACKEND=memory` starts with a warning,
and keeps nothing.

To keep the file anyway, for a trial you will throw away, set
`JAMJET_ALLOW_EPHEMERAL_STORE=1`. The server then starts with a warning. The
flag is strict like the other switches: `0` means off, and a value like `maybe`
stops the server. Only `serve` checks this. `token create` and `token revoke`
do not.

The platform stops an instance with SIGTERM and kills it after a stop timeout,
10 s by default on these platforms. On a platform the server assumes 10 s and
sets its grace to 4 s: 4 s for the in-process workers, 2 s for the scheduler, 3 s
for open connections and 1 s of margin. Set `JAMJET_STOP_TIMEOUT_SECS` (a whole
number of at least 6) if your platform's timeout differs. The grace follows it,
unless you set `JAMJET_SHUTDOWN_GRACE_SECS`. A grace that does not fit inside the stop
timeout stops startup and names both variables. A step that is still running at
the kill is reclaimed when its lease expires.

A by-execution claim that is waiting when the stop signal arrives is answered
at once with `{"claimed": false, "reason": "draining"}`. A claim that arrives
after draining has started gets `{"claimed": false}` with no reason. Treat both
as "ask again in about a second". The instance is going away, and the next claim
lands on a new one.

`/health` carries the commit the engine was built from, to check a deploy:

```bash
curl https://engine.example/health
# {"status":"ok","version":"0.5.0","commit":"<40 hex characters>","commit_source":"build-env","features":[...]}
```

`commit` is null when the build had no record of it. `commit_source` says
where it came from: `build-env` (set at build time, as in the image), `git`, or
`none`. Compare `commit` with the commit you meant to deploy. Build your own
image with `docker build --build-arg JAMJET_BUILD_COMMIT=$(git rev-parse HEAD) .`,
because the image has no `.git`. `features` lists what the engine can do, so a
client can check before it relies on a newer route.

Images on Docker Hub are pushed for release tags only (`crates-v*`), and
`latest` is the newest release. A manual run of the image workflow on a branch
fails before it builds. Pin a version tag in a deployment, not `latest`.

---

## Behind a platform identity check

Some serverless container platforms check the caller's identity before a
request reaches your service. The caller then sends two credentials: the
platform's identity token and the engine's own token. They need two headers.
The platform's token goes in `X-Serverless-Authorization`, and the JamJet token
stays in `Authorization`, because the engine reads that one.

From Python, pass an auth hook next to the token. The hook sends its token
only to the engine's address:

```python
from jamjet import JamjetClient
from jamjet.auth import GoogleIdTokenAuth  # pip install 'jamjet[google-auth]'

auth = GoogleIdTokenAuth("https://engine.example")
async with JamjetClient("https://engine.example", api_token=jamjet_token, auth=auth) as client:
    print(await client.health())
```

For a scheduled job, `jamjet ops tick` takes the same flag and reads the JamJet
token from `JAMJET_TOKEN`:

```bash
JAMJET_TOKEN=<operator token> jamjet ops tick \
  --runtime https://engine.example \
  --google-id-token-audience https://engine.example
```

`jamjet ops tick` needs an engine whose `/health` lists `ops_tick`, and exits 2
when the engine has none.

With curl, send both headers yourself:

```bash
curl https://engine.example/health \
  -H "X-Serverless-Authorization: Bearer $(gcloud auth print-identity-token --audiences=https://engine.example)" \
  -H "Authorization: Bearer $JAMJET_TOKEN"
```

A 401 or 403 that does not carry the engine's JSON error body came from the
platform, not the engine. The Python client raises it as `PlatformAuthError`.
The usual causes are a token for the wrong audience and a caller without
permission to call the service. See the
[Python SDK reference](python-sdk.md#calling-an-engine-credentials-and-retries)
for retries and for what the client does after a lost answer.

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
| `JAMJET_DISCOVERY_ALLOW_PRIVATE` | `1`/`true`/`yes`/`on`: let `POST /agents/discover` reach private and loopback addresses and plain `http`. For local development | off |
| `JAMJET_ARTIFACT_THRESHOLD_BYTES` | Node outputs larger than this are stored as artifacts | `8192` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Export traces and metrics over OTLP | unset |
| `RUST_LOG` | Log filter. Logs are JSON outside dev mode | `info` |
| `JAMJET_ALLOW_SELF_APPROVAL` | The principal that started a run may approve it | off |
| `JAMJET_TENANT_TOKENS_AS_DEFAULT` | Tokens of other tenants act in `default` (no isolation) | off |
| `JAMJET_AUDIT_SIGNING_MODE` | `hmac` or `ed25519` | `hmac` |
| `JAMJET_AUDIT_SIGNING_KEY` | HMAC key for the audit log | unset (entries left unsigned) |
| `JAMJET_AUDIT_ALLOW_INSECURE_KEY` | Sign with the built-in public dev key when no HMAC key is set | off |
| `JAMJET_AUDIT_ED25519_KEY_FILE` or `JAMJET_AUDIT_ED25519_KEY` | Ed25519 seed, 64 hex digits | unset |
| `JAMJET_MODEL_TIMEOUT_SECS` | Longest one model provider call may take, in seconds. Connecting is limited to 10 s. An invalid value fails startup. A model's own `timeout_secs` in the IR (`models.<name>.timeout_secs`) limits its calls further | `300` |
| `JAMJET_SHUTDOWN_GRACE_SECS` | On SIGTERM or Ctrl-C, how long to wait for the in-process workers to finish the items they hold. The server keeps answering HTTP meanwhile, so external workers can heartbeat and complete. Then the scheduler gets 2 s and open HTTP connections 3 s more, and a second signal exits at once. With the default a shutdown takes at most 25 s, inside Kubernetes' default `terminationGracePeriodSeconds` of 30. `docker stop` sends SIGKILL after 10 s: run it as `docker stop -t 30`, or set this to 5 | `20` |

---

## Health check

```bash
curl http://localhost:7700/health
# {"status":"ok","version":"0.5.0","commit":null,"commit_source":"none","features":[...]}
```

`commit` and `features` are described under
[On a serverless container platform](#on-a-serverless-container-platform).

`/health` is the only status route. A path the API does not serve answers
`200` with the web UI's HTML for `GET`, so check the status code and the
content type when you probe a route.
