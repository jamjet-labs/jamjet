# Audit Trail Pattern

## What

Compliance-ready audit logging for every AI agent interaction — prompts, responses, tool calls, and token usage.

## Setup

Audit trails are enabled by default in the Spring Boot starter:

```properties
spring.jamjet.audit.enabled=true
spring.jamjet.audit.include-prompts=true
spring.jamjet.audit.include-responses=true
```

The `JamjetAuditAdvisor` intercepts every ChatClient call and logs:
- Inbound prompts (before LLM call)
- Outbound responses (after LLM call)
- Tool calls made during the interaction
- Token usage and estimated cost

## Querying the audit trail

```java
@Autowired
JamjetAuditService auditService;

// By execution
var entries = auditService.queryByExecution("exec-123", 50, 0);

// By actor (user)
var userEntries = auditService.queryByActor("user-456", 50, 0);

// By event type
var toolCalls = auditService.queryByEventType("ToolCall", 50, 0);
```

## Compliance use cases

- **SOC 2:** Full audit trail of all AI interactions with timestamps
- **GDPR:** Query by actor ID to find all data associated with a user
- **Internal policy:** Track which tools were invoked and by whom
