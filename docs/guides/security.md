# Security Guide

---

## Authentication

### API Tokens (v1)
```bash
# Generate a token
jamjet auth token create --name my-service --scopes read,write

# Use in requests
curl -H "Authorization: Bearer $TOKEN" http://localhost:7700/executions
```

Set token in Python SDK:
```python
from jamjet import JamjetClient
client = JamjetClient(api_token=os.environ["JAMJET_TOKEN"])
```

---

## RBAC

| Role | Permissions |
|------|-------------|
| `operator` | Full access, including tenant policy and hand-enqueued work items |
| `developer` | Create/run/inspect workflows; manage agents and tools; act as a worker |
| `reviewer` | Read executions, approve HITL nodes |
| `viewer` | Read-only access to executions and traces |

Roles are ordered: each role can do everything the roles below it can. Outside dev
mode every route below needs a Bearer token, and the role is enforced in code on the
route, not by convention. Dev mode (`JAMJET_DEV_MODE=1`) has no tokens and enforces
none of this.

| Route | Minimum role | Notes |
|-------|--------------|-------|
| `GET /health`, `GET /.well-known/did.json` | none | public |
| any other `GET` | `viewer` | except the tenant routes below |
| `POST /workflows` | `developer` | a version is immutable once registered |
| `POST /cron`, `DELETE /cron/:name` | `developer` | jobs belong to the token's tenant (`GET /cron` lists only those, and a name is unique per tenant); the job records its creator, who becomes the initiator of every run it starts, in the job's tenant |
| `POST /executions`, `POST /executions/:id/cancel`, `POST /executions/:id/external-event` | `developer` | the starting token is the run's initiator |
| `POST /executions/:id/approve` | `reviewer` | never the run's initiator |
| `POST /artifacts` | `developer` | |
| `POST /agents`, `/agents/discover`, `/agents/:id/activate`, `/deactivate`, `/heartbeat` | `developer` | |
| `POST /work-items` | `operator` | the engine is the only normal producer of work items |
| `POST /work-items/claim`, `/:id/complete`, `/:id/fail`, `/:id/heartbeat` | `developer` | worker protocol; claim and complete enforce policy and approvals |
| `POST /tenants` | `operator` | |
| `GET /tenants` | `operator` | lists only the token's own tenant |
| `GET /tenants/:id`, `PUT /tenants/:id` | `operator` | only the token's own tenant (403 otherwise) |
| `/mcp` | per tool | `viewer` for reads, `developer` to run, cancel or discover, `reviewer` to approve |

**The runtime is single-tenant today.** The API layer scopes requests by the
token's tenant, but the engine under it runs every workflow in tenant `default`.
So a token of any other tenant is refused with 403, on REST and MCP.
`JAMJET_TENANT_TOKENS_AS_DEFAULT=1` (read once at startup) lets such tokens act in
`default` instead; that gives no isolation. To run tenants side by side, these would
have to carry the tenant first, and none does yet:

- the scheduler's work items (every item after the start node is enqueued in `default`);
- workflow lookup and its caches in the scheduler and the in-process worker (keyed by
  workflow id and version only, so two tenants' versions collide);
- execution and work-item creation on the base backend, and segment rollover;
- tenant policy for work items after the start node (they are in `default`, so default's
  tenant policy applies to them);
- the agent registry (global today).

The tenant policy is the one policy layer a developer cannot rewrite: workflow and node
policies live in IR that a developer registers, and a developer can always register a
new version. Put approval gates that must bind the people who start runs at tenant
level, and make them catch-alls (`require_approval_for: ["*"]`): a pattern matches a
name the IR author chose, and the same function can be reached under another name
(a re-export or alias), so a narrow tenant pattern does not bind an author who writes
their own IR.

---

## Secrets

**Never** put secrets in workflow YAML or Python code. Use environment variables:

```yaml
# agents.yaml
agents:
  researcher:
    mcp:
      servers:
        github:
          auth:
            type: bearer
            token_env: GITHUB_TOKEN   # reads from environment
```

For production, use a secrets backend (Vault, AWS Secrets Manager) — pluggable in v2.

Secrets are **redacted from logs and traces** automatically when referenced via `*_env` config keys.

---

## Inter-Agent Security

### MCP
- Bearer token or API key per MCP server connection
- Configured per-server in `agents.yaml`

### A2A
- Bearer token (v1)
- OAuth2 client credentials (v1)
- mTLS for cross-org federation (Phase 4)

All inter-agent communications are **audit logged**.

---

## Tool Permissions

```yaml
tools:
  get_ticket:
    permissions: [read_only]      # safe — read-only data access

  delete_record:
    permissions: [write, privileged]  # requires privileged worker
```

Agents with `allowed_tools` constraints in their autonomy config cannot invoke tools outside that list — enforced at the runtime level, not just in prompts.

---

## Audit Trail

All of the following are permanently logged with identity, timestamp, and decision:
- Human approval decisions, and the approval requests they answer (with the
  action digest and deadline each request bound), their expiries and their
  escalations. These are sealed into the signed, hash-chained audit log, so
  editing what was asked breaks verification as editing the decision does.
- Privileged tool invocations
- Agent-to-agent delegations
- Policy violations
- Authentication events
