# Changelog

All notable changes to JamJet will be documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
JamJet uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Security

- **Approvals are bound to the approver, the run's initiator and the exact action.**
  Behaviour changes to plan for when upgrading:
  - The MCP bridge requires a Bearer token outside dev mode, for every method.
  - The recorded approver is the API token (`token:<id>`), never a body `user_id`.
    `reviewer` tokens may now approve.
  - The token that started a run cannot approve it (403). A run with no recorded
    initiator (started in dev mode, or before this release) cannot be approved
    outside dev mode. `JAMJET_ALLOW_SELF_APPROVAL=1` turns both checks off.
    Separation of duties distinguishes tokens, not people: issue one token per human.
  - An approval covers one action: run, node occurrence, node definition and the
    payload the node is dispatched with (its `input`, for function nodes). If any of
    those change, the node asks again. The payload, not the run's materialized
    state: the two disagree on nested objects, so binding the state showed the
    reviewer one input while the node ran on another. Outside dev mode an `approved`
    decision must echo the pending request's `action_digest` (or `calls_hash` for an
    agent tool dispatch): missing is 400, stale is 409. The pending listing shows the
    payload and node the digest covers (`action_basis.payload`).
  - An agent tool dispatch approval binds the run, the occurrence and the node
    definition as well as the tool calls: its request carries an `action_digest`
    built by the same function as every other node's. A dispatch node that
    completed and is dispatched again with the same calls asks again instead of
    firing on the first approval. A dispatch approval from before this release
    (bound to `calls_hash` only) authorises nothing by itself; the node asks again.
  - The ADK dispatcher no longer takes the code it runs from run state. It used to
    import whatever `module:function` the run's `tools` map named for a call and
    follow any attribute path, and the run's state is the initiator's to seed, so
    a call named `lookup` could run a blocked `send_wire` (directly, through
    `send_wire.__call__`, or through a re-export). `jamjet.agents.tool_runtime` now
    accepts a `tools` entry only when it names a `@tool` registered under the
    called name and is exactly that tool's own `module:qualname` (what the SDK
    compiler writes); anything else is refused and nothing runs. An entry may
    still select a same-named `@tool` from another importable module, but never
    code the engine did not see: the engine matches the call on that handler and
    binds it into the approval. A call with no entry resolves through the `@tool`
    registry by name. Checking an entry may import its module (running its
    top-level code); a refused entry leaves the registry as it found it.
  - The engine also matches each call on the handler its `tools` entry names (as
    written and in dotted form) and on that function's own name, binds the handler
    into a per-call approval, and blocks the batch when an entry is not a
    `module:function` string, has a function-path segment starting with `_`
    (`send_wire.__call__`), or a module segment starting with `__`. This is
    defence in depth: a pattern still binds a name, not code, and only a catch-all
    binds an author who writes their own IR.
  - Behaviour changes for ADK users:
    - SDK-started runs carry the compiler's `tools` map, so their calls are now also
      matched on handler and function name (`blocked_tools: ["delete"]` blocks a
      tool `archive` whose function is `delete`; `payments.*` catches tools defined
      in `payments.tools`).
    - A `tools` entry must name a `@tool`. A plain function in the map, or a path to
      a tool under another name (a re-export, an alias, an attribute such as
      `.__call__`), is refused.
    - A stale entry is refused rather than silently replaced by the registry: after a
      tool module is renamed, recompile the agent so its map matches.
    - A tool whose function name starts with `_` is refused by the engine when the
      map names it; rename it.
    - A tool defined in `__main__` is refused; define tools in an importable module,
      as the examples do.
    - Tools defined inside a function (a factory or closure) and `@staticmethod`
      over `@tool` resolve when the tool is registered in the worker (the factory
      has run, or the class's module has been imported); otherwise their entry
      cannot be walked and is refused.
  - A decision carrying a `state_patch` is refused with 400. The edit never reached
    the executed action, so recording it was a false record.
  - The worker protocol evaluates the same policy and approval rules as the
    in-process worker, for every node kind: a gated or blocked node is withheld from
    `POST /work-items/claim`, and `POST /work-items/:id/complete` refuses a result
    for a gated node with no approval for its current occurrence. `POST /work-items`
    is operator-only.
  - The tenant routes are operator-only, reads included, and an operator can read or
    change only the tenant of their own token. Before this, any developer could
    `PUT /tenants/:id` and drop a tenant-level approval gate. The route/role table
    is in docs/guides/security.md.
  - Neither a run's initial input nor a worker's reported `state_patch` can write
    the engine's bookkeeping key `__budget`; `start_run` (REST, MCP and cron) and
    `POST /work-items/:id/complete` drop it. It was a way to reset a
    run's budget and to change later nodes' input outside any approval.
  - `agent_tool_dispatch: true` is refused at `POST /workflows` on any node that is
    not the ADK dispatcher (`jamjet.agents.tool_runtime:dispatch_tool_calls`), and on
    every `java_fn`. The flag used to send a node's policy straight to the per-call
    guard, so an author could flag an ordinary function, present no tool calls, and
    pass a tenant `["*"]` gate. Every node's own names are now checked first,
    dispatch nodes included; a dispatcher matched by a gate is held with an approval
    of its full payload even when it has no calls, and that approval covers its calls.
  - A tenant policy is parsed strictly. `PUT /tenants/:id` refuses a policy that is
    not an object of `blocked_tools`, `require_approval_for` and `model_allowlist`
    with the right types (400), so a misspelt field can no longer be dropped. A
    stored policy that does not parse (written by an older build or by hand) now
    blocks every node of the tenant with a rule naming the fix, on the claim route
    and in-process alike, where it used to read as no tenant policy at all.
    To fix one, `PUT /tenants/:id` with a corrected policy, or `{"policy": {}}` to
    clear it; a `null` or missing policy keeps the stored one.
  - Tenant policy now applies to nodes the embedded workers run on SQLite. The base
    SQLite backend, which the scheduler and the embedded pool use, could not read a
    tenant record, so a tenant's `blocked_tools`, `require_approval_for` and
    `model_allowlist` never applied to in-process kinds (`tool`, `mcp_tool`,
    `a2a_task`, `model`, `agent`), while the claim route applied them. Behaviour
    changes for a SQLite deployment with embedded workers and a policy on its
    `default` tenant row:
    - `model_allowlist` now applies to in-process model nodes, so a model outside it
      fails the node (and the run);
    - `require_approval_for: ["*"]` now holds every in-process `tool`, `mcp_tool`,
      `a2a_task` and `agent` node; runs started by a cron job with no recorded
      creator have no initiator, so those holds can never be approved outside dev
      mode;
    - a `default` row whose policy (or timestamp) cannot be read now refuses every
      node: a policy that does not parse blocks it (see above), and a row the
      backend cannot read at all leaves every node unavailable to retry until the
      row is fixed.
  - A node blocked by policy fails its run. Both transports settle the item and
    record `NodeFailed { retryable: false }` in one fenced commit, so the scheduler
    fails the run on its next tick. Before, a block on `POST /work-items/claim` left
    the run `Running` forever, and the in-process worker retried the blocked node
    until its attempts ran out, recording a violation each time. A node whose IR the
    engine cannot evaluate is failed the same way.
  - `python_fn`, `java_fn` and `external_fn` nodes can now be gated and blocked.
    Their policy names are the handler reference (`module:function`,
    `class/method`, `queue/handler`) and the node id. Before, these kinds had no
    name, so no pattern matched them. Behaviour change: an existing policy whose
    pattern matches one of these names, `["*"]` in particular, now gates or blocks
    these nodes where it used to let them through. The node id counts as a name,
    so a `blocked_tools` pattern written for tools can now hit a function node:
    `blocked_tools: ["delete_*"]` blocks a `python_fn` node with id
    `delete_temp_files`. Because a blocked node now fails its run (see below), such
    a run fails where it used to complete. Check `blocked_tools` patterns against
    your function node ids before upgrading. Python and Java handlers also
    match in canonical dotted form, so `payments.transfer:send` and
    `payments:transfer.send` are one name. This also widens existing patterns,
    toward refusing: the module/function boundary disappears, so
    `blocked_tools: ["db:drop*"]` now also blocks `db.dropdown:load`, and
    `myapp.payments:*` covers every submodule (`myapp.payments.audit:log`). Since a
    block fails the run, review handler patterns before upgrading. A pattern still
    binds a name, not code: a
    re-export reaches the same function under another path, so only a catch-all
    binds an author who writes their own IR.
  - `mcp_tool` nodes are now matched on their tool. The policy engine read a
    `tool_name` field the kind does not have, so no `blocked_tools` or
    `require_approval_for` pattern ever applied to an `mcp_tool` node. Behaviour
    change: an existing pattern that names an MCP tool now gates or blocks it.
  - Node ids must be 1 to 128 ASCII letters, digits, `_`, `.` or `-`, and a node's `id`
    must equal its key in `nodes`; `POST /workflows` refuses anything else (400). Node ids are author-chosen and are printed into the
    approve command a reviewer pastes. Behaviour change for IR with other ids; every
    id the SDK compilers generate, and every example in this repo, already conforms.
  - A registered workflow version is immutable: an identical re-register is a 200
    no-op, a different IR under the same version is a 409. Identity is the
    canonical content hash, so key order and whitespace do not matter.
    For anything that re-registers its workflows at boot (PAS does): an edited
    graph under an unchanged version now fails boot with 409, where before the
    new IR silently replaced the old one. The first boot after upgrading also
    fails if the database already holds a copy of an unchanged version that
    differs from what the code now builds (a past edit made without a version
    bump). Remedy: bump the workflow's version, which is the supported fix and
    keeps runs of the old version readable; or, if no run of that version still
    needs its old definition, delete the stale row
    (`DELETE FROM workflow_definitions WHERE tenant_id = ? AND workflow_id = ? AND version = ?`;
    PAS runs in the `default` tenant)
    and boot again.
  - The embedded cron scheduler starts runs in-process instead of POSTing to
    `/executions` without a token, which failed with 401 outside dev mode. A cron
    job records the token that created it (`created_by`, migration 0013), and its
    runs take that token as initiator, so the creator cannot approve them. Jobs
    created before this upgrade have no creator; their runs cannot be approved
    outside dev mode until the job is recreated.
  - `jamjet-timers` public API changes (a breaking release for that crate, so the
    next crates.io publish needs a major-compatible bump, 0.5 to 0.6):
    `CronStore::delete` now takes `(tenant_id, name)` and `CronStore::disable` takes
    the job id; `CronStore::list_for_tenant` is new; `CronJob` gains `created_by`
    and `tenant_id`; `HttpCronLauncher` and `CronScheduler::new` are removed.
  - The unused HTTP cron launcher (`HttpCronLauncher`, `CronScheduler::new`) is
    removed. It POSTed `/executions` with no token, so it failed outside dev mode,
    and giving it a token would have made that service the initiator of every cron
    run. Cron runs through the server's embedded scheduler.
  - Cron jobs belong to a tenant. `POST`, `GET` and `DELETE /cron` act on the
    caller's tenant only, a job name is unique per tenant (migration 0013 rebuilds
    `cron_jobs` with `UNIQUE (tenant_id, name)`), and a job's runs start in its own
    tenant. Before, jobs were global: any tenant could list them with their input,
    and replace or delete one by name, and every run started in `default`.
  - The runtime is single-tenant, and now says so. On SQLite token validation
    never read a token's tenant and put every token in `default`; it now reads
    it, and a token of any other tenant is refused with 403 ("multi-tenant
    tokens are not supported by this runtime yet"), on REST and MCP. The API
    layer is tenant-scoped but the engine under it is not, so such a token could
    register a workflow version that the default tenant's runs were scheduled
    from, and its own runs lost their tenant and tenant policy after the start
    node. `JAMJET_TENANT_TOKENS_AS_DEFAULT=1` (read once at startup) restores the
    old behaviour, where such a token acts in `default`; it gives no isolation.
    Upgrade note: a deployment that inserted non-default tokens by hand gets 403
    for them until it sets that variable or moves the tokens to `default`.
    The embedded cron launcher follows the same rule: a job in any other tenant
    starts no run (a warning is logged at each due time), unless the opt-out is
    set, in which case it runs in `default`.

- **Approval requests can expire.** A tenant, workflow or node policy may set
  `approval_timeout_secs` (1 s to 1 year) and `escalate_after_secs` (shorter, same
  policy). Every level is a ceiling: the shortest timeout and the shortest escalation
  any of tenant, workflow and node sets apply, so a workflow or node can only shorten
  the tenant's deadline (an escalation that would not come first is dropped and
  logged; the timeout is never extended). A request records `expires_at` and `escalate_at` (UTC) in its context, and the pending
  listing shows them. When a request expires, the scheduler records `approval_expired`
  and treats it as a rejection by `system:deadline`: the node and the run fail as on a
  rejection; nothing is ever approved by a deadline. At `escalate_at` it records
  `approval_escalated` once, for notifiers. A decision after the deadline is 409 and
  records nothing. A decision and the expiry of one request cannot both land: each
  appends through the new `StateBackend::append_event_if` (a trait method a custom
  backend must implement) under the condition that the request is still pending.
  Invalid deadlines are refused at registration and at `PUT /tenants`. The dead
  `timeouts.approval_timeout` field is removed and a non-null value is refused when a
  NEW version is registered, since nothing ever enforced it; a version already stored
  with it still re-registers idempotently (and still has no deadline: register a new
  version with `approval_timeout_secs` to get one).

- **Approval requests are sealed into the audit chain.** Only the decision was sealed;
  the request a worker or the claim route raised was in the event log alone, so what
  was asked could be rewritten unnoticed. Every `tool_approval_required`,
  `approval_expired` and `approval_escalated` is now sealed into the signed,
  hash-chained audit log with its action digest and deadline. Requests are recorded
  through one helper shared by both guards and both transports; the in-process worker
  pool and the scheduler take the server's `AuditEnricher` (`with_sealer`) through the
  new `ApprovalSealer` trait. Every record is sealed under the run's own tenant; the
  scheduler reads it from the execution row through the new
  `StateBackend::execution_tenant` (a trait method a custom backend must implement). A
  seal that fails is recorded in the run's log as `audit_seal_failed` (naming the
  unsealed event) and counted (`AuditEnricher::seal_failures`), since `verify_chain`
  cannot see an entry that was never written. `ApprovalSealer::seal_approval_record`
  returns a `Result`.

- **Clients can approve what they reviewed.** `jamjet approvals` prints each pending
  request's digest, what it covers and the exact approve command (`--show-basis` prints
  the bound payload). The command is printed alone on its line with every argument
  shell-quoted, and is omitted for a value holding control characters, so it is safe to
  paste. The Java SDK's `approve` overload that takes an action digest is listed under
  the Java SDK changes below.

- **The tool-effect idempotency key is derived by the engine, not taken from the request
  body** (#130). `POST /work-items/:id/complete` persisted the caller's
  `idempotency_key` verbatim, so a worker holding item B's lease could complete B while
  echoing item A's key: the backend filed B's output under A, and a later replay for A
  returned a result A never produced. The key is now derived from the coordinates the
  engine recorded for the item — `StateBackend::get_work_item` was added so the route
  can ask rather than believe the body, and the tenant-scoped backend filters that
  lookup by tenant.

  A worker that omits the field now gets replay coverage anyway, since the engine
  derives it either way.

  Derivation runs before `commit_turn` appends the `NodeCompleted`, so a linear run
  reproduces the claim's key exactly. A node running beside a sibling whose
  `state_patch` landed in between derives a different key and loses replay exactness —
  already v1's documented posture (`F-2c-3`), and why a mismatch warns rather than
  failing the completion.

- **Dropped `hyper 0.14` and `h2 0.3.x` from the tree** (RUSTSEC-2026-0258, h2 unbounded
  empty DATA frames). The OTLP stack moved from opentelemetry 0.22 / tonic 0.11 to
  opentelemetry 0.32 / tonic 0.14, which was the only thing holding the pre-1.x hyper
  line in. The `cargo audit` suppression is removed, and the unused workspace `tonic`
  dependency is gone with it.

  The suppression's own risk note was too generous: it read the advisory as not
  flagging the `h2 0.4.15` used by the inbound-facing servers, but the advisory patches
  at `>= 0.4.16` with no unaffected range, so those were in scope too. The tree now
  resolves `h2 0.4.18`.

  CI's tripwire is replaced rather than deleted: it used to police the *exception*, and
  now asserts the *fix* — `h2 0.3.x` and `hyper 0.14` must stay out of the tree, so a
  future dependency bump cannot quietly bring the line back.

- **A log holding an event kind this build cannot read fails closed.** `EventKind`
  gains a `#[serde(other)]` `Unknown` variant, so such a log still loads and lists
  (`GET /executions/:id/events` shows the event as `unknown`). Nothing acts on it: the
  approval state of every node in that run is indeterminate, so no approval covers
  anything, no request is raised and no decision is accepted (409, "event log contains
  event kinds this build cannot read"), and the scheduler does not advance the run,
  recording `run_stalled` once. Downgrade note: an older build refuses to act on a log
  a newer build wrote rather than ignoring what it cannot read. Builds from before
  this release cannot load a log holding `approval_expired`, `approval_escalated`,
  `audit_seal_failed` or `run_stalled` at all, so do not roll back past this release on
  a store a newer build has written.

### Fixed

- **Runtime: an unmatched API request is a JSON 404, not the web UI.** The SPA
  fallback answered every unmatched path, for every method, with `200 text/html`
  (`index.html`). A client on a wrong path saw a success, failed to parse the body,
  and the integrations above it logged a warning and carried on with no durability
  and no audit. Any non-GET request, and any path under `/api/`, now gets
  `404 {"error": "not found", "hint": "..."}`, with a hint naming the real routes.
  A GET or HEAD page load still serves `index.html`, and no route was added.
- **Web companion: fetch the runtime's real routes and read their real shapes.** It
  requested `/api/executions`, which the runtime has never served, so every fetch got
  HTML and the list read "No executions". It now uses the bare paths, and its types
  match what the runtime sends: executions carry `execution_id` (not `id`), the event
  routes answer `{"events": [...]}`, and the coordinator and agent-tool event fields
  follow `runtime/state/src/event.rs`. A response of the wrong shape becomes a query
  error, and each pane has an error boundary, so a render error shows a message in
  that pane instead of a blank page. In auth mode the companion still sends no token
  and shows the 401.
- **Java SDK (`jamjet-sdk` 0.5.1): `JamjetClient` now drives the runtime.** Every call
  went to `/api/v1/...`, which the runtime has never served, so only `health()`
  worked. All calls now use the runtime's routes, and several request shapes were
  wrong beyond the prefix:
  - `createWorkflow` sends `{"ir": ...}` instead of the raw IR.
  - `startExecution` posts `{workflow_id, workflow_version?, input}` to
    `/executions`, not to `/workflows/{id}/execute`. A new overload takes the
    workflow version.
  - `sendExternalEvent` posts to `/executions/{id}/external-event`, not `/events`.
  - `listWorkflows()` is deprecated and throws `UnsupportedOperationException`
    without calling the runtime, which has no route for it.
  - New `getApprovals(executionId)` reads `GET /executions/{id}/approvals`, and a new
    `approve(executionId, decision, nodeId, actionDigest, comment)` overload sends the
    reviewed action's digest.
  - IDs are percent-encoded as path segments, and an HTML reply is reported as a
    wrong base URL or path instead of a JSON parse failure.
  - A failed call's exception message carries the runtime's reason (its `error`
    field, or the raw body text, capped at 500 characters), so `jamjet` CLI errors
    read `JamJet API error 404 on /executions: workflow research v1.0.0` instead of
    stopping at the status. The full body is still on `JamjetApiException.body()`.
  - The `User-Agent` is `jamjet-java-sdk/<version>` again, with the version Maven
    stamped into the build, and `jamjet -V` prints that version too (it said `0.1.0`).

  `jamjet-cli` picks these up, so `jamjet executions` and `jamjet agents` now reach
  the runtime, and `jamjet run` starts an execution of a registered workflow. `run`
  gains `--workflow-version`: without it the runtime runs version `1.0.0`, but the
  Java `Workflow` builder defaults to `0.1.0`, so a builder workflow needs
  `--workflow-version 0.1.0` (or `startExecution(id, ir.version(), input)` in code).
  A started Java workflow does not execute its steps on the runtime: they are queued
  for a `python_tool` worker, and there is no Java worker. A new CI job runs the
  client against a runtime built from the same commit.
- **A run's start node is scheduled once.** Starting a run was four writes (the
  execution row, `WorkflowStarted`, the start node's `NodeScheduled`, its work item),
  and a scheduler tick landing between the first and third saw a live run with its
  start node unscheduled and scheduled it again, so the start node ran twice under
  load. The four are now one write, `StateBackend::start_execution_atomic`, on every
  backend (a new trait method: a custom backend must implement it). A run whose start
  node does not exist is now refused before anything is written.

---

## 0.5.0 - 2026-08-22

Durability and enforcement hardening across the engine seam. Everything below is
about the boundary where an external worker meets the engine — the path that
carries every ADK tool call, since `python_tool` and `java_tool` nodes run with
no in-process worker.

### Fixed

- **A worker-reported tool failure no longer strands its workflow.** `POST /work-items/:id/fail`
  wrote a status no sweep selects and emitted nothing, so the scheduler kept the node
  scheduled and the execution never reached a terminal state. It is now fenced and
  emits `NodeFailed` / `RetryScheduled` with real retry semantics.
- **`/work-items/:id/complete` settles and emits in one transaction.** It was four
  disjoint operations; a crash between the settle and the append wedged the execution
  permanently.
- **A forged zero lease fence can no longer complete an unclaimed item.** `commit_turn`
  had no `status = 'claimed'` guard and pending rows carry `lease_fence = 0`.
- **Duplicate completions closed on the external HTTP path**, which was unfenced while
  the in-process path was not.
- **Approval-hold settles are fenced**, and a zombie `NodeStarted` from a worker whose
  lease was stolen no longer wedges a run forever.
- **Claim-side lease expiry consumes an attempt** and the reclaim writes are guarded, so
  an item cannot retry indefinitely without counting.
- **Fail closed when a tenant record is unreadable** rather than resolving to allow.
- **A provider-only allowlist entry matches on the durable path**, so `["anthropic"]`
  means the same thing in-process and durable.
- **NaN cost no longer slips past a budget ceiling.**

### Added

- **Reserve-before-fire.** An idempotency key is reserved before its effect fires, with
  a lease-like TTL; a worker that loses the race retries once, then polls for the
  winner's result.
- **Idempotency keys for the external tool tier.** The claim derives and hands out the
  key, the worker echoes it on completion, and the effect is recorded against it — so
  the replay guard covers both transports rather than the in-process one only. Both
  paths derive the key through one shared function.
- **`java_tool` queue and `JavaFn` node**, enabling durable Java tool execution.

### Changed

- Workspace crates move to 0.5.0 together; sibling dependencies are pinned exactly.

---

## 0.10.2 - 2026-06-12

### Fixed

- `jamjet dev`: server binary cache is now keyed by SDK version (`~/.jamjet/bin/<version>/jamjet-server`) so upgrading via `pip install -U jamjet` always fetches the matching runtime binary. Previously a cached binary was reused indefinitely regardless of SDK version.
- `jamjet approvals` table: title uses `-` instead of an em dash; missing comment cells render as empty instead of an em dash character.

---

## 0.10.0 - 2026-06-12

### SDK

- `jamjet approvals EXECUTION_ID` - table of pending (node_id, tool_name, approver) and decided (node_id, status, user_id, comment) entries; friendly empty-state message when nothing is on record.
- `jamjet approve EXECUTION_ID --decision approved|rejected` - posts to the runtime approve endpoint, prints resolved node_id on success; `--node-id` and `--comment` are optional. Validated decision values give a clean parameter error instead of a traceback. HTTP 4xx from the server prints the `error` field and exits 1 without a traceback.
- `client.list_approvals(execution_id)` - GETs `/executions/{id}/approvals`, returns `{"pending": [...], "decided": [...]}`.
- `client.approve()` gains an optional `node_id` parameter; the runtime infers the node when exactly one approval is pending.
- `jamjet --version` reads the installed package version via `importlib.metadata` instead of the hardcoded string.

---

## Runtime 0.4.0 - 2026-06-12

### Added
- Approval/HITL loop end-to-end: nodes gated by `require_approval_for` policy park durably; `POST /executions/:id/approve` resumes or fails them through the normal scheduler path; rejected approvals fail closed with decider and comment recorded in the event log.
- `GET /executions/:id/approvals` returns `{"pending": [...], "decided": [...]}` with node_id, tool_name, approver, context, and sequence for each entry.
- `jamjet_approve` MCP tool routes through the same approve path.
- Validated approve API contract: 409 when nothing is pending or already decided or the execution is terminal; 400 when the node is ambiguous (multiple pending without a node_id); response includes the resolved node_id.
- OTel GenAI semantic conventions emitted on model-node spans (model, system, input/output tokens, finish reason).
- SQLite `BEGIN IMMEDIATE` on read-then-write transactions to prevent SQLITE_BUSY under concurrent worker load.

---

## 0.8.3 — 2026-05-11

### Added
- `jamjet.integrations.openai_guardrail` — JamJet policy as an OpenAI Agents SDK tool guardrail. Same `policy.yaml`, same v1 audit JSONL as `@jamjet/claude-code-hook` / `@jamjet/mcp-shim` / `@jamjet/openai-guardrail` (TS).
- New exceptions: `JamjetPolicyBlocked`, `JamjetApprovalRequired`.

### Notes
- Approval flow surfaces as `JamjetApprovalRequired` exception in v0.1. Full approval flow with `jamjet approve <run-id>` integrates with the OpenAI Agents SDK approval API in v0.2.

---

## 0.8.2 — 2026-05-11

### Added
- `jamjet demo` audit events now emit the v1 portable schema: `adapter: "python-sdk"`, `host: "python"`, `schema_version: 1`, `policy_version: "1"`, `rule_kind`. Plus `args`, `server`, and the `ts` field (replaces `timestamp`).
- Audit events from `jamjet` now share the same JSONL shape as `@jamjet/claude-code-hook`, `@jamjet/mcp-shim`, and `@jamjet/cloud` — `cat ~/.jamjet/audit/<YYYY-MM-DD>/*.jsonl` gives a unified view across every JamJet adapter.

### Changed
- Internal `DemoAuditEvent.timestamp` field renamed to `ts`. The `timestamp` key is preserved on serialized output as an alias for backward compatibility with anything reading jamjet 0.8.1 audit JSON.

### Notes
- Foundation for Phase 2's "one policy, one audit trail" claim. Other JamJet adapters (claude-code-hook, mcp-shim, openai-guardrail) live in [`jamjet-labs/jamjet-policy`](https://github.com/jamjet-labs/jamjet-policy).

---

## [0.8.1] — 2026-05-11

### Added
- `jamjet demo` CLI: 4 zero-setup safety demos — `unsafe-tool-call`, `approval`, `budget-cap`, `mcp-tool-policy`.
  All demos run with no API key, no Docker, no cloud. Mock agent named `DeterministicDemoAgent`. Every output ends with "The model is mocked. The enforcement path is real."
- `--json` flag on every demo emits a machine-readable audit event.
- Audit events written to `.jamjet-demo/runs/<run-id>.json` for inspection.
- Examples 01–04 in `examples/` mirror the demo CLI commands.

### Changed
- PyPI description rewritten from "agent-native runtime and framework" to "safety layer for AI agents".
- README hero rewritten to lead with the 60-second safety demo.

### Notes
- This release is SDK-based. **JamJet Gateway** — an MCP proxy applying the same policy to MCP traffic from Claude Desktop, Cursor, and other MCP clients — is the next major milestone. See [jamjet.dev/gateway](https://jamjet.dev/gateway).

---

## [0.1.0] — 2026-03-08

First public release. JamJet is a performance-first, agent-native runtime for AI agents — built in Rust, authored in Python.

### Runtime Core (Rust)

- **Durable graph execution** — event-sourced state machine; crash the process and execution resumes exactly where it stopped
- **IR (Intermediate Representation)** — typed graph compiled from YAML or Python; both authoring surfaces compile to the same IR
- **Rust async scheduler** — Tokio-based worker pool with distributed lease semantics; prevents double-execution across multiple runtime instances
- **7 node types**: `model`, `tool`, `http`, `branch`, `parallel`, `wait`, `eval`
- **State machine** — typed state schema (JSON Schema), per-node state patches, full event log
- **PostgreSQL + SQLite** — production-grade durable storage with automatic schema migration; SQLite for local dev (zero config)
- **Event sourcing + snapshots** — every state transition recorded; periodic snapshots for fast recovery
- **REST API** — submit executions, poll status, resume waiting nodes, inspect full event timeline
- **OpenTelemetry** — spans for every node execution, GenAI semantic conventions for model calls, Prometheus metrics

### Protocol Layer

- **MCP client** — connect to any MCP server over `stdio` or `http+sse`; tool discovery, typed invocation, auto-retry
- **MCP server** — expose your agent's tools and resources to external MCP clients
- **A2A client** — discover external agents via Agent Cards, delegate tasks, stream SSE progress
- **A2A server** — publish an Agent Card at `.well-known/agent.json`, accept tasks from any A2A-compatible agent, stream events

### Agent System

- **Agent Cards** — machine-readable identity for every agent (id, name, capabilities, input/output schema, endpoint)
- **Agent registry** — local registry for managing and discovering agents
- **Autonomy modes** — `guided`, `supervised`, `autonomous` with configurable cost and iteration limits

### Eval Harness

- **`eval` node type** — inline quality scoring in any workflow; LLM-judge, assertions, latency, cost scorers
- **`on_fail: retry_with_feedback`** — scorer feedback auto-injected into the next model call
- **Python eval runner** — `EvalDataset`, `EvalRunner`, `BaseScorer` for custom scorers
- **`jamjet eval run`** — CLI command for batch eval; JSONL dataset support, rich table output, `--fail-below` for CI gates

### Python SDK

- **Decorator API** — `@workflow`, `@node` for building workflows as Python classes
- **Builder API** — `WorkflowBuilder`, `ModelNode`, `ToolNode`, `BranchNode`, `ParallelNode`, `EvalNode`
- **`JamJetClient`** — async client for submitting, polling, inspecting, and streaming executions
- **Full type stubs** — compatible with mypy strict mode and Pyright

### CLI

- `jamjet init [name]` — scaffold new project or add JamJet to existing project (like `git init`)
- `jamjet dev` — start local runtime with SQLite; zero config
- `jamjet validate <workflow>` — compile and validate a workflow file without running it
- `jamjet run <workflow> --input <json>` — submit and wait for completion
- `jamjet run ... --stream` — stream execution events as they happen
- `jamjet inspect <exec-id>` — full execution state, event timeline, token usage, cost
- `jamjet ls` — list recent executions with status and duration
- `jamjet resume <exec-id>` — resume a waiting or failed execution
- `jamjet cancel <exec-id>` — cancel a running execution
- `jamjet tools list / call` — inspect and test MCP tool servers
- `jamjet agents inspect` — inspect A2A agent cards
- `jamjet eval run` — run a batch eval dataset

### Packaging

- Python wheel bundles the `jamjet-server` Rust binary (maturin) — `pip install jamjet` gives you everything
- Platform wheels: macOS arm64, macOS x86_64, Linux x86_64, Linux aarch64, Windows x86_64
- Published to PyPI via GitHub Actions on version tags

### Developer Experience

- 8 ready-to-run examples in [jamjet-labs/examples](https://github.com/jamjet-labs/examples)
- Full documentation at [jamjet.dev/docs](https://jamjet.dev/docs/quickstart)
- `justfile` for common dev tasks (`just test`, `just lint`, `just build`)

---

## [0.2.0] — Planned

- Go SDK
- TypeScript SDK
- Hosted runtime plane (zero-ops deployment)
- Enhanced policy engine with multi-tenant isolation
- NATS/Kafka queue backend for high-throughput workloads

---

[Unreleased]: https://github.com/jamjet-labs/jamjet/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jamjet-labs/jamjet/releases/tag/v0.1.0
