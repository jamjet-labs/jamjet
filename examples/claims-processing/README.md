# Claims Processing Agent

An insurance claims processing pipeline using JamJet's native Python SDK. Four specialist agents (intake, assessor, fraud analyst, resolution writer) collaborate through a typed Pydantic workflow with a human approval step for high-value claims.

## Use case

A multi-step agent that processes insurance claims end-to-end:
- Analyzes damage photos and claim descriptions
- Looks up customer policy coverage
- Estimates repair cost based on damage + policy terms
- Runs fraud detection checks
- Routes high-value claims (>$10K) to human adjusters for approval
- Generates professional decision letters
- Produces a complete audit trail for compliance

## Architecture

```
intake → analyze_photos → lookup_policy → assess_damage → check_fraud → approve → resolve
                                                                           ↓ (>$10K)
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

The agents call Anthropic models, so set a key first:

```bash
pip install 'jamjet[model]'
export ANTHROPIC_API_KEY=sk-ant-...
python examples/claims-processing/main.py           # in-process, no runtime needed
python examples/claims-processing/main.py CLM-5000  # another claim id
```

For an agent that runs on the durable engine (event log, replay, crash
recovery), see [`react-agent-durable`](../react-agent-durable).

## Key features demonstrated

| Feature | How it's used |
|---------|---------------|
| Native Python SDK | `Agent`, `Workflow`, `@workflow.step`, Pydantic state |
| Agent strategies | react, plan-and-execute, critic — each suited to the task |
| Typed state | `ClaimsState(BaseModel)` validated at every step |
| Human-in-the-loop | `human_approval=True` marks the approval step for high-value claims |
| Tools | `@tool` decorated functions for photos, policy DB, fraud check |

## Comparing with Google ADK

This example is a direct comparison with Google ADK's agent patterns. See the [blog post](https://jamjet.dev/blog/google-adk-vs-jamjet/) for a side-by-side walkthrough showing the same claims agent in both frameworks.
