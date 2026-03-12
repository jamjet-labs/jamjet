# Java SDK Reference

The JamJet Java SDK provides a type-safe, JDK 21+ native API for defining tools, workflows, agents, and eval harnesses. It compiles to the same canonical IR as the Python SDK and YAML definitions.

---

## Installation

### Maven

```xml
<dependency>
    <groupId>dev.jamjet</groupId>
    <artifactId>jamjet-sdk</artifactId>
    <version>0.1.0</version>
</dependency>
```

### Gradle

```groovy
implementation 'dev.jamjet:jamjet-sdk:0.1.0'
```

**Requirements:** JDK 21+ (uses records, sealed interfaces, virtual threads).

---

## Core imports

```java
import dev.jamjet.workflow.Workflow;
import dev.jamjet.agent.Agent;
import dev.jamjet.tool.Tool;
import dev.jamjet.tool.ToolCall;
import dev.jamjet.tool.ToolRegistry;
import dev.jamjet.ir.IrCompiler;
import dev.jamjet.ir.IrValidator;
import dev.jamjet.eval.EvalRunner;
```

---

## Defining tools

Tools are Java records annotated with `@Tool` that implement `ToolCall<T>`. Input fields are the record components; the JSON Schema is derived automatically.

```java
import dev.jamjet.tool.Tool;
import dev.jamjet.tool.ToolCall;

@Tool(description = "Search the web for a query")
record WebSearch(String query) implements ToolCall<String> {
    @Override
    public String execute() {
        return "Results for: " + query;
    }
}
```

**Rules:**
- Must be a `record` class
- Must implement `ToolCall<T>` where T is the return type
- Record components become the input schema
- Supports nested records, enums, `Optional`, `List`

### Registering tools

```java
ToolRegistry.register(WebSearch.class);

// Export as OpenAI function-calling format
var functions = ToolRegistry.toOpenAiFunctions();
```

---

## Defining workflow state

State is a Java record — no frameworks required:

```java
record ResearchState(String question, String result, String summary) {}
```

---

## Defining workflows

```java
var wf = Workflow.<ResearchState>builder("research")
        .version("0.1.0")
        .state(ResearchState.class)
        .step("search", state ->
            new ResearchState(state.question(), "found: " + state.question(), null))
        .step("summarize", state ->
            new ResearchState(state.question(), state.result(), "Summary of " + state.result()))
        .build();
```

### Conditional routing

```java
var wf = Workflow.<ResearchState>builder("branching")
        .state(ResearchState.class)
        .step("classify", s -> s, step -> step
            .next(s -> s.result() != null, "summarize")
            .next(s -> s.result() == null, "search"))
        .step("search", s -> s)
        .step("summarize", s -> s)
        .build();
```

### Running locally (no runtime needed)

```java
var result = wf.run(new ResearchState("What is JamJet?", null, null));
System.out.println(result.state().summary());
```

### Compiling to IR

```java
var ir = wf.compile();
System.out.println(ir.toJson());  // canonical IR, same as Python SDK

// Validate before submitting
var errors = IrValidator.validate(ir);
```

---

## Defining agents

```java
var agent = Agent.builder("researcher")
        .model("gpt-4o")
        .tools(WebSearch.class)
        .instructions("Research and summarize the topic.")
        .strategy("react")          // react | plan-and-execute | critic
        .maxIterations(5)
        .build();

// Execute against a prompt (requires OPENAI_API_KEY or OPENAI_BASE_URL)
var result = agent.run("What is JamJet?");
System.out.println(result.output());
```

### Strategies

| Strategy | Description |
|----------|-------------|
| `react` | Think → act → observe loop (default) |
| `plan-and-execute` | Generate plan → execute steps → verify |
| `critic` | Draft → critique → revise loop |

### Compiling agent to IR

```java
var ir = agent.compile();
// Submit to runtime via client
client.createWorkflow(ir.toMap());
```

---

## Task interface (simplest API)

For the simplest agent definition, use the `@Task` annotation on an interface:

```java
import dev.jamjet.agent.Task;
import dev.jamjet.agent.JamjetAgent;

interface Researcher {
    @Task("Research the given topic and return a summary")
    String research(String topic);
}

// Execute via dynamic proxy
var result = JamjetAgent.run(Researcher.class, "What is JamJet?");
```

---

## Runtime client

```java
import dev.jamjet.JamjetClient;
import dev.jamjet.client.ClientConfig;

var client = new JamjetClient(ClientConfig.builder()
        .baseUrl("http://localhost:7700")
        .apiToken("your-token")
        .build());

// Submit workflow
client.createWorkflow(ir.toMap());

// Start execution
var exec = client.startExecution("research", Map.of("question", "What is JamJet?"));

// Check status
var status = client.getExecution(exec.get("execution_id").toString());

// Human approval
client.approve(executionId, Map.of("decision", "approved"));

// List agents
var agents = client.listAgents();
```

---

## IR validation

The `IrValidator` mirrors the Rust runtime validator to catch errors before submission:

```java
var ir = workflow.compile();
var errors = IrValidator.validate(ir);

if (!errors.isEmpty()) {
    errors.forEach(System.err::println);
}

// Or throw on first error
IrValidator.validateOrThrow(ir);
```

Checks performed:
- `workflow_id` non-empty
- `version` is valid semver
- `start_node` exists in nodes
- All edge targets exist (or are `"end"`)
- All nodes reachable from start via BFS
- `tool_ref`, `model_ref`, MCP server, and remote agent references resolve

---

## Eval harness

```java
import dev.jamjet.eval.*;

var dataset = EvalDataset.fromJsonl(Path.of("eval/cases.jsonl"));

var runner = EvalRunner.builder()
        .scorer(new Scorer.AssertionScorer("output", "expected"))
        .scorer(new Scorer.LatencyScorer())
        .parallelism(4)
        .build();

var results = runner.run(dataset, row -> {
    // Your agent/workflow execution
    return agent.run(row.get("input").toString()).output();
});

results.forEach(r -> System.out.printf("%s: %.2f%n", r.id(), r.score()));
```

---

## CLI

The Java CLI (`jamjet-cli` module) provides command-line access:

```bash
# Check runtime health
jamjet health

# Start a workflow execution
jamjet run <workflow-id> [input.json]

# Validate IR file (uses full IrValidator)
jamjet validate workflow.json

# List agents / executions
jamjet agents list
jamjet executions list
```

---

## Examples

Three complete examples are included in `sdk/java/examples/`:

| Example | Description |
|---------|-------------|
| `basic-tool-flow` | Single agent + tool |
| `plan-and-execute-agent` | Strategy-based agent with iteration limits |
| `rag-assistant` | Retrieval-augmented workflow |

Run any example:

```bash
cd sdk/java/examples/basic-tool-flow
mvn compile exec:java
```

---

## Next steps

- [Python SDK Reference](python-sdk.md) — equivalent Python API
- [YAML Reference](yaml-reference.md) — declarative workflow authoring
- [Eval Guide](eval.md) — evaluation framework details
- [Workflow Authoring Guide](workflow-authoring.md) — patterns and best practices
