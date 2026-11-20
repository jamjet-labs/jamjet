# Changelog

## Unreleased

### Added

- **`JamjetClient(auth=...)`, an auth hook for an engine behind a platform identity
  check.** The hook adds its own header, and the JamJet token keeps `Authorization`.
  `jamjet.auth.GoogleIdTokenAuth` gets a Google ID token and sends it in
  `X-Serverless-Authorization`. It needs `pip install 'jamjet[google-auth]'`.
  `jamjet.auth.TokenSourceAuth` does the same for any token source.
- **`PlatformAuthError`.** A 401 or 403 that does not carry the engine's error body came
  from something in front of the engine. It is a subclass of `JamjetAPIError`, so existing
  handlers still catch it.
- **Retries are on by default, and `RetryPolicy.off()` turns them off.** A call that was
  never sent is sent again. A call that is safe to repeat is sent again. An unsafe call
  that may have taken effect raises `OutcomeUnknown` and is never sent again. The CLI
  commands that start, approve or activate say so and exit 1.
- **`jamjet ops tick`** runs one tick on an engine that has its background loops off. It
  needs an engine whose `/health` lists `ops_tick`.
- **`client.list_pending_approvals(limit=None, after=None)`** lists pending approvals across runs
  from `GET /approvals`. Against an engine without that list it raises `JamjetAPIError` and asks
  for an upgrade. `approve()` returns the engine's `repeat`, `sequence` and `waiting_for`.
- **`approve(request_sequence=...)`** names the request a decision answers. The engine refuses
  the decision with 409 if the node has asked again since, so a retry cannot decide a newer
  request.

### Changed

- **Credentials go only to the engine's own address.** The JamJet token and anything an
  auth hook adds are sent to the scheme, host and port of `base_url` and nowhere else. A
  request to another address raises `CredentialScopeError` before anything is sent.
  Redirects are raised, not followed.
- **An unsafe call whose answer was lost raises `OutcomeUnknown`.** For `start_execution`,
  `approve` and every other call that is not safe to repeat, a 5xx or a lost answer (a
  timeout or a broken connection) now raises `OutcomeUnknown` in place of `JamjetAPIError`
  or an httpx error. It is a `RuntimeError`, not an httpx error, so
  `except httpx.HTTPError` does not catch it. The original error is on `__cause__`.
  Import it from `jamjet.transport`.

### Fixed

- **A JamJet token that cannot go in a header is refused, and not shown.** A token with
  a line break, a NUL or a space at one end, as a token read from a file often has, made
  every call fail with an httpx error that quoted the whole token, and `jamjet` commands
  printed it. The client now refuses such a token, and one with any other control
  character, with a `ValueError` when it is built. An `auth` hook's value of that kind is
  refused before the request is sent. A transport error no longer quotes a credential,
  and a request httpx could not write is raised at once, not sent again and not reported
  as `OutcomeUnknown`.
- **`jamjet dev` names the extra it needs.** Without uvicorn it told you to run
  `pip install 'jamjet'`: Rich read `[sidecar]` as a style tag and dropped it. It now
  prints `pip install 'jamjet[sidecar]'`.

## 0.13.0 - 2026-11-18

An agent built with JamJet now runs end to end with a real model or with no API key,
and every step of it can be followed and checked from the command line. The engine
binaries for this release are on the `python-sdk-v0.13.0` GitHub release, built from the
same commit. `jamjet dev` downloads the ones that match the installed SDK and checks
them against `SHA256SUMS`. Engine changes are listed under Unreleased in the root
CHANGELOG.

### Security

- **`jamjet dev` checks the engine it downloads.** It fetches the release for the
  installed SDK version, not the newest release, and refuses a binary whose SHA-256 does
  not match the release's `SHA256SUMS`, or a release with no `SHA256SUMS`.
  `JAMJET_SERVER_PATH` names a binary to use instead.

### Added

- **A scripted model, for running an agent with no API key.** `model="scripted/<path to
  YAML>"` answers each model call from the file. Only the model is scripted: policy,
  approval, budget and durability are the real ones. It needs no key and no `[model]`
  extra. Durable runs also need `jamjet[sidecar]`.
- **`jamjet executions`** lists runs, and `AgentResult.execution_id` gives a durable
  run's id. Errors from `run_durable` carry it too.
- **`jamjet approvals` with no id** lists every run waiting for approval, each with a
  command you can paste to approve it.
- **`jamjet approve <id> -d approved --as <name>`** records who approved. A dev-mode
  engine records the name you give; with an API token the engine records the token's
  name.
- **`jamjet audit <id>`** prints a run's audit trail, or JSON with `--json`: what the
  model asked for, what was blocked, who approved and when, and what ran.
- **`run_durable` waits through an engine restart.** It says on stderr when it loses the
  engine and when it is back. If the engine does not come back within
  `timeout_seconds`, it raises `TimeoutError` (with the connection error as its cause)
  instead of `httpx.ConnectError`. A worker killed in the middle of a tool still fails
  the run.
- `jamjet dev --db PATH` creates the folder for the database.

- **`agent_ref`: attribute a workflow or node to a registered Agent Card.**
  `Agent(..., agent_ref="jamjet://acme/researcher")` emits it on the durable path's
  workflow IR; YAML takes `agent_ref:` in the `workflow:` header (and in an agent-first
  `agent:` block or a fleet unit) and `acts_for:` on a node. The engine then holds the
  workflow's nodes to that card's `allowed_tools`, `blocked_tools`,
  `require_approval_for` and `allowed_delegations`, and refuses them if the agent is not
  registered. A non-string or empty value is refused at compile time.

- **`jamjet.runtime_audit`: verify the runtime's audit log offline.** With the runtime
  in Ed25519 mode (`JAMJET_AUDIT_SIGNING_MODE=ed25519`), `verify_entries` checks each
  entry of a `GET /audit` export against the public key alone: the recomputed entry hash,
  the Ed25519 signature and, oldest first (`chain_order`), the links.
  `public_key_from_did_document` reads the key the runtime publishes in
  `/.well-known/did.json`. Load the export with `load_entries`, which keeps each number
  as the runtime wrote it; a float parsed and printed again can change spelling and fail
  the hash. Small-order or non-canonical keys, small-order signature `R` values and
  fields Rust would not read (wrong JSON types, non-canonical UUIDs or hex) are refused.

- **Approval deadlines in compiled policies.** A YAML workflow or node `policy:` and an
  `Agent(policy={...})` dict may set `approval_timeout_secs` and `escalate_after_secs`.
  The compiler checks them as the runtime does (1 s to one year; escalation needs a
  timeout in the same policy and must be shorter) and emits them only when set. They
  were refused as unknown keys (YAML) or dropped (Agent).

### Changed (breaking)

- **`approval_required` now raises on `agent.run()` instead of warning.** It emitted a
  `UserWarning` and then ran every tool ungated, so the knob read as configured and did
  nothing. A warning cannot carry that: Python prints a given one once per location, and
  `-W ignore`, `PYTHONWARNINGS` or a pytest config drop it entirely — the caller who most
  needed the signal was the least likely to see it. `run()` executes tools with no policy
  engine between the model and the call, so a gate can be neither evaluated nor held
  there; it now refuses with `ApprovalNotEnforceableError` and points at `run_durable`,
  where the engine decides before any worker receives the payload. To run without gates
  deliberately, drop `approval_required`.

- **`Model()` builds the default governed chain instead of an empty one.** Constructing
  the seam with no `middleware` produced an empty chain, so the call reached the provider
  with no PII redaction, no metering and no budget, one line below a module docstring
  calling it "the single governed path for every model call". Every in-tree call site
  already passed `default_model_middleware()`, so this was never a live hole in JamJet's
  own paths. It was the public default, and the trap was the next caller who wrote
  `Model()` and reasonably assumed the seam governed. `Model(middleware=[])` now raises
  `ValueError` rather than quietly meaning the same thing; `Model(ungoverned=True)` is
  the way to ask for no chain, and it has to be asked for by name.

  Scope, stated narrowly: with no `GovernanceConfig` the default chain is an allow-all
  allowlist, PII redaction, a no-op budget and a metering recorder. So what it closes is
  a PII-leak default. It does not add model restriction where no policy exists, and the
  metering half is bookkeeping rather than enforcement: `MeteringMiddleware` is built with
  no sink, and nothing in the package reads `.records`, so spend is recorded into the
  instance and discarded with it.

  `stream()` runs only the `before` chain, so a streamed call through a bare `Model()` is
  redacted but not metered, and an all-streaming workload will not trip a budget. That
  asymmetry predates this change and is unchanged by it.

  Two behaviour changes for anyone who was constructing a bare `Model()` on purpose.
  Redaction rewrites `ModelRequest.messages` in place, so the caller's own request object
  comes back redacted. And redaction is fail-closed on content it cannot traverse, so a
  call carrying provider SDK objects inside `tool_calls` now raises
  `ModelDeniedError(code="pii_unredactable_content")` where it previously succeeded; the
  JSON-shaped form of the same conversation is unaffected. `Model(ungoverned=True)`
  restores the old behaviour exactly.

  `Model(middleware=...)` now also rejects non-`ModelMiddleware` elements with `TypeError`
  at construction. Previously `Model(middleware="pii")` iterated the string into
  `['p','i','i']` and failed later with a bare `AttributeError` at the provider boundary.

- **`jamjet worker --lease-secs` rejects values outside 5..3600.** The CLI now fails
  on a value below 5 or above 3600 instead of sending it and letting the engine clamp it
  silently to a lease the worker never asked for.

- **`jamjet worker` runs sync `@tool` handlers in a thread.** A plain `def` handler now
  runs via `asyncio.to_thread`, so a blocking handler no longer stalls the event loop and
  the lease heartbeat with it. Async handlers run on the loop as before. A sync handler
  that touches state bound to the loop's thread must now do so thread-safely.

### Fixed

- **A durable agent keeps its tool results.** The engine passed the model only each
  message's role and text, so a real model never saw its own tool calls and repeated
  them.
- **Plain `def` tools work with `@tool`.** They run in a worker thread. To use the event
  loop, write the tool as `async def`.
- **`@tool` parameters get real JSON Schemas** for lists, `Optional`, `Literal`, dicts
  and Pydantic models.
- **LiteLLM's debug banner is off** unless `JAMJET_LITELLM_DEBUG=1`.
- **`blocked_tools` is enforced on `agent.run()`.** It was enforced only on the durable
  path, so a tool the policy forbids ran normally in-process — silently, without even the
  warning `approval_required` gave. `blocked_tools` is a *tool* control, so the seam
  middleware chain could never carry it: that chain sits at the *model* boundary and
  enforces budget, allowlist and PII. Blocked tools are now dropped before the strategy
  runner sees them, which makes them neither offered to the model nor dispatchable, and
  patterns are globbed with the same matcher the engine uses. Both paths resolve the
  effective policy through one shared function, so an `Agent(...)` cannot mean one thing
  in-process and another durable.

## 0.12.0 — 2026-08-22

Enforcement and exactly-once correctness. The ADK's governance knobs were
enforceable on paper but reachable around in practice; this closes that, and
fixes three defaults that quietly meant the opposite of what they read.

### Fixed

- **Tool policy and approval are now enforced on ADK agent dispatch.** `blocked_tools`
  and `require_approval_for` were evaluated on ordinary tool nodes but not on the
  agent's own dispatch node, so an ADK agent's tool calls bypassed both. An approval
  is bound to a content hash of `(name, arguments)`, so it cannot be replayed against
  a different payload.
- **A spent budget refuses to fire.** Previously a run that had already exhausted its
  budget could still make one more model call.
- **`max_cost_usd` no longer defaults to `1.0`.** The default doubled as the "unset"
  sentinel, so asking for a `$1.00` ceiling was indistinguishable from asking for
  nothing and was silently dropped. The default is now `None` — no implicit ceiling —
  and the governance knobs track explicitly-set values separately.
- **The model allowlist globs in-process, matching the durable engine.** `["*"]` — the
  natural way to write "allow everything" — denied every model in-process while being
  accepted durable-side, and a provider-only entry like `["anthropic"]` was the mirror
  image. Both transports now share one matcher, pinned by a parity table.
- **`Team` no longer overrides a governance value a sub-agent set explicitly.** It
  compared values rather than tracking what was set, so a sub-agent that deliberately
  chose the coordinator's default had that choice overwritten.

### Added

- **External tool effects carry an idempotency key.** `complete_work_item` accepts and
  echoes the key the claim hands out, so the engine records the result and a re-run
  replays it instead of firing the tool a second time. Without it nothing was recorded
  and every replay re-fired — on the transport ADK agents actually take, since
  `python_tool` nodes run with no in-process worker.

## 0.11.0 — 2026-06-29

The full ADK feature set. A plain `Agent` is now durable and governed by default, with first-class sessions, memory, multi-agent teams, deploy, and a five-minute dev loop.

### Added

- Governance on by default. `Agent(policy=, approval_required=, budget=, pii=, audit=, receipts=)`: a model allowlist, fail-closed budget caps, PII redaction at the model seam, a signed hash-chained audit record per action, and an AgentBoundary receipt per turn. Enforced on both `run()` and `run_durable()`.
  (Corrected later: approval was **not** enforced on `run()` — it warned and ran ungated. See the Unreleased entry above.)
- Sessions and memory. `Session` + a persistent `SessionStore` continue a conversation thread across runs and restarts; `memory=` wires the Engram bridge with an automatic, governed retrieve/record loop keyed by the session.
- Team multi-agent API: `Sequential`, `Parallel` (with `MergeStrategy`), `Team` (coordinator), and `Loop`. Each sub-agent runs as its own governed durable execution; `run()` / `run_durable()` return a `TeamResult` with per-agent isolation.
- `agent.deploy(runtime="local" | "self-host" | "cloud" | "<url>")` and `Team.deploy(...)`: ship the same compiled IR to any runtime over the durable engine. Artifacts API (`POST/GET /artifacts`).
- Devtools: `jamjet create` scaffolds a runnable project, `jamjet dev` runs the whole local stack (model sidecar + engine + worker) with one command, and `jamjet eval` adds deterministic trajectory evaluation (`jamjet eval trajectory-diff`).

## 0.10.1 — 2026-06-12

### Added

- YAML workflows: policy blocks (blocked_tools, require_approval_for, model_allowlist) at workflow and node scope now compile into runtime IR. Unknown keys inside a policy block raise ValueError.

## 0.9.0 — 2026-06-06

### Added — Multi-agent YAML fleets + scheduling

- One YAML file can declare a fleet: an `agents:` map (strategy agents) and a
  `workflows:` map (explicit node graphs) sharing a top-level `tools:`/`mcp:`
  catalog. Agents reference tools via a typed `uses:` list with `tool:`, `mcp:`,
  `agent:`, and `workflow:` prefixes, plus inline `tools:`.
- `jamjet deploy <file>` registers every unit in a fleet and installs its cron
  schedules. `jamjet run <file> [unit]` runs one unit on demand; single-unit
  files need no selector.
- Per-unit `schedule: { cron: "..." }` installs a cron job through the runtime's
  new `/cron` API; the bundled `jamjet-server` runs due jobs locally via a
  dev-embedded scheduler. UTC only in this release.
- `jamjet.workflow.bundle.compile_bundle` validates unknown refs, duplicate unit
  ids, cyclic sibling refs, and cron expressions. Singular `agent:` and graph
  `workflow:`/`nodes:` files continue to compile unchanged.
- `JamjetClient.create_cron_job` and `list_cron_jobs`.
- Example fleet at `examples/fleet/fleet.yaml`.

## 0.8.5 — 2026-05-14

### Added — Cloud Sync v0.1 Path B (direct-push)

- `jamjet.cloud.cloud_pusher.CloudPusher` — serverless-friendly audit event
  pusher that POSTs directly to `/v1/policy-audit/events`. Used by short-lived
  runtimes (Lambda, Cloud Run, Edge) where the on-disk daemon isn't an option.
- `jamjet.cloud.trace_context` — W3C `traceparent` parser + propagator. Lets
  the guardrail stamp `trace_id` on every emitted event so they group with the
  rest of the OTLP span tree in Cloud.
- `jamjet.cloud.sync_redaction` — `apply_args_redaction()` /
  `resolve_args_redaction()` helpers (R9 invariant): never push raw tool args
  to Cloud, only `full` / `hash` / `none` per the policy.
- `jamjet.integrations.openai_guardrail` — wired for Path B. When
  `JAMJET_CLOUD_AUDIT_URL` is set, the guardrail uses `CloudPusher` to direct-push
  redacted decisions in addition to local logging.

### Changed

- `jamjet-engram` requirement bumped (see #57). No public API impact.
- Python CI lint policy normalized for the guardrail module (no behaviour
  change).

## 0.8.4 — 2026-05-12

### Fixed
- `jamjet.cloud.policy.PolicyEvaluator.evaluate()` now correctly returns the
  **first** matching rule's decision (previously: last match). Aligns Python with
  the TS `@jamjet/cloud@0.3.0` evaluator and the shared conformance contract at
  `jamjet-policy/conformance/policy-decisions.yaml`. No production trigger
  observed today because policies in the wild use single rules; this prevents
  drift the moment multi-rule policies ship.

## 0.8.0 — 2026-05-08

### Added
- New `jamjet.spec` package: Pydantic IR models (`AgentSpec`, `DurableAgentSpec`, `WorkflowSpec`, `MemoryConfig`, `LLMConfig`, `ToolSpec`, `DurabilityConfig`, `MethodSpec`, `NodeSpec`, `EdgeSpec`, `AgentStrategy`)
- New `jamjet.runtime` package: `Runtime` Protocol + `LocalRuntime` (in-process executor with SQLite-backed durability, replay, crash recovery)
- New `jamjet.decorators` package: `@DurableAgent` (class decorator with bare/parameterized/`stateless=True` forms), `@workflow`, `@task`, `@tool` (re-export)
- `jamjet.memory.AgentMemory` — Engram v2 bridge for `self.memory` inside `@DurableAgent`. Supports `record` / `record_message` / `recall` / `context` / `synthesize` / `ask` (mode-aware)
- Top-level `run()`, `resume()`, `deploy()` entry points
- `RuntimeEvent` callback for streaming step lifecycle to consumers (no exporter yet — Phase 6)
- Stub runtimes for cloud / java / rust (raise `NotImplementedError` until Phase 5)
- 5 new examples in `examples/python/`

### Changed
- `Agent.compile()` returns `AgentSpec` instead of a dict (breaking for code that introspected the dict; call `.model_dump()` if you need a dict)
- `Workflow.compile()` returns `WorkflowSpec` instead of a dict
- `Agent.run()` now executes via `LocalRuntime` under the hood. Strategy executors moved to `jamjet.runtime.local.strategies.*`. Public behavior unchanged.

### Notes
- This release combines roadmap Phase 1 (DSL + IR) and Phase 2 (Local runtime + durability)
- LLM providers other than OpenAI, MCP wiring, multi-agent coordination, OTel exporters, dispatch backends — all land in Phases 3-7
- Method-level checkpointing inside `@task` methods is partial — only the entrypoint method is checkpointed. Full intra-method interception lands in a Phase 1+2 follow-up.

## 0.7.0 — 2026-04-29

### Added
- `jamjet.durable` — `@durable` decorator for exactly-once tool execution.
  Backed by a SQLite-based idempotency cache. Sync and async functions both
  supported.
- `jamjet.durable.durable_run(execution_id)` — context manager that scopes
  cached results to a single agent run.
- Five framework shims that bridge native run identity to JamJet's execution
  context:
    - `jamjet.langchain` — for `langchain.agents.AgentExecutor`
    - `jamjet.crewai` — for `crewai.Crew`
    - `jamjet.adk` — for Google Agent Development Kit
    - `jamjet.anthropic_agent` — for Claude Agent SDK
    - `jamjet.openai_agents` — for OpenAI Agents SDK
- Optional dependency extras: `[langchain]`, `[crewai]`, `[adk]`,
  `[anthropic-agent]`, `[openai-agents]`.

### Notes
- Implements "Keep your framework. Add JamJet." for Python users by providing
  a one-decorator integration path that doesn't require rewriting the agent.
