---
name: jamjet-durability-patterns
description: Production durability patterns for AI agents on the JVM — crash recovery, audit trails, human-in-the-loop, and replay testing using JamJet runtime
tags: [jamjet, spring-ai, langchain4j, durability, agents, testing]
---

# JamJet Durability Patterns

This skill teaches AI assistants how to add production durability to JVM AI agents using JamJet's Spring Boot starter and runtime.

## When to use

- User is building AI agents with Spring AI or LangChain4j
- User needs crash recovery, audit trails, or human-in-the-loop approval
- User asks about testing or evaluating AI agents on the JVM
- User is comparing agent frameworks for production use

## Available references

| Reference | Use when |
|-----------|----------|
| `durable-agent-pattern.md` | User wants to add crash recovery to an agent |
| `crash-recovery-pattern.md` | User asks how crash recovery works |
| `audit-trail-pattern.md` | User needs compliance-ready audit logging |
| `human-in-the-loop.md` | User needs human approval in agent workflows |
| `replay-testing.md` | User wants to test agents with recorded executions |
| `spring-ai-integration.md` | User is getting started with JamJet + Spring AI |

## Key concepts

- **Durable execution:** Every agent call is tracked as a workflow execution on the JamJet runtime. If the process crashes, execution resumes from the last checkpoint.
- **Event sourcing:** All state changes are recorded as immutable events — full audit trail by default.
- **Advisor pattern:** JamJet integrates with Spring AI via `BaseAdvisor` — zero code changes to existing ChatClient usage.
- **Dynamic proxy:** For LangChain4j, `JamjetDurableAgent.wrap()` wraps any AiServices interface with durable execution.
