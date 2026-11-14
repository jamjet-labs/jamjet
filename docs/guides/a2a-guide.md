# A2A Integration Guide

The runtime does not delegate tasks over A2A and does not serve an A2A
endpoint. Registration refuses `a2a_task` nodes (400): the engine has no
executor for them, so they would never run. See
[Node types](yaml-reference.md#node-types). What works today is reading Agent
Cards.

---

## Reading a remote Agent Card

```bash
jamjet a2a discover https://agents.partner.com
```

The CLI fetches `/.well-known/agent.json` at the URL's origin (its scheme,
host and port; any path, query or fragment is dropped) and prints it:

```
Fetching Agent Card from: https://agents.partner.com/.well-known/agent.json

                     Agent Card
┏━━━━━━━━━━━━━┳━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃ Field       ┃ Value                              ┃
┡━━━━━━━━━━━━━╇━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┩
│ Name        │ Code Reviewer Agent                │
│ Description │ Reviews Python and TypeScript code │
│ Version     │ 1.0.0                              │
│ URL         │ https://agents.partner.com         │
│ Input Modes │ text                               │
└─────────────┴────────────────────────────────────┘
                          Skills
┏━━━━━━━━━━━━━┳━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃ Name        ┃ Description                              ┃
┡━━━━━━━━━━━━━╇━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┩
│ Code review │ Review code for bugs and security issues │
└─────────────┴──────────────────────────────────────────┘
```

It only reads the card. Nothing is registered.

---

## Registering a remote agent

`POST /agents/discover` with `{"url": "<base url>"}` (or
`jamjet agents discover <url> --register`) fetches the same card, at the
origin's `/.well-known/agent.json`, and registers it in the runtime's agent
registry. An agent served under a path (`https://host/team/x`) is read and
registered from `https://host/.well-known/agent.json`.
The document must be a JamJet Agent Card (`id`, `uri`, `name`, `capabilities`,
`autonomy`, `auth` and the other fields in the
[Agent Model Guide](agent-model-guide.md)). A standard A2A card does not parse,
and the call fails with `parse Agent Card`.

A registered agent can be named by a workflow's `agent_ref` or a node's
`acts_for`, which holds those nodes to the card's constraints. Nothing in the
runtime sends work to the agent's URL.
