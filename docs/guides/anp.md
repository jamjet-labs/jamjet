# ANP Decentralized Discovery Guide

> **Status:** Experimental — Phase 2. This guide will be written when the ANP adapter is implemented.

---

## Overview

JamJet supports **ANP (Agent Network Protocol)** as a decentralized agent discovery mechanism. ANP uses DID (Decentralized Identifiers) so agents can discover each other without a central registry or broker.

ANP complements A2A: use ANP to *discover* agents across organizations, use A2A to *delegate tasks* to them.

## When to use ANP vs A2A vs local registry

| Scenario | Recommended |
|----------|-------------|
| Agents within your JamJet instance | Local registry |
| Known external agents at fixed URLs | A2A direct |
| Discovery across organizations without a central directory | ANP |

## Planned API

```python
from jamjet import AgentRegistry

registry = AgentRegistry()

# Discover agents via ANP (DID-based, no central broker)
agents = await registry.discover_anp(did="did:web:agents.partner.com")

# Use the discovered agent
result = await agents[0].invoke(skill="code_review", input={"code": code})
```

## Planned YAML shape

```yaml
agents:
  reviewer:
    discovery:
      protocol: anp
      did: did:web:agents.partner.com:reviewer
```

## Status

ANP is marked **experimental** because the protocol is less widely adopted than MCP and A2A. JamJet implements ANP via the same `ProtocolAdapter` trait, so it carries no architectural risk — it can be updated or replaced as the protocol matures.

## Tracking

Follow progress in `progress-tracker.md` under Phase 2, Workstream I tasks I2.1–I2.4.
