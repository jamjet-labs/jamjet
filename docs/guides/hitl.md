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
node, the node's definition as registered, and the state the node will run on.
List the pending approvals first. Each one carries an `action_digest` in its
`context`, next to an `action_basis` that shows what the digest covers (the
state, the occurrence and the node). Approve by echoing the digest of the
request you reviewed:

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

From the CLI: `jamjet approvals exec_01abc`, then
`jamjet approve exec_01abc --decision approved --action-digest <digest>`. From
Python: `await client.approve(execution_id, "approved", action_digest=digest)`.

Rules the runtime enforces:

- **The digest is required to approve** when auth is on (outside
  `JAMJET_DEV_MODE`). A missing digest is a 400. A digest that no longer matches
  the pending request is a 409: the request was replaced (the state changed, or
  this is a later visit of the node), so re-read the listing and decide again.
  For an agent tool dispatch, echo the request's `calls_hash` instead.
- **If the state changes after you approve**, the node does not run on your
  approval. It asks again with a fresh request for the new state. Engine
  bookkeeping (`__budget`) does not count as a change.
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
  `POST /work-items/:id/complete` refuses a result for it otherwise.
  `POST /work-items` (hand-enqueueing an item) is operator-only.
- **A workflow version is immutable.** Re-registering the same IR is a no-op;
  a different IR under the same version is a 409. Register a new version.

Limits worth knowing:

- An approval covers the action including its retries. A retry, or a lease that
  expires and is reclaimed, fires again under the same approval; the tool-effect
  idempotency key limits double effects, and firing is at-least-once in v1.
- The decision records the sequence of the request it answers, but status is
  resolved from the latest request. A decision that races a re-request fails
  safe: its digest does not match, and the node asks again.
- The digest is computed over the run's state as seen in the `default` tenant.
  Every token is minted in `default` today; per-tenant state views are not
  threaded through yet.

---

## Audit Log

Every human decision is recorded:
- Who made it (the API token's name and `token:<id>`)
- What decision was made, and which request it answered
- The `approved_digest` it authorised
- Timestamp

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
