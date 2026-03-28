# Replay Testing Pattern

## What

Record production agent executions and replay them in JUnit 5 tests — deterministic, reproducible agent testing.

## Setup

```xml
<dependency>
    <groupId>dev.jamjet</groupId>
    <artifactId>jamjet-spring-boot-starter-test</artifactId>
    <version>0.1.0-SNAPSHOT</version>
    <scope>test</scope>
</dependency>
```

## Replay tests

```java
@WithJamjetRuntime
class MyAgentTest {

    @Test
    @ReplayExecution("exec-abc123")
    void agentProducesConsistentOutput(RecordedExecution execution) {
        AgentAssertions.assertThat(execution)
            .completedSuccessfully()
            .usedTool("WebSearch")
            .completedWithin(30, TimeUnit.SECONDS)
            .costLessThan(0.50);
    }
}
```

## Deterministic stubs

```java
var stub = DeterministicModelStub.builder()
    .onPromptContaining("weather", "Sunny, 72F")
    .onPromptContaining("stock", "AAPL at $150")
    .defaultResponse("I don't know")
    .build();
```

## Assertion API

```java
AgentAssertions.assertThat(execution)
    .completedSuccessfully()
    .completedWithin(30, SECONDS)
    .costLessThan(0.50)
    .usedTool("WebSearch")
    .usedToolTimes("WebSearch", 2)
    .didNotUseTool("DeleteFile")
    .nodeCompleted("search-node")
    .outputContains("revenue");
```
