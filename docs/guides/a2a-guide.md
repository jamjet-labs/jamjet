# A2A Integration Guide

Delegate to external agents and expose your JamJet agent to the world via the A2A protocol.

---

## Discovering a Remote Agent

```bash
jamjet a2a discover https://agents.partner.com
```

Output:
```
Agent Card: https://agents.partner.com/.well-known/agent.json
  Name: Code Reviewer Agent
  Skills:
    - code_review     Review Python/TypeScript code for bugs and security issues
    - security_audit  Full security audit of a codebase
  Protocols: a2a
  Auth: bearer_token
```

---

## Delegating to an External A2A Agent

### Configuration

```yaml
# agents.yaml
agents:
  pipeline:
    model: default_chat
    a2a:
      remote_agents:
        code_reviewer:
          url: https://agents.partner.com
          auth:
            type: bearer
            token_env: PARTNER_TOKEN
```

### Workflow node

```yaml
nodes:
  review:
    type: a2a_task
    remote_agent: code_reviewer
    skill: code_review
    input:
      code: "{{ state.generated_code }}"
      language: python
    output_schema: schemas.ReviewResult
    stream: true
    timeout: 300s
    on_input_required: human_review   # if remote needs more info, route to HITL
    next: fix_issues
```

### What happens at runtime

1. JamJet fetches the Agent Card from `/.well-known/agent.json`
2. Submits the task via `tasks/send` or `tasks/sendSubscribe`
3. If `stream: true`, consumes SSE stream — progress appears in execution traces
4. If remote returns `input-required`, JamJet pauses the workflow and routes to `on_input_required`
5. On `completed`, maps result artifacts into workflow state

All of this is **durable** — a crash mid-delegation is handled safely. JamJet re-tracks the remote task using the stored `task_id`.

---

## Publishing Your Agent as an A2A Server

```yaml
# agents.yaml
agents:
  research_analyst:
    model: default_chat
    a2a:
      expose:
        enabled: true
        port: 8081
        skills:
          - name: deep_research
            description: Research any topic and produce a structured report
            input_schema: schemas.ResearchQuery
            output_schema: schemas.ResearchReport
```

Once activated, your agent publishes:
- `GET /.well-known/agent.json` — Agent Card
- `POST /` — A2A JSON-RPC endpoint (tasks/send, tasks/get, tasks/cancel)

Any A2A-compatible client or framework can now discover and invoke your agent.

---

## Multi-Turn Interaction

A2A supports multi-turn tasks via `input-required` state. If your agent needs more information mid-task, it returns `input-required`. The calling agent or human provides additional context and resumes.

In JamJet workflows, `on_input_required` routes to a designated node (typically a `human_approval` node) when this state is received.
