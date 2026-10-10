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

An agent's `allowed_tools` (in its Agent Card's `constraints`) is enforced by the runtime, not just in prompts, on every node that acts for the agent: a workflow or node whose IR sets `agent_ref` to the card's `uri`. Such a node runs only if its tool is on the list. For an ADK agent's tool calls, both the name the model called and the code the call resolves to must be on it, while the run's `tools` map has an entry for the call. A node that chooses its target at run time (a coordinator, agent discovery, an `auto` agent tool) is refused under a list. A refused node fails with a policy violation recorded under scope `agent`, on both the HTTP claim route and the in-process worker. An absent or empty list restricts nothing, and a node that acts for no agent is not checked against any card. The list is an additional restriction: tenant, workflow and node policy still apply. A node acting for an agent that is not registered is refused. The card's `blocked_tools`, `require_approval_for` and `allowed_delegations` apply to the same nodes, and under a non-empty list a node that picks its target at run time is refused (or, for `require_approval_for`, gated); [What each constraint does](agent-model-guide.md#what-each-constraint-does) lists the rest of the card and how little of it is enforced.

```json
{ "workflow_id": "research", "agent_ref": "jamjet://acme/researcher", "nodes": { ... } }
```

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

### Signing the audit log

Every audit entry is hash-chained to the one before it and signed. There are two signing modes.

| Mode | Set | Who can verify | Who can forge |
|------|-----|----------------|---------------|
| HMAC-SHA256 (default) | `JAMJET_AUDIT_SIGNING_KEY` | holders of the shared key | the same people |
| Ed25519 | `JAMJET_AUDIT_SIGNING_MODE=ed25519` and `JAMJET_AUDIT_ED25519_KEY_FILE` (or `JAMJET_AUDIT_ED25519_KEY`) | anyone with the public key, offline | only the runtime |

The Ed25519 key is a 32-byte seed written as 64 hex digits:

```bash
openssl rand -hex 32 > audit-ed25519.key
export JAMJET_AUDIT_SIGNING_MODE=ed25519
export JAMJET_AUDIT_ED25519_KEY_FILE=$PWD/audit-ed25519.key
```

A missing or malformed key leaves entries unsigned, with a warning; it never falls back to HMAC. Prefer the key file: a key in `JAMJET_AUDIT_ED25519_KEY` stays in the process environment, which child processes the runtime starts (an MCP stdio server, for example) inherit. The runtime warns if the key file is readable by other users. The key is not logged, and only the public key leaves the process: it is published in `/.well-known/did.json` as the verification method `#audit-key` (a JWK, `kty: OKP`, `crv: Ed25519`). Existing HMAC deployments need no change.

Each entry is signed on its own, not a chain head at intervals. The chain spans every tenant, so one tenant's slice of the log is not contiguous, and a per-entry signature is the only one a reader of that slice can check.

To verify `GET /audit` offline with the Python SDK (pin the public key you trust rather than fetching it from the runtime you are checking; the DID document only says which key the runtime claims):

```python
import httpx
from jamjet.runtime_audit import chain_order, load_entries, public_key_from_did_document, verify_entries

base = "http://localhost:7700"
headers = {}  # {"Authorization": "Bearer <token>"} outside dev mode
public_key = public_key_from_did_document(httpx.get(f"{base}/.well-known/did.json").json())
entries = load_entries(httpx.get(f"{base}/audit", params={"limit": 200}, headers=headers).text)
# One tenant (this engine's only mode): the export is one unbroken run of the chain.
# For one tenant's entries of a log shared by several, use linked=False instead.
verify_entries(chain_order(entries), public_key)  # raises ChainError on the first bad entry
print(f"{len(entries)} entries verified")
```

An entry verifies only under the key that signed it. The DID document publishes the current key, so after rotating the key (or switching from HMAC) keep the old public keys and verify older entries with them; HMAC-signed entries need the HMAC key. The verifier refuses a small-order or non-canonical public key, and a signature whose `R` is of small order, as the runtime does.

In Rust, `AuditSigner::ed25519_verifier(&public_key)` with `jamjet_audit::verify_chain` (or `verify_entry` for a non-contiguous slice) does the same.

What a signature covers: the JCS (RFC 8785) statement `{"alg":"Ed25519","entry_hash":"<hex>","kid":"<kid>","spec":"jamjet.audit.entry/v1"}`, where `kid` is the first 16 hex digits of SHA-256 over the raw public key. The stored signature is `ed25519:<kid>:<hex>`. How this lines up with AgentBoundary's verifier:

- Same: Ed25519, JCS for the signed statement, SHA-256 in lowercase hex, a domain-separating tag (`spec`), and strict verification (canonical `S`, small-order keys refused).
- Different: the `entry_hash` the statement names is the runtime's existing binary content hash (fields joined by `0x1f`), not SHA-256 over JCS of the entry, so that chains written before this release still verify. There is no COSE_Sign1 envelope, the `kid` is not a COSE header, and the signature is hex in a prefixed string rather than a base64 `sig.value`. As with AgentBoundary today, nothing detects a truncated tail: there is no signed checkpoint or transparency log.
