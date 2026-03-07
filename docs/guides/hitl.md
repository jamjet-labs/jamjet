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
3. A human approves, rejects, or edits the state via API or UI
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

```bash
# Approve
curl -X POST http://localhost:7700/executions/exec_01abc/approve \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"decision": "approved", "comment": "Looks good"}'

# Reject
curl -X POST http://localhost:7700/executions/exec_01abc/approve \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"decision": "rejected", "comment": "Report needs more citations"}'

# Approve with state edit (e.g., fix the report before publishing)
curl -X POST http://localhost:7700/executions/exec_01abc/approve \
  -H "Authorization: Bearer $TOKEN" \
  -d '{
    "decision": "approved",
    "state_patch": {"report": {"title": "Corrected Title"}},
    "comment": "Fixed title"
  }'
```

---

## Audit Log

Every human decision is recorded:
- Who made it (identity from auth token)
- What decision was made
- What state patch was applied (if any)
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
