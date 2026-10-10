# Human-in-the-Loop (HITL)

JamJet treats human approval as a first-class workflow primitive — not an afterthought.

---

## Approval Nodes

```yaml
nodes:
  review_report:
    type: human_approval
    description: "Please review the generated report before publishing"
    timeout: 72h          # optional: auto-route to fallback after timeout
    fallback: auto_publish  # node to go to if timeout expires (optional)
    next: publish
```

When this node is reached:
1. Workflow **pauses indefinitely** (or until timeout)
2. The current workflow state is available via API for inspection
3. A human approves or rejects the action via API or CLI
4. The decision is **audit logged** with the human's identity and timestamp
5. Workflow resumes

---

## Inspecting Paused Workflows

```bash
# List all paused workflows waiting for approval
jamjet executions list --status paused

# Inspect the state of a paused workflow
jamjet inspect exec_01abc
```

---

## Approving via API

An approval is bound to one exact action: the run, which occurrence of the
node, the node's definition as registered, and the payload the node is
dispatched with. The payload is what the executor or external worker receives;
for a `python_fn`, `java_fn` or `external_fn` node its `input` is the state the
function is called with. List the pending approvals first. Each one carries an
`action_digest` in its `context`, next to an `action_basis` that shows what the
digest covers (the payload, the occurrence and the node). Approve by echoing
the digest of the request you reviewed:

```bash
# What is waiting, and what exactly it would do
curl http://localhost:7700/executions/exec_01abc/approvals \
  -H "Authorization: Bearer $TOKEN"

# Approve the request you reviewed
curl -X POST http://localhost:7700/executions/exec_01abc/approve \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"decision": "approved", "node_id": "review_report",
       "action_digest": "<action_digest from the listing>", "comment": "Looks good"}'

# Reject (no digest needed)
curl -X POST http://localhost:7700/executions/exec_01abc/approve \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"decision": "rejected", "comment": "Report needs more citations"}'
```

From the CLI: `jamjet approvals exec_01abc` prints, for each pending request,
what its digest covers and the exact `jamjet approve ... --action-digest <digest>`
command; add `--show-basis` to print the full payload the digest binds. Then
`jamjet approve exec_01abc --decision approved --action-digest <digest>`. From
Python: `await client.approve(execution_id, "approved", action_digest=digest)`.

Rules the runtime enforces:

- **The digest is required to approve** when auth is on (outside
  `JAMJET_DEV_MODE`). A missing digest is a 400. A digest that no longer matches
  the pending request is a 409: the request was replaced (the state changed, or
  this is a later visit of the node), so re-read the listing and decide again.
  An agent tool dispatch request carries an `action_digest` too: it binds the
  run, the occurrence, the node's definition and the tool calls (names and
  arguments). A dispatch request recorded before this release carries only a
  `calls_hash`; echo that, and the node asks again for a bound approval.
- **If the node's input changes after you approve**, the node does not run on
  your approval. The engine builds each dispatch from the run's state at that
  moment; if the payload is not the one you approved, it asks again with a
  fresh request. A state change that does not reach the payload is not a change
  to the action: a `tool` node, for example, is dispatched without the run's
  state. Engine bookkeeping (`__budget`) does not count as a change.
- **Approval edits are not supported.** A decision carrying a `state_patch` is
  refused with 400. To change what runs, reject and re-run with different input.
- **Separation of duties.** The token that started a run cannot approve it
  (403). Rejecting your own run is allowed. A run with no recorded initiator
  (started in dev mode, or before initiators were recorded) cannot be approved
  outside dev mode, since nobody can show the approver is someone else.
  `JAMJET_ALLOW_SELF_APPROVAL=1` turns both checks off for single-operator
  installs.
- **A cron job's creator is the initiator of its runs.** `POST /cron` records the
  calling token as the job's `created_by`, and every run the job starts records
  that token as its initiator, so whoever scheduled a run cannot approve it.
  Replacing a job by name replaces its creator. Jobs created before this was
  recorded, or in dev mode, start runs with no initiator, which cannot be
  approved outside dev mode; recreate them with `POST /cron` to fix that.
- **SoD distinguishes tokens, not people.** Tokens are minted by an operator, so
  one person holding two tokens can approve their own run. Issue one token per
  human.
- **The approver is the token.** The recorded approver is the API token's name
  and `token:<id>`; a `user_id` in the body is ignored when auth is on.
- **Workers cannot skip the gate.** A node that needs approval is withheld from
  `POST /work-items/claim` until an approval covers it, and
  `POST /work-items/:id/complete` refuses a result for it otherwise. Outside
  dev mode the result of a gated node must echo the `lease_fence` from its
  claim; the legacy unfenced completion is refused for it (409).
  `POST /work-items` (hand-enqueueing an item) is operator-only.
- **A workflow version is immutable.** Re-registering the same IR is a no-op;
  a different IR under the same version is a 409. Register a new version.

What a policy can gate:

`require_approval_for` and `blocked_tools` are glob patterns matched against a
node's names. An approval binds the node's dispatched payload (plus the run,
the occurrence and the node definition), so what it covers depends on what the
payload carries.

| Node kind | Names a pattern matches | What the payload (and so an approval) carries |
|-----------|-------------------------|-----------------------------------------------|
| `python_fn` | `module:function` (also in dotted form `module.function`), and the node id | the run's state, as `input` |
| `java_fn` | `class/method` (also `class.method`), and the node id | the run's state, as `input` |
| `external_fn` | `queue/handler`, and the node id | the run's state, as `input` |
| `tool` | its `tool_ref` | no state |
| `mcp_tool` | its `tool` | no state |
| `a2a_task` | its `skill` | no state |
| `agent` | its `agent_ref` | no state |
| `model` | none: governed by `model_allowlist` only | the conversation (`messages`) and the resolved model settings |
| `condition` | none | the run's state, as `state` |
| `agent_tool`, `agent_discovery`, `coordinator`, `eval`, `human_approval`, `wait`, `parallel`, `join`, `subgraph`, `memory_retrieval`, `policy`, `finalizer`, `limit_exceeded` | none | no state |

Kinds with no name cannot be gated or blocked by a pattern. `agent_tool` and
`agent_discovery` make network calls in-process, so keep that in mind when
relying on patterns alone.

- A pattern applies if it matches any of a node's names, so `["*"]` gates every
  named kind. Python and Java handlers also match in canonical dotted form:
  `payments.transfer:send` and `payments:transfer.send` both read
  `payments.transfer.send`, and a pattern in either spelling matches both.
- **An agent tool dispatch node** (`jamjet.agents.tool_runtime:dispatch_tool_calls`
  with `agent_tool_dispatch: true`) is checked on its own names first: a gate
  there holds the node with an approval of its full payload, which covers the
  calls in it, even when there are no calls. Then each call is matched on the
  tool the model chose. The run's `tools` map can choose only among `@tool`s
  named like the call: the dispatcher accepts an entry only when it names a
  `@tool` registered under the called name and is that tool's own
  `module:qualname`, and refuses the call otherwise. Such an entry may select a
  same-named `@tool` from another importable module rather than the one the
  worker registered first. That is not code the engine did not see: the engine
  matches the call on the handler the entry names and binds that handler into a
  per-call approval (`evaluate_dispatch` in `runtime/policy/src/dispatch.rs`,
  and the bound calls in `runtime/workers/src/dispatch_guard.rs`), so policy and
  the approval both see the code that runs. To check an entry, the dispatcher
  may import the module it names, which runs that module's top-level code (any
  module importable on the worker) before the call is refused; a tool name that
  import overwrites is put back, and names it adds are removed if the entry is
  refused. The engine also matches each call on the handler its entry names
  (as written, in dotted form, and by function name), binds the handler into a
  per-call approval (the listing shows it), and blocks the batch on an entry
  that is not a `module:function` string or whose path has a `_`-prefixed
  function segment or a `__`-prefixed module segment. A blocked call is blocked
  even after the node was approved. The `agent_tool_dispatch` flag is accepted only on the ADK
  dispatcher; registering it on any other node is refused.
- **A pattern binds a name, not code.** The node id and the handler reference
  are chosen by whoever writes the IR, and a function is reachable under other
  names: a module that re-exports it (`myapp.api:send`) runs the same code
  under a path no narrow pattern mentions. So a narrow pattern such as
  `payments.*` stops honest mistakes, not an author who writes their own IR.
  Only a catch-all binds that author. Where it matters, set the tenant policy
  to `require_approval_for: ["*"]` and keep precision elsewhere (in which tools
  your workers expose), rather than relying on a narrow pattern.
- Workflow and node policies live in IR a developer registers, and a developer
  can register a new version without the gate. A gate that must bind the
  developers who start runs belongs in the tenant policy, which only an operator
  can change (see the route table in the security guide).

What an approval proves, and what it does not:

- It proves a reviewer other than the initiator agreed to the action shown in
  `action_basis`, before the action ran.
- It does not vouch for the result. An external worker reports the result of
  an approved node (`output` and `state_patch`) with its own token, and worker
  tokens are developer tokens, the same role that starts runs. The integrity of
  the result rests on who holds worker credentials. Give workers their own
  tokens, distinct from the tokens of the people who start runs.

Limits worth knowing:

- An approval covers the action including its retries. A retry, or a lease that
  expires and is reclaimed, fires again under the same approval; the tool-effect
  idempotency key limits double effects, and firing is at-least-once in v1.
- The decision records the sequence of the request it answers, but status is
  resolved from the latest request. A decision that races a re-request fails
  safe: its digest does not match, and the node asks again.
- An external worker that reads the run's state through the API, instead of
  using its payload, acts on something the approval does not cover.
- The start item of a new segment (after a segment rollover) is enqueued with a
  minimal payload, not the one the scheduler builds. A gated start node of a
  later segment therefore asks for approval twice: once for that item, and
  again for the scheduler's dispatch after the first approval.

---

## Approval Deadlines

A policy can give the approval requests it raises a deadline:

```json
{
  "require_approval_for": ["payments.*"],
  "approval_timeout_secs": 3600,
  "escalate_after_secs": 900
}
```

From the SDKs, the same keys go in a YAML `policy:` block (workflow or node
level) or in an agent's policy dict; the Python compiler checks them as the
runtime does and emits them only when set:

```yaml
nodes:
  send_wire:
    type: tool
    tool_ref: payments.send_wire
    policy:
      require_approval_for: ["*"]
      approval_timeout_secs: 3600
      escalate_after_secs: 900
```

```python
agent = Agent(
    "payments",
    model="claude-sonnet-4-5",
    tools=[send_wire],
    policy={"require_approval_for": ["send_*"], "approval_timeout_secs": 3600},
)
```

The tenant level is set with `PUT /tenants/:id` (operators only). The Java
builder has no policy support yet; use IR or the tenant policy.

- `approval_timeout_secs` (1 second to 1 year): how long a request stays
  open. When it passes, the scheduler records `approval_expired` and treats it
  exactly as a rejection by `system:deadline`: the node fails the way a
  rejected node does, and so does the run. An expiry never approves anything.
- `escalate_after_secs` (optional, at least 1 and less than the timeout, and
  only next to a timeout in the same policy): if the request is still pending
  then, the scheduler records `approval_escalated` once. It changes nothing
  about the request; it is the hook point for an external notifier (Slack,
  email) to watch for.
- Both can be set in the tenant, workflow or node policy, and every level is
  a ceiling: the shortest `approval_timeout_secs` any level sets applies, and
  so does the shortest `escalate_after_secs`. A workflow or node policy can
  only shorten the tenant's deadline or bring its escalation forward; a
  one-year node policy neither lengthens a tenant's one-hour deadline nor
  drops its escalation. If the shortest escalation would not come before the
  shortest timeout, the escalation is dropped (and logged), never the timeout
  extended.
- Invalid values are refused at registration (400) and at `PUT /tenants`
  (400). `timeouts.approval_timeout` is retired: it was never read, so a run
  under it never expired. A non-null value is refused when a new version is
  registered; a version stored with it before still re-registers unchanged,
  with no deadline.

A request raised under a deadline records it in its context as UTC times,
`expires_at` and (when set) `escalate_at`; the pending listing shows both on
each entry. A request raised with no deadline behaves as before: it waits
until someone decides it.

Rules the runtime enforces:

- **A decision after the deadline is refused (409) and records nothing**,
  whether or not the sweep has recorded the expiry yet. The deadline is
  reached at its instant: a decision at exactly `expires_at` is late. It is
  checked again in the same step as the append, under the store's write lock,
  so a decision checked in time whose write is delayed past the deadline (a
  lock wait, a pause) is refused too. The deadline is checked before the digest
  rules, so a late approval with no digest or a stale one is also 409
  "expired" rather than 400 or a stale-digest 409, and a decision naming no
  node on a run whose latest request expired says it expired.
- **One outcome per request.** A decision and an expiry each append only while
  the request is still the node's pending one, checked in the same step as the
  append. If the sweep expires a request while a decision is in flight, the
  decision gets 409; if the decision lands first, the sweep records nothing.
  A sweep that runs again, or on another scheduler, records nothing twice.
- **An unreadable deadline counts as passed.** Only the engine writes
  `expires_at`; a value that is not a time is treated as expired, never as
  open forever.

Limits worth knowing:

- The sweep runs on the scheduler tick (every 500 ms by default) for running
  executions that hold a node on a request carrying a deadline (a run with
  no deadlines is never read for it), so an expiry is recorded within a
  tick of the deadline and the node fails on the next. A run that is not
  `running` (paused) is not swept until it runs again; a decision on it is
  still refused after the deadline.
- Deadlines are computed from the clock of the process that raised the
  request and checked against the clock of the one that sweeps or decides.
  Keep their clocks in sync.
- A hold raised by an agent card's `require_approval_for` takes the
  deadline of the policy chain (tenant, workflow, node), like any other
  request; a card has no deadline settings of its own. The autonomy path's
  approval hold (guided mode) is not reachable today, so it raises none.

---

## Event Kinds a Build Cannot Read

A run's log can hold an event a newer release wrote. This build reads it as
`unknown`, so the log still loads and the event still lists, but it does not
guess what it meant: it could be a revocation or a denial. Every approval in
that run is treated as not covering anything, no request is raised, a
decision is refused (409, "event log contains event kinds this build cannot
read"), and the scheduler stops advancing the run and records `run_stalled`
once. Upgrade the engine to continue the run.

---

## Audit Log

Every human decision is recorded:
- Who made it (the API token's name and `token:<id>`)
- What decision was made, and which request it answered
- The `approved_digest` it authorised
- Timestamp

The request is recorded the same way. Every `tool_approval_required`, whichever
component raised it (the HTTP claim route, the in-process worker, the agent
tool dispatch guard), is sealed into the signed, hash-chained audit log next to
the decision, with its context: the `action_digest`, what it covers and its
deadline. So are `approval_expired` (actor `system:deadline`) and
`approval_escalated`. Editing a sealed request, for example to point it at a
different action, fails chain verification at that entry, exactly as editing a
decision does.

Sealing follows the durable write of the event and never undoes it. If the
audit write fails, the run's own event log gets an `audit_seal_failed` event
naming the unsealed event (its id, sequence and type) and why, and the
server's enricher counts the failure.

What chain verification can and cannot see: `verify_chain` checks the
entries it is given, so it detects an edited entry, a re-ordered or inserted
one, a broken link and a forged or stripped signature. It cannot see an
entry that was never written (a failed seal), or entries removed from the
end of the log. To find gaps, compare the run's event log with its audit
entries; `audit_seal_failed` marks every gap the engine knows about. Every record is sealed
under the run's own tenant (the scheduler reads it from the run's execution
row), so `GET /audit` shows a tenant its own requests, expiries and
escalations and no other tenant's.

```bash
jamjet events exec_01abc --type approval_received
```

---

## Timeouts and Fallbacks

```yaml
human_review:
  type: human_approval
  timeout: 48h
  fallback: auto_approve   # route here if timeout expires
```

If no `fallback` is specified and the timeout expires, the workflow fails with a `timeout_exceeded` event.

---

## A2A Multi-Turn (input-required)

When JamJet delegates to an external A2A agent and receives `input-required`, the workflow pauses and routes to the configured `on_input_required` node — typically a human approval node. This enables multi-turn interactions with external agents via HITL.
