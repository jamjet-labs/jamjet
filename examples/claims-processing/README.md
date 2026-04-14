# Claims Processing Agent

An insurance claims processing pipeline that demonstrates JamJet's production-grade features: durable execution, human-in-the-loop approval, parallel tool orchestration, quality evaluation, and full audit trails.

## Use case

A multi-step agent that processes insurance claims end-to-end:
- Analyzes damage photos and claim descriptions
- Looks up customer policy coverage in parallel
- Estimates repair cost based on damage + policy terms
- Runs fraud detection checks
- Routes high-value claims (>$10K) to human adjusters for approval
- Generates decision letters with quality scoring
- Produces a complete audit trail for compliance

## Why this example matters

This is the kind of workflow where framework choice has real consequences:
- **Crash at step 5?** JamJet resumes from the checkpoint. Other frameworks restart from scratch.
- **$47K claim?** The `wait` node pauses for human approval — and survives restarts.
- **Regulator audit?** `jamjet inspect` shows every decision, prompt, and tool call.
- **Quality drift?** The eval node catches bad decision letters before they ship.

## Architecture

```
intake → ┬─ photo_analysis ─┬→ assessment → fraud_check → approval_gate → resolution → evaluate
         └─ policy_lookup  ─┘                                ↓ (>$10K)
                                                        human_review
```

## Run

```bash
# Start the runtime
jamjet dev

# Run the example
python examples/claims-processing/main.py

# Inspect the execution
jamjet inspect <execution_id> --events

# Replay a past execution
jamjet replay <execution_id>
```

## Key features demonstrated

| Feature | How it's used |
|---------|---------------|
| Durable execution | Every step checkpointed — crash-safe |
| Parallel branches | Photo analysis + policy lookup run concurrently |
| Human-in-the-loop | `wait` node for high-value claim approval |
| Eval harness | LLM judge scores decision letter quality |
| MCP tools | Database and vision tools via MCP servers |
| Cost tracking | Per-step token and cost attribution |
| Audit trail | Full execution trace for compliance |

## Comparing with Google ADK

This example is designed as a direct comparison with Google ADK's agent patterns. See the [blog post](https://jamjet.dev/blog/google-adk-vs-jamjet/) for a side-by-side walkthrough.
