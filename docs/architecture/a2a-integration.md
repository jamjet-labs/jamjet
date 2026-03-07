# A2A Integration Architecture

JamJet provides native A2A (Agent-to-Agent) protocol support — agents can discover, communicate with, and delegate to other agents across any framework or organization.

---

## A2A as A2A Server

Any JamJet agent is publishable as an A2A-compliant server:

- Agent Card at `/.well-known/agent.json`
- Accepts tasks via `tasks/send` and `tasks/sendSubscribe`
- Full task lifecycle: `submitted → working → input-required → completed / failed / canceled`
- SSE streaming for long-running task progress
- Push notifications to registered webhooks

### Task lifecycle mapping

| A2A State | JamJet Event |
|-----------|-------------|
| `submitted` | `node_scheduled` |
| `working` | `node_started` |
| `input-required` | `interrupt_raised` |
| `completed` | `node_completed` |
| `failed` | `node_failed` |
| `canceled` | `node_cancelled` |

---

## JamJet as A2A Client

JamJet agents delegate to external A2A agents:

1. Fetch Agent Card from `/.well-known/agent.json`
2. Negotiate capabilities at connection time
3. Submit task via `tasks/send`
4. Poll or stream (`tasks/sendSubscribe` SSE) for progress
5. Handle `input-required` — pauses the JamJet workflow, routes to HITL or supervisor
6. On `completed` — map task artifacts into workflow state

### Durability

A2A task state is durably tracked in the JamJet event log. A crash mid-delegation:
- Runtime restores state from event log
- Resumes polling/tracking the remote A2A task using the stored `task_id`
- No duplicate submissions

---

## Protocol Adapter

The A2A client/server is implemented as a `ProtocolAdapter` in `jamjet-a2a`:

```
ProtocolAdapter trait:
  discover(url)    → AgentCard
  invoke(task)     → TaskHandle
  stream(task)     → impl Stream<Item = TaskEvent>
  status(task_id)  → TaskStatus
  cancel(task_id)  → ()
```

---

## Security

- Bearer token auth (v1)
- OAuth2 client credentials (v1)
- mTLS for cross-org federation (v2/Phase 4)
- Capability-scoped authorization — agents only invoke authorized skills
- All delegations are audit-logged
