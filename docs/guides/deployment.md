# Deployment Guide

The runtime is one binary, `jamjet-server`. It keeps state in a SQLite file, in
Postgres (a preview, see [Postgres](#postgres-preview)), or in memory for
throwaway runs.

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
  stops at its first such node and stays `running`. With the loops off
  ([Request-driven mode](#request-driven-mode)) the engine runs them inside
  requests instead, and this setting is ignored.
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
  `draining` (the server is stopping; claim again, usually on another
  instance) or `not_ready` (not queued yet, or the engine could not decide
  now; try again). `blocked`, `approval_required` and `refused` come from the run's
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
worker has to call `POST /work-items/claim` with the run's `execution_id`. In
Python, `agent.run_durable(drive=True)` is that worker (see "Driving a run from
your own process" in the Python SDK guide).

This decides who runs the node and reports its result, not who can read the
run: `GET /executions/:id` shows its state to any token of the tenant.

#### Model steps run by the run's own worker

A `model` node can leave its model call to the run's own worker: the process
that holds the token that started the run, which may send the call on through
a router of its own. Set `executor` in the node's kind:

```json
"think": { "kind": { "type": "model", "model_ref": "planner",
                     "prompt_ref": "Find a venue in {{ state.city }}",
                     "output_schema": "answer", "tools": [ ... ],
                     "executor": "worker" } }
```

- `executor` is `engine` (the default) or `worker`. `POST /workflows` refuses
  it on any kind but `model` (400), and refuses `claim` on a `model` node: a
  worker model step is always claimed by execution. It goes inside `kind`. Put
  at the node's top level it is ignored, and the engine makes the call.
- The YAML compiler does not write it yet. The Python SDK writes it for
  `agent.run_durable(drive=True)`. A client that writes it should write it only
  for `worker`: `"executor": null` is refused on every kind.
- The step goes to the built-in queue `model_worker`. Only a claim that names
  the run gets it: the oldest-first claim never hands it out, the engine's own
  workers never run it, and `jamjet worker` does not claim it. While nobody
  claims it the step stays pending and the run waits. A new `external_fn`
  version may not name `model_worker` as its queue.
- `POST /work-items` refuses such a node (400). Its payload is the request the
  claim hands out, and only the engine builds it, from the run's state.
- A step of such a node written straight into the store on the engine's
  `model` queue is run by the engine. The run's worker cannot complete or fail
  it (403), and any other token is told `404`, as for every step of the node.
- A runtime from before this change drops `executor` and makes the call
  itself. Check the runtime version before you rely on it.

The claim is a claim by execution, as above:

```json
POST /work-items/claim
{ "worker_id": "router-1", "queue_types": ["model_worker"],
  "execution_id": "exec_...", "wait_ms": 5000 }
```

```json
{ "claimed": true, "work_item": {
    "id": "...", "execution_id": "...", "node_id": "think",
    "queue_type": "model_worker", "attempt": 0,
    "lease_fence": 4294967297, "lease_secs": 60, "idempotency_key": "...",
    "payload": { "workflow_id": "w", "workflow_version": "0.1.0", "node_id": "think",
                 "model": "vertex_ai/gemini-2.5-pro", "temperature": 0.5, "max_tokens": 512,
                 "system_prompt": "You plan events.", "tools": [ ... ],
                 "prompt": "Find a venue in Lisbon", "output_schema": "answer",
                 "claim": "by_execution" },
    "budget_left": { "total_tokens": 1200, "input_tokens": null,
                     "output_tokens": null, "cost_usd": 0.42 } } }
```

- The claim runs the same gate as every item: the model the payload names
  against the allow lists (tenant, workflow, node), the agent card, autonomy
  limits (the step is counted as an agent step when it is claimed), approvals
  and the run's caps. A spent run answers `budget`. A node an approval gates
  answers `approval_required` until it is decided. A model off an allow list
  answers `blocked`, and the node fails.
- The payload is the request the engine built: the model id and its settings,
  the system prompt, the tool schemas, the node's `output_schema`, and the
  node's prompt rendered against the run's state. A node with no prompt sends
  the conversation in the run's `state.messages` instead, which earlier steps
  of the run wrote.
- A step whose request has no prompt to send fails at claim, with the words
  the in-process model executor uses, and the claim answers `blocked`. That is
  a prompt template that names a state key the run lacks, or no prompt and no
  conversation. The worker never sends a prompt of its own making.
- `budget_left` is what the run's caps leave, counted at claim from the same
  read of the run's log that decided the step may fire. It sits beside
  the payload, so the payload and the digests that bind it are unchanged. A
  field is null when the run has no such cap, and 0 when the engine cannot
  check that cap (a step left out a count it reads, or the stored dollar cap
  is not a valid one). It is never negative. Other steps of the run can spend
  after the claim, so it is a limit to pass to the model, not a promise: the
  engine still enforces the caps.
- A step claimed again after its lease ran out, by any worker of the run,
  carries the same `idempotency_key`, as long as the run's state is the same
  at both claims. The key covers the state, as it does for every external
  step, so another branch of the run that writes state in between gives the
  retry a new key. A router that keeps answers by that key can answer a retry
  without paying again.

The worker completes the step with the model's answer, never with an output or
a state patch:

```json
POST /work-items/:id/complete
{ "lease_fence": 4294967297, "idempotency_key": "...", "duration_ms": 1840,
  "model_result": { "content": "", "finish_reason": "tool_calls",
                    "model": "vertex_ai/gemini-2.5-pro",
                    "tool_calls": [ { "id": "c1", "name": "search", "arguments": { "q": "venues" } } ],
                    "input_tokens": 812, "output_tokens": 95, "cost_usd": 0.0031 } }
```

- `content`, `finish_reason` and `model` are required. `content` is a string:
  send `""` when the provider returned no text with its tool calls.
  `tool_calls` is optional, a flat list of `{id, name, arguments}`.
  `input_tokens` and `output_tokens` are required, 0 included. `cost_usd` is
  optional; `cost_currency`, if sent, must be `USD`. Fields the engine does
  not read are ignored.
- The engine builds the step's output and state patch from the answer with the
  in-process model executor's own function, so the two executors record the
  same step. The output is `content`, `model`, `finish_reason` and
  `tool_calls`. The state patch is exactly `last_model_output`,
  `last_model_finish_reason` and `last_model_tool_calls`. The calls then reach
  the next tool dispatch node, as an in-process model step's do, and each one
  passes the dispatch guard there: a blocked call fails that node, as it would
  for a model the engine called.

The checks, in order. Each refusal answers `{"completed": false, "reason":
"..."}` and records nothing, except where the table says the step fails.

| What was sent | Answer |
|---|---|
| By a caller who may not claim the item | `404`, whatever the body says |
| No `lease_fence`, or one the item no longer holds: an answer after the lease ran out (read on the store's clock, whether or not anything has taken the step back yet), or the same answer sent twice | `409` |
| `output` or `state_patch`; no `model_result`; usage beside `model_result` instead of in it (`input_tokens`, `output_tokens`, `cost_usd`, `cost_currency`, `finish_reason`, `gen_ai_model`, `gen_ai_system`) | `400` |
| A `model_result` that does not parse: a cost sent as a string, a token count that is negative or not whole, `content` that is not a string, a call with no name | `422`. When the usage is what does not parse, a cap that reads it fails the step for good, as for usage left out: a token count under any cap, a cost under a dollar cap. An answer that does not parse and leaves out a token count does the same under any cap, whatever else is wrong with it |
| A token count left out, or a cost that is not a number from 0 to 10,000 in USD | `400`. Under a cap that cannot count it the step fails for good in the same write, and the answer says so: missing tokens under any cap, a bad cost under a dollar cap |
| A `model` that is empty, longer than 256 characters or holds a control or invisible format character; more than 256 calls; `finish_reason` `tool_calls` with no calls, or calls with any other `finish_reason`; a call id that is empty or used twice; a call to a tool the registered node does not offer | `422` |
| A block or a gate put on after the claim | `409` |
| A `model` off an allow list (tenant, workflow or node) | `409`. A `PolicyViolation` "model used is not allowed: <model>" is recorded and the step fails for good |
| Anything else | `200`. `NodeCompleted` is recorded under the engine's idempotency key, in one fenced write. The scheduler's next pass fails a run past its cap |

A `503` means the engine could not read what it needed to decide, or could
not tell whether its write landed (a Postgres `COMMIT` that failed). Send the
same answer again. If that answer is a `409`, read the run to see whether the
first one was recorded.

A worker that hands the call to a router should pass it the run id, the node
id, the attempt and the idempotency key, so the router can tell a retry from a
new call. A `422` will never be recorded as sent. Treat it as a failed
attempt: report it on `POST /work-items/:id/fail` with the `lease_fence`, so
the step is retried, rather than sending the same answer again.

Under a dollar cap, an answer with no `cost_usd` is recorded with its cost
unknown, and the run fails on the next pass with `cost_not_reported`. It is
answered `200` and recorded, so do not send it again. A negative cost, or one
over the ceiling, is a `400` and is not recorded. With no dollar cap, no cost
is fine.

Where the two executors still differ:

- The worker's step does not write the `__budget` display counters the
  in-process worker writes. The caps read the log, not those counters.
- The `jamjet.node` span, its GenAI attributes and the
  `gen_ai.client.token.usage` metric are recorded for steps the engine runs
  only. A worker's model step is on the log and in `usage`, not in them.
- The spend of a step that fails for good is not on the log, as for any failed
  attempt. That covers a model off an allow list, and tokens or a cost a cap
  cannot count. The run's `usage` does not include that call.

A long output (above `JAMJET_ARTIFACT_THRESHOLD_BYTES`, 8 KiB by default) is
moved to the artifact store on both executors, and the event records the same
reference. The state patch stays inline.

The completion of every other kind is unchanged: it still needs `output` and
`state_patch` (422 without them), and `model_result` on one is a 400.

#### What the engine checks, and what it trusts

The worker here is the run's own code, holding the token that started the run.
The engine cannot stop it doing anything on its own machine. What the engine
guarantees is what it hands out, what it records and what it moves past.

The worker is trusted for two things:

- The model's answer: its text and its tool calls. This is model output, which
  is untrusted text in any case.
- The usage it reports: the tokens, the cost and the model id.

The engine checks everything else, every time:

| What | How |
|---|---|
| Who may claim | By execution only, and only the run's initiator. Anyone else is told `not_found` at claim, and `404` on complete, fail and heartbeat. |
| The model | The allow lists (tenant, workflow, node) at claim, against the model the request names, and at completion, against the model the worker reports. |
| The caps | Checked at claim. A run past its cap fails in the next pass. Under a cap, an answer whose cost or tokens cannot be counted fails the step. |
| The tools | Every call the model asks for goes to a tool dispatch node and passes its guard: blocks, approvals, the agent card's lists. A call to a tool the node did not offer is refused at completion. |
| The state | The engine builds the output and the state patch from the answer, with the in-process function. The worker cannot write any state key, the engine's included. |
| The lease | A fenced completion only. A late or repeated answer is a `409` and records nothing. |
| The kind | Only a `model` node marked `executor: worker` reaches this queue. A kind the engine runs never does. |

Put plainly: a lying worker can report less spend than it made, since the caps
hold only as well as the report (the caller's router keeps the money
boundary), and it can name a model it did not use. It cannot run a blocked
tool through the engine, skip an approval, write another run's log, write the
engine's state keys, be recorded after its lease, or get a step on a spent
run.

Two narrow gaps remain, both in the audit log, neither in the run's state:

- A model off an allow list, reported after the run was cancelled, records its
  `PolicyViolation` in the cancelled run's log. The answer is then a `409` and
  the item is withdrawn.
- If another claim takes the lease back between the engine's last read of it
  and the write of that violation, the violation is recorded for a step the
  new holder then runs.

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

## Postgres (preview)

Several engine instances can share one Postgres database. A `DATABASE_URL`
that starts `postgres://` or `postgresql://` selects it; `STORAGE_BACKEND=memory`
still wins over any URL. The server logs `Postgres store (preview)` at start.
The URL never reaches the logs at info level, and the commands below never
print its password.

```bash
export DATABASE_URL=postgres://jamjet:<password>@db.internal:5432/jamjet

# Before each deploy, once:
jamjet-server migrate
# schema at version 3 (3 applied)

jamjet-server            # every engine instance
```

### Migrate before each deploy

`jamjet-server migrate` applies the pending migrations and exits 0. Run it once
per deploy, before the new instances start. Two runs at once take turns: the
second waits and then applies nothing. It exits 3 when the database is newer
than this build and 4 when a migration is half done or differs from this
build's, and changes nothing in either case.

`jamjet-server migrate --check` changes nothing. It exits 0 when the schema is
current, 2 when it is behind (or empty), 3 when it is newer, 4 when it is half
done or changed.

The server checks the schema at start and refuses one it does not match:

- empty or older: run `jamjet-server migrate`;
- newer: this build is older than the database schema; do not roll back below
  a schema migration;
- half done or changed: the message names the migration.

Migrations stay additive for one release (new tables, new columns with
defaults, new indexes), so instances of the previous build keep running while a
deploy migrates. A restarted instance of the old build then refuses the newer
schema, as it should.

On SQLite, `jamjet-server migrate` migrates the file the way the server does
when it opens it, and `--check` is refused.

### Pool and connections

Each engine opens one pool, shared by the state store, the audit log, the agent
registry and the audit outbox drain. Each setting is checked at start; a value
that is set but not valid stops the server, naming the variable.

| Variable | What it does | Default |
|----------|--------------|---------|
| `JAMJET_PG_POOL_SIZE` | Connections in the pool, at least 2 | `10` |
| `JAMJET_PG_ACQUIRE_TIMEOUT_MS` | How long a call waits for a free connection before it answers 503 | `5000` |
| `JAMJET_PG_STATEMENT_TIMEOUT_MS` | `statement_timeout` on each connection | `30000` |
| `JAMJET_PG_LOCK_TIMEOUT_MS` | `lock_timeout` on each connection | `10000` |
| `JAMJET_PG_IDLE_IN_TX_TIMEOUT_MS` | `idle_in_transaction_session_timeout` on each connection | `60000` |

The connection budget is the number of engines times `JAMJET_PG_POOL_SIZE`,
plus 2 for each `token` command running and 1 for each `migrate`. Keep it under
the database's `max_connections`, with room for your own tools.

### What differs on Postgres

- Cron is not supported yet: each engine would fire every schedule.
  `JAMJET_EMBED_CRON=1` stops the server at start. In dev mode the cron
  scheduler is skipped with a warning, and `/cron` answers 400, as with
  `STORAGE_BACKEND=memory`. The Docker image sets `JAMJET_EMBED_CRON=1`, so
  pass `-e JAMJET_EMBED_CRON=0` with a Postgres URL.
- Every engine on one database writes one audit chain, so every engine and
  every `token` command must use the same audit signing settings
  ([Signing the audit log](security.md#signing-the-audit-log)).
- The token commands work as on SQLite, against a migrated database:
  `jamjet-server token create --role operator` with the same `DATABASE_URL`.
  A token issued once is accepted by every engine on the database.

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
and the tokens you issued are gone. To run the image on Postgres, pass
`-e DATABASE_URL=postgres://...` with `-e JAMJET_EMBED_CRON=0`, since cron is
not supported on Postgres yet. On a serverless container platform that gives
an instance no CPU between requests, add `-e JAMJET_BACKGROUND=off` too (see
[Request-driven mode](#request-driven-mode)).
The audit log is hash-chained but not
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

A platform that sets `K_SERVICE` (Knative and platforms built on it) keeps an
instance's files in memory and drops them when the instance stops. A SQLite
file there is lost with the instance, and a network volume is no safe home for
SQLite either. So on such a platform `jamjet-server` refuses to start with
SQLite, and the message names `DATABASE_URL` and the switch below. A
`postgres://` URL passes the check: use a managed Postgres there (see
[Postgres](#postgres-preview)). Where the platform gives an instance no CPU
between requests, turn the loops off too (see
[Request-driven mode](#request-driven-mode)). `STORAGE_BACKEND=memory` starts
with a warning and keeps nothing.

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
within half a second with `{"claimed": false, "reason": "draining"}`. A claim that arrives
after draining has started gets `{"claimed": false}` with no reason. Treat both
as "ask again in about a second". The instance is going away, and the next claim
usually lands on another one.

`/health` carries the commit the engine was built from, to check a deploy:

```bash
curl https://engine.example/health
# {"status":"ok","version":"0.5.0","commit":"<40 hex characters>","commit_source":"build-env","features":[...],"background":"off","tick_overdue":false}
```

`commit` is null when the build had no record of it. `commit_source` says
where it came from: `build-env` (set at build time, as in the image), `git`
(read from the checkout), `crate-package` (a crate built from crates.io) or
`none`. A build from a tree with uncommitted changes reports HEAD, so build a
release from a clean checkout. Compare `commit` with the commit you meant to deploy. Build your own
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

## Request-driven mode

By default the engine runs its own loops: the scheduler, the approvals
projector, the in-process workers, the audit outbox drain on Postgres, and
cron. They need CPU between requests. A serverless container platform may give
an instance no CPU once it has answered, and stops instances nobody calls, so
there the loops stall. With `JAMJET_BACKGROUND=off` none of them start. Two
things move the engine instead:

- **Inline advance.** After a write has committed, and before it answers, the
  engine takes that run as far as it can. It dispatches the next steps and runs
  its own quick steps (`condition`, `human_approval`, `limit_exceeded`) within
  `JAMJET_INLINE_ADVANCE_MS`. The writes that do this are `POST /executions`,
  approve, cancel, an external event, a work item's complete and fail, and the
  MCP tools that start, approve and cancel a run.
- **A tick.** `POST /ops/tick` does once what the loops do. It reclaims lapsed
  leases, seals the audit outbox on Postgres, advances each running run from
  where the last tick stopped (applying approval deadlines on the way), runs the
  engine steps still on its queues, model and eval calls among them, and brings
  the approvals read model up to date. A scheduler calls it about once a minute.

### Settings

| Variable | What it does | Default |
|----------|--------------|---------|
| `JAMJET_BACKGROUND` | `on` or `off`, in any case. Any other value, `0`, `false` and empty included, stops the server | `on` |
| `JAMJET_TICK_INTERVAL_SECS` | How often you tick, in whole seconds, at least 1. The overdue check below counts in it | `60` |
| `JAMJET_TICK_BUDGET_MS` | Longest one tick spends, at least 100. A tick request can lower it, never raise it | `45000` |
| `JAMJET_INLINE_ADVANCE_MS` | Longest a write's advance spends running engine steps, at least 1. One step always runs, whatever the budget | `2000` |
| `JAMJET_TICK_ENGINE_CONCURRENCY` | Engine steps one tick runs at once, at least 1 | `4` |

The four numbers are read at every start, with the loops on too. A value that
is set but not valid stops the server, naming the variable.

With the loops off:

- `JAMJET_EMBED_CRON=1` stops the server and names both variables, since cron is
  a loop. The Docker image sets it, so pass `-e JAMJET_EMBED_CRON=0`. Cron
  schedules do not fire.
- `JAMJET_EMBED_WORKERS` is ignored, with one info line. The model setup is
  still checked at start, so a bad one stops the server as before.
- On Postgres, `JAMJET_TICK_ENGINE_CONCURRENCY` must be below
  `JAMJET_PG_POOL_SIZE - 1`, or the server stops: each engine step holds a
  connection, the tick holds one, and the requests need the rest. The defaults
  (4 and 10) fit.
- The server logs `background loops are off; call POST /ops/tick every minute`
  once at start. In dev mode no loop starts either.

With the loops on, on a platform that sets `K_SERVICE`, the server logs one
warning at start that the loops need CPU between requests.

### When each step runs

- A step a worker runs moves on at once. Its completion advances the run before
  it answers, so the next step can be claimed as soon as the worker has its 200.
- A quick engine step that a write's pass dispatches runs in that write's
  advance. A run whose first node is a quick step, and a quick step an advance
  left when its budget ran out, wait for the next tick.
- A model or eval step the engine runs waits for a tick, which runs it and
  waits for it to finish. The step after it is dispatched by the next write to
  the run or the next tick. A run of such steps moves about one step per tick.
- Approval deadlines expire, and lapsed leases are reclaimed, only on a tick.
- `GET /approvals` reads the approvals read model, and so does
  `GET /executions/:id/approvals` once a tick has projected that run. Only a
  tick brings the model up to date, so both can lag the log by one tick
  interval. Before a tick has projected a run, `GET /executions/:id/approvals`
  reads the run's log. The approve route always decides from the log.

The advance never changes a write's answer. A failure in it is logged and
counted, and the next tick goes on from the log. It runs as the engine, never as
the caller, and only after a request that wrote something: a repeated decision,
or a start whose `run_key` names a run already started, advances nothing. While
the server stops, writes skip the advance and the next tick catches up. A write
answers after its advance, so it can take up to about `JAMJET_INLINE_ADVANCE_MS`
longer.

### The tick

```bash
curl -X POST https://engine.example/ops/tick -H "Authorization: Bearer $TICKER_TOKEN"
```

The body is empty, `{}`, or `{"budget_ms": n}` with `n` at least 100. A larger
budget is held to `JAMJET_TICK_BUDGET_MS`. Any other field is a 400.

```json
{"ran": true, "complete": true, "elapsed_ms": 48, "budget_ms": 45000, "over_budget_ms": 0,
 "reclaimed": {"retried": 0, "dead_lettered": 0},
 "runs": {"visited": 3, "advanced": 1, "stale_passes": 0, "errors": 0},
 "engine_steps": {"quick": 1, "slow": 0}, "deadlines": {"expired": 0, "escalated": 0},
 "audit_outbox": {"sealed": 0, "failed": 0}, "projected": 1,
 "next_after": null, "background": "off"}
```

- `complete` is false when the tick stopped before the end: its budget ran out,
  the server began to stop, or a read failed. `next_after` is the run it stopped
  after, and the next tick starts there. Tick again, or use
  `jamjet ops tick --until-complete`.
- `over_budget_ms` is how far past its budget the tick ran, waiting for engine
  steps it had started. A tick starts engine steps only in the first half of its
  budget, and a step in hand always finishes.
- While another tick runs, on this instance or on another engine on the same
  store, the answer is `{"ran": false, "reason": "busy", "held_by": "...",
  "lease_until": "..."}` with 200. `lease_until` is null when the other tick
  runs on the same instance.
- `503` with `Retry-After: 1` while the server stops.
- `409` from an engine with the loops on and no embedded workers: it has
  nothing to run engine steps with. With the embedded workers on, or in dev
  mode, a tick runs beside the loops and says `"background": "on"`.

Two ticks never run at once. A lock in each process and a lease in the store
collapse them to one, so with several engines on one database you can tick any
of them. A repeated or late tick records nothing twice. That rests on the
store's conditional writes, not on the lease. The lease lasts the tick's budget
plus 120 s. If an instance dies mid-tick, every tick answers busy until that
lease lapses. A tick goes on when its caller hangs up, so a scheduler that gives
up early does not cut it off between two steps.

### Who may call the tick

1. Outside dev mode, a Bearer token the engine admits (it exists, is not revoked
   or expired, and is in tenant `default`) with the `operator` role. No token,
   or a bad one, is a 401. Any other role, or another tenant's operator token,
   is a 403. POST only.
2. In dev mode, no token, as for every route. Dev mode listens on loopback only
   unless `JAMJET_DEV_MODE_ALLOW_REMOTE=1`.
3. A platform identity check in front of the engine is a second gate, never a
   replacement. The engine reads no platform header for identity.
4. The body can only lower the budget. It names no run, tenant or node.
5. A tick does only what the loops would do. It cannot approve, pick a branch
   or complete a worker's step. The inline advance is the same, and runs as the
   engine.
6. Ticks collapse to one at a time, so a flood of ticks from a stolen operator
   token costs one running tick. The token is the larger risk.

The tick takes the JamJet token in `Authorization` only, as every route does.
It is never open to the platform's identity alone: one wrong invoker permission
would then open the engine.

`GET /ops/status` changes nothing, and any role may read it. See
[Noticing that nobody ticks](#noticing-that-nobody-ticks).

### The scheduler's token

Give the scheduler an operator token of its own, used for nothing else. Keep it
in your platform's secret store, not in the job's settings. Rotate it in three
steps: issue, deploy, revoke.

```bash
# 1. Issue a new token, with the server's DATABASE_URL and audit signing settings.
jamjet-server token create --role operator --name ticker
# 2. Put it in the secret store, and deploy the scheduler or job that reads it.
# 3. Once a tick with the new token has run, revoke the old one by its id.
jamjet-server token revoke <old token id>
```

`token create` prints the token once, on stdout, and its id on stderr. Run the
token commands where they can reach the database, for example as a one-off job
from the same image.

### A scheduler that can send one credential

Without a platform identity check in front, any scheduler that can send a POST
with a header can tick: `Authorization: Bearer <operator token>`.

Behind one, a tick needs two credentials in two headers (see
[Behind a platform identity check](#behind-a-platform-identity-check)). A
scheduler that can send only its platform identity token, in `Authorization`,
cannot send the engine's token too. Run `jamjet ops tick` as a scheduled job
instead. It sends both:

```bash
JAMJET_TOKEN=<operator token> jamjet ops tick \
  --runtime https://engine.example \
  --google-id-token-audience https://engine.example
```

It exits 0 when a tick ran or another was running, 1 on an error, and 2 when
the engine has no tick route. `--until-complete` ticks again until a tick
reports `complete`, at most `--max-calls` times (10 by default). `--json` prints
the last answer. From Python, `await client.tick()` sends one tick.

### Timeouts

The platform's request timeout must cover the tick budget plus the slowest
engine step, because a tick waits for every step it started. One model call can
take up to `JAMJET_MODEL_TIMEOUT_SECS` (300 s by default), or less where a
model's own `timeout_secs` in the IR says so. With the defaults, allow at least
45 s plus 300 s. Lower the model timeout or the tick budget if your platform's
limit is shorter.

A tick that waits more than 120 s past its budget for a slow step outlives its
lease, and another tick can then start beside it. Nothing is recorded twice,
but it is wasted work.

On a stop signal, a tick or an advance in progress counts as work the shutdown
waits for, within the grace (4 s on a platform). A tick that is stopping
finishes the run in hand, starts nothing more and gives its lease back. A step
still running when the platform kills the instance is retried once its lease
runs out, as at any kill.

### Noticing that nobody ticks

With the loops off nothing fails when the ticks stop, so `/health` says so:

```bash
curl https://engine.example/health
# {"status":"ok",...,"background":"off","tick_overdue":false}
```

- `tick_overdue` is true when the last tick finished more than three intervals
  ago (`JAMJET_TICK_INTERVAL_SECS`), or when none has finished and the process
  has been up for longer than that. It is null with the loops on, and when the
  store could not be read, or not within 2 s. `status` stays `"ok"`, since the
  process still serves.
- Each instance reads the tick state at most every 30 s, so a tick another
  engine finished shows up to 30 s later.
- While a tick is overdue, any request makes the instance log one error line,
  at most once every 5 minutes: `no tick for N s while the background loops are
  off: approvals do not expire, lapsed leases are not reclaimed, engine steps do
  not run`. Alert on it.

`GET /ops/status` reads the stored tick state fresh, and returns it with the
last tick's report as `POST /ops/tick` answered it:

```bash
curl https://engine.example/ops/status -H "Authorization: Bearer $JAMJET_TOKEN"
# {"background":"off","tick_interval_secs":60,"tick_overdue":false,
#  "tick":{"holder":null,"lease_until":null,
#          "last_started_at":"2026-11-23T10:00:00.006Z","last_finished_at":"2026-11-23T10:00:00.054Z",
#          "last_complete":true,"next_after":null,"last_report":{"ran":true,"complete":true,...}}}
```

`tick` is null until a tick has begun. When the store cannot be reached, or does
not answer within 2 s, the route answers 503 with `Retry-After`. Any other store
error is a 500.

### A deploy, in order

1. `jamjet-server migrate` against the managed Postgres, before the new
   instances start ([Migrate before each deploy](#migrate-before-each-deploy)).
2. Start the engine with the Postgres `DATABASE_URL`, `JAMJET_BACKGROUND=off`
   and `JAMJET_EMBED_CRON=0`.
3. Issue the scheduler's operator token and put it in the secret store.
4. Schedule a tick every `JAMJET_TICK_INTERVAL_SECS`, a minute by default.
5. Set the platform's request timeout above the tick budget plus the slowest
   engine step.
6. A few minutes after the deploy, check that `/health` says
   `"tick_overdue": false`.

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
| `DATABASE_URL` | SQLite database, as `sqlite://<path>`, or Postgres, as `postgres://` or `postgresql://` ([Postgres](#postgres-preview)) | `sqlite://.jamjet/runtime.db` in the working directory (`sqlite:///data/runtime.db` in the Docker image) |
| `STORAGE_BACKEND` | `memory` keeps state in memory and keeps no audit log; any other value uses the store `DATABASE_URL` names | the store `DATABASE_URL` names |
| `JAMJET_EMBED_WORKERS` | Runs the in-process worker pool outside dev mode | off (`1` in the Docker image) |
| `JAMJET_EMBED_CRON` | Runs the cron scheduler outside dev mode (not with `STORAGE_BACKEND=memory`; refused on Postgres) | off (`1` in the Docker image) |
| `JAMJET_BACKGROUND` | `on` or `off` (not one of the switches above): `off` starts none of the engine's loops, and a caller ticks it. See [Request-driven mode](#request-driven-mode), which lists `JAMJET_TICK_INTERVAL_SECS`, `JAMJET_TICK_BUDGET_MS`, `JAMJET_INLINE_ADVANCE_MS` and `JAMJET_TICK_ENGINE_CONCURRENCY` too | `on` |
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
# {"status":"ok","version":"0.5.0","commit":null,"commit_source":"none","features":[...],"background":"on","tick_overdue":null}
```

`commit` and `features` are described under
[On a serverless container platform](#on-a-serverless-container-platform),
`background` and `tick_overdue` under
[Request-driven mode](#noticing-that-nobody-ticks).

`/health` is the only status route that needs no token. `GET /ops/status`
needs one, of any role. A path the API does not serve answers
`200` with the web UI's HTML for `GET`, so check the status code and the
content type when you probe a route.
