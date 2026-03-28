# Human-in-the-Loop Pattern

## What

Pause agent execution for human approval before proceeding. Useful for high-stakes actions.

## Setup

```properties
spring.jamjet.approval.enabled=true
spring.jamjet.approval.timeout=30m
spring.jamjet.approval.default-decision=rejected
spring.jamjet.approval.webhook-url=https://hooks.slack.com/...
```

## How it works

1. `JamjetApprovalAdvisor` intercepts the ChatClient call
2. Marks the request as requiring approval
3. Blocks the thread (with configurable timeout) waiting for a decision
4. On approval: continues to the LLM; on rejection: throws `ApprovalRejectedException`

## Approval REST endpoints

```bash
# Submit approval decision
POST /jamjet/approvals/{executionId}
{"decision": "approved", "userId": "admin", "comment": "Looks good"}

# List pending approvals
GET /jamjet/approvals/pending
```

## Integration with Slack/Teams

Set `spring.jamjet.approval.webhook-url` to receive notifications when approval is needed. Your webhook handler can then call the approval endpoint.
