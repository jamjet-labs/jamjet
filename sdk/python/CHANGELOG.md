# Changelog

## 0.11.0 — 2026-06-29

The full ADK feature set. A plain `Agent` is now durable and governed by default, with first-class sessions, memory, multi-agent teams, deploy, and a five-minute dev loop.

### Added

- Governance on by default. `Agent(policy=, approval_required=, budget=, pii=, audit=, receipts=)`: a model allowlist, fail-closed budget caps, PII redaction at the model seam, a signed hash-chained audit record per action, and an AgentBoundary receipt per turn. Enforced on both `run()` and `run_durable()`.
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
