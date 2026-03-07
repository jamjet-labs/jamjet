# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.x (pre-release) | Yes — latest only |

Once JamJet reaches 1.0, we will maintain a formal supported version table.

## Reporting a Vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

To report a security issue, email **security@jamjet.dev** with:

- A description of the vulnerability
- Steps to reproduce
- Potential impact
- Any suggested mitigations (optional)

You will receive an acknowledgment within **48 hours** and a substantive response within **7 days**.

We will coordinate disclosure with you and give credit to reporters (unless you prefer to remain anonymous).

## Security Model

JamJet's security model is documented in [docs/guides/security.md](docs/guides/security.md). Key areas:

- **Authentication:** API tokens (v1); OAuth/OIDC (v2)
- **Authorization:** RBAC with operator, developer, reviewer, viewer roles
- **Inter-agent auth:** Bearer tokens, OAuth2 client credentials, mTLS (cross-org)
- **Secrets:** Environment variables (local); pluggable secrets backend (production)
- **Tool sandboxing:** Permission classes and allowlists (v1); isolated sandbox workers (v2)
- **Audit trail:** All human actions, privileged tool use, and inter-agent delegations are logged

## Scope

In scope for responsible disclosure:
- Remote code execution via workflow definitions or tool invocations
- Authentication bypass in the API or inter-agent protocols
- Secret exposure via logs, traces, or API responses
- Privilege escalation between tenants or agents
- MCP/A2A protocol-level vulnerabilities in the JamJet implementation

Out of scope:
- Vulnerabilities in third-party MCP servers or A2A agents that JamJet connects to
- Denial-of-service without demonstrated exploitability
- Issues requiring physical access to the server
