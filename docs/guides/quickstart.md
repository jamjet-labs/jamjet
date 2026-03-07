# Quickstart

Get a JamJet workflow running locally in under 10 minutes.

---

## Prerequisites

- Python 3.11+

---

## 1. Install the CLI

```bash
pip install jamjet
```

Verify:
```bash
jamjet --version
```

---

## 2. Create a project

```bash
jamjet init my-first-agent
cd my-first-agent
```

This creates:
```
my-first-agent/
  workflow.yaml       # workflow definition
  agents.yaml         # agent definitions
  tools.yaml          # tool definitions
  schemas.py          # Pydantic state schemas
  prompts/
    summarize.md      # prompt template
  jamjet.yaml         # project config
```

---

## 3. Start local dev mode

```bash
jamjet dev
```

This starts the local runtime (SQLite-backed) in the background. You'll see:

```
JamJet runtime started
  Mode: local (SQLite)
  API: http://localhost:7700
  Ready.
```

---

## 4. Validate your workflow

```bash
jamjet validate workflow.yaml
```

---

## 5. Run it

```bash
jamjet run workflow.yaml --input '{"question": "What is the capital of France?"}'
```

Output:
```
Execution started: exec_01abc...
  [fetch]     running...  done (1.2s)
  [summarize] running...  done (2.1s)
Completed in 3.4s

Result:
  answer: "The capital of France is Paris."
```

---

## 6. Inspect the execution

```bash
jamjet inspect exec_01abc
```

```bash
# See the full event timeline
jamjet events exec_01abc
```

---

## Next Steps

- [Core Concepts](concepts.md) — understand agents, workflows, nodes, and state
- [Workflow Authoring](workflow-authoring.md) — full authoring guide
- [MCP Integration](mcp-guide.md) — connect to an external MCP tool server
- [Python SDK](python-sdk.md) — define workflows in Python
