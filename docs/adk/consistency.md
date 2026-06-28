# Consistency contract

What the JamJet durable engine actually guarantees, and what it does not. Every
claim below is grounded in the engine source so you can verify it yourself. This
is a description of observed engine invariants, not a distributed-systems wish
list. Paths are relative to the repository root.

The short version: **a single run is strongly consistent, views across runs are
eventually consistent, and stored artifacts are immutable.** There is no managed
multi-tenant execution service and no cross-region replication.

## Within a run: strongly consistent

A single execution's event log is the source of truth. State transitions for one
run are linear and durable, because the engine commits each turn in one
transaction.

- **The settle, the event, and the snapshot commit atomically.**
  `SqliteBackend::commit_turn` opens a single `BEGIN IMMEDIATE` transaction
  (`runtime/state/src/sqlite.rs:1219`), takes the write lock up front, performs a
  fenced settle that fails closed on a stale fence and emits nothing
  (`runtime/state/src/sqlite.rs:1223`), assigns the next sequence and appends the
  event in the same transaction (`runtime/state/src/sqlite.rs:1254-1265`), records
  tool effects idempotently (`runtime/state/src/sqlite.rs:1306`), and writes the
  state snapshot in the same transaction (`runtime/state/src/sqlite.rs:1426`)
  before a single `tx.commit()` (`runtime/state/src/sqlite.rs:1444`). A crash
  between steps cannot half-apply a turn: either the whole turn commits or none of
  it does.

- **Writers are serialized.** `BEGIN IMMEDIATE` takes the write lock at the start
  of the transaction rather than upgrading mid-way, so a concurrent writer gets an
  immediate, handled busy signal instead of a silent lost update
  (`runtime/state/src/sqlite.rs:515-522`). SQLite is single-writer; the engine
  leans on that rather than fighting it.

- **Replay is deterministic.** The state for a run is a fold over its event log.
  `apply_events_seeded` is the core replay primitive: it iterates events in
  sequence order and applies each one to evolve state
  (`runtime/state/src/materializer.rs:90-159`), and `apply_events` is the
  from-origin form built on it (`runtime/state/src/materializer.rs:160-173`).
  Replaying the same log reconstructs the same state, which is what makes
  time-travel debugging and crash recovery correct.

The practical contract: once `commit_turn` returns for a turn, that turn is
durable and visible to every later read of that execution, and a restart replays
the log to exactly the committed state.

## Across runs: eventually consistent

Read and aggregate views that span more than one execution are built by an
asynchronous projector that tails the event log. They converge; they are not
instantaneously consistent with the latest write.

- **The projector is a background loop.** `Projector::run` loops forever, calling
  `tick()` and then sleeping 500ms between passes
  (`runtime/scheduler/src/projector.rs:250-256`). It is spawned as a detached
  background task at startup
  (`runtime/api/src/main.rs:166-167`: `tokio::spawn(async move { projector.run().await })`).

- **It folds events into durable read rows behind a checkpoint.** Each pass reads
  the event delta since a per-execution checkpoint, folds the relevant events
  (for example approval events), and advances the checkpoint only if every changed
  row in the batch committed (`runtime/scheduler/src/projector.rs:113`
  `project_execution`, reading the delta with `get_events_since` at
  `runtime/scheduler/src/projector.rs:119` and committing the batch plus the
  checkpoint atomically via `apply_approval_projection_batch` at
  `runtime/scheduler/src/projector.rs:227`). The checkpoint makes the projection
  catch-up safe and idempotent, but it runs after the fact.

The practical contract: a write to one run is immediately consistent for that run,
but a cross-run projection (for example an approvals view) reflects it on the next
projector pass, within a bounded lag rather than instantly. Build cross-run UIs
and queries to tolerate that lag.

## Artifacts: content-addressed and immutable

Large values are stored in a content-addressed store keyed by the SHA-256 of the
bytes.

- **The key is the hash of the content.** `put_artifact` computes
  `sha256_hex(bytes)` and stores the blob under that hash
  (`runtime/state/src/sqlite.rs:868-884`); the hash function is a plain SHA-256
  hex of the raw bytes (`runtime/state/src/hashing.rs:65-72`). Identical bytes
  always produce the same reference.

- **Stored artifacts are immutable.** The insert is `INSERT OR IGNORE`, which
  keeps the first row for a given `(tenant, hash)`
  (`runtime/state/src/sqlite.rs:872-889`). A second write of the same bytes does
  not overwrite or mutate what is stored.

The practical contract: an `ArtifactRef` is a stable, verifiable handle. The same
content hashes to the same ref, and a ref never changes underneath you.

## Storage backends

The engine has exactly two storage backends, selected by the `STORAGE_BACKEND`
environment variable: an in-memory backend and a SQLite backend
(`runtime/api/src/main.rs:48-155`). There is no Postgres backend and no other
durable store in the engine. The in-memory backend is ephemeral and per-process;
SQLite is the durable, single-writer store used for self-host and hosted
deployments.

Cron scheduling requires the SQLite backend. The cron handlers return
"cron scheduling requires the sqlite backend" when no cron store is configured,
which is the case for the in-memory backend (`runtime/api/src/cron.rs:25-30`).

## What is not guaranteed

State this plainly so nobody designs against a guarantee that does not exist.

- **No managed multi-tenant execution cell.** `runtime="cloud"` deploys the same
  IR to *your* hosted `jamjet-server` engine (a URL you configure) and layers on
  JamJet Cloud governance. JamJet Cloud (`api.jamjet.dev`) is a governance and
  observability span API, not a workflow execution engine. There is no managed
  "cell" that runs your workflows for you; the string "cell" does not appear in
  the engine source. The deploy surface that honors this model is
  `sdk/python/jamjet/deploy/__init__.py`.

- **No cross-region or replicated consistency.** The engine is a single
  SQLite-or-memory store per process. There is no multi-region replication, no
  sharding, and no cross-region coordination in the engine (a repository-wide
  search for region / shard / replica / replication implementation finds none).
  Run-level guarantees hold within one engine instance, not across a fleet of
  instances.

- **Cross-run views are not instantaneous.** As above, the projector is
  asynchronous. Do not assume an aggregate or cross-execution query reflects a
  write the moment it commits.

## Claim-to-code map

| Claim | Backing code |
| --- | --- |
| Atomic settle + event + snapshot per turn | `runtime/state/src/sqlite.rs:1219-1444` (`commit_turn`) |
| Serialized single writer | `runtime/state/src/sqlite.rs:515-522` (`BEGIN IMMEDIATE`) |
| Deterministic replay (fold over the log) | `runtime/state/src/materializer.rs:90-173` (`apply_events_seeded`) |
| Async cross-run projector | `runtime/scheduler/src/projector.rs:250-256`; spawned `runtime/api/src/main.rs:166-167` |
| Content-addressed artifacts | `runtime/state/src/sqlite.rs:868-884`; `runtime/state/src/hashing.rs:65-72` |
| Immutable artifacts (`INSERT OR IGNORE`) | `runtime/state/src/sqlite.rs:872-889` |
| SQLite-or-memory only, no Postgres | `runtime/api/src/main.rs:48-155` |
| Cron requires SQLite | `runtime/api/src/cron.rs:25-30` |

## See also

- `docs/adr/ADR-003-event-sourcing-snapshots.md` for the event-sourcing and
  snapshot design decision behind the run-level guarantee.
- `sdk/python/jamjet/deploy/__init__.py` for the deploy surface (`local` /
  `self-host` / `cloud`), which honors the same honest model: three engine URLs,
  with Cloud governance optionally layered on the `cloud` leg.
