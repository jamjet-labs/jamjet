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

- **Authentication:** API tokens, issued with `jamjet-server token create`. There is no OAuth, OIDC or mTLS support.
- **Authorization:** RBAC with operator, developer, reviewer, viewer roles
- **Secrets:** read from the server's environment (and a key file for the Ed25519 audit key). There is no secrets backend.
- **Tool limits:** `blocked_tools`, `require_approval_for` and an agent's `allowed_tools`. There is no sandbox.
- **Audit trail:** approval requests and decisions go to a hash-chained, optionally signed log. Policy blocks, delegations, logins and token issuance are not written to it.

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
