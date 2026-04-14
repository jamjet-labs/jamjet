# Claims Processing Agent

An insurance claims processing pipeline using JamJet's native Python SDK. Four specialist agents — intake, assessor, fraud analyst, resolution writer — collaborate through a typed Pydantic workflow with durable execution, human-in-the-loop approval, and full audit trails.

## Use case

A multi-step agent that processes insurance claims end-to-end:
- Analyzes damage photos and claim descriptions
- Looks up customer policy coverage in parallel
- Estimates repair cost based on damage + policy terms
- Runs fraud detection checks
- Routes high-value claims (>$10K) to human adjusters for approval
- Generates professional decision letters
- Produces a complete audit trail for compliance

## Why this example matters

This is the kind of workflow where framework choice has real consequences:
- **Crash at step 5?** JamJet resumes from the checkpoint. Other frameworks restart from scratch.
- **$47K claim?** The `human_approval=True` step pauses for review — and survives restarts.
- **Regulator audit?** `jamjet inspect` shows every decision, prompt, and tool call.

## Architecture

```
intake → ┬─ analyze_photos ─┬→ assess_damage → check_fraud → approve → resolve
         └─ lookup_policy  ─┘                                 ↓ (>$10K)
                                                         human review
```

## Agents

| Agent | Role | Strategy | Model |
|-------|------|----------|-------|
| Intake Specialist | Extracts claim details, analyzes photos | react | claude-sonnet-4-6 |
| Damage Assessor | Estimates repair cost vs policy terms | plan-and-execute | claude-sonnet-4-6 |
| Fraud Analyst | Cross-references claim history | react | claude-haiku-4-5 |
| Resolution Writer | Generates decision letter | critic | claude-sonnet-4-6 |

## Run

```bash
# Local (in-process, no runtime needed)
python examples/claims-processing/main.py

# On JamJet runtime (durable execution)
jamjet dev
python examples/claims-processing/main.py --runtime

# Inspect and replay
jamjet inspect <exec_id> --events
jamjet replay <exec_id>
```

## Key features demonstrated

| Feature | How it's used |
|---------|---------------|
| Native Python SDK | `Agent`, `Workflow`, `@workflow.step`, Pydantic state |
| Agent strategies | react, plan-and-execute, critic — each suited to the task |
| Typed state | `ClaimsState(BaseModel)` with compile-time validation |
| Durable execution | Every step checkpointed — crash-safe |
| Parallel branches | Photo analysis + policy lookup run concurrently |
| Human-in-the-loop | `human_approval=True` for high-value claims |
| Tools | `@tool` decorated functions for photos, policy DB, fraud check |
| Cost tracking | Per-step token and cost attribution |
| Audit trail | Full execution trace for compliance |

## Comparing with Google ADK

This example is a direct comparison with Google ADK's agent patterns. See the [blog post](https://jamjet.dev/blog/google-adk-vs-jamjet/) for a side-by-side walkthrough showing the same claims agent in both frameworks.
