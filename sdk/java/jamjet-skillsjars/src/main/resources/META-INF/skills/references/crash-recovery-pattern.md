# Crash Recovery Pattern

## What

If your application crashes mid-agent-call, JamJet resumes execution from the last checkpoint instead of restarting from scratch.

## How it works

1. The `JamjetDurabilityAdvisor` checks for an existing execution on each call
2. If an execution exists with status `Paused` or `Failed`, it fetches the event log
3. Events reveal the last completed node — execution resumes from there
4. The advisor tracks active executions via a `ConcurrentHashMap<sessionId, executionId>`

## What gets recovered

- Workflow state (which nodes completed, which are pending)
- Event history (all prior tool calls, LLM responses)
- Session context (conversation ID, user metadata)

## What doesn't get recovered

- In-flight LLM responses (the current streaming response is lost)
- Local variables in your application code (only JamJet-tracked state survives)

## Configuration

Crash recovery is automatic when durability is enabled. No additional configuration needed.

```properties
spring.jamjet.durability-enabled=true
```

## Runtime requirement

```bash
docker run -p 7700:7700 ghcr.io/jamjet-labs/jamjet:latest
```
