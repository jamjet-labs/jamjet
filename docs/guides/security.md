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
| `operator` | Full access — manage runtime, workers, policies |
| `developer` | Create/run/inspect workflows; manage agents and tools |
| `reviewer` | Read executions, approve HITL nodes |
| `viewer` | Read-only access to executions and traces |

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
- Human approval decisions
- Privileged tool invocations
- Agent-to-agent delegations
- Policy violations
- Authentication events
