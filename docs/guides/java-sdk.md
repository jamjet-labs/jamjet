# Java SDK Reference

The JamJet Java SDK provides a type-safe, JDK 21+ native API for defining tools, workflows, agents, and eval harnesses. It compiles to the same canonical IR as the Python SDK and YAML definitions.

---

## Installation

### Maven

```xml
<dependency>
    <groupId>dev.jamjet</groupId>
    <artifactId>jamjet-sdk</artifactId>
    <version>0.5.1</version>
</dependency>
```

### Gradle

```groovy
implementation 'dev.jamjet:jamjet-sdk:0.5.1'
```

**Requirements:** JDK 21+ (uses records, sealed interfaces, virtual threads).

Use 0.5.1 or later with the runtime client. Earlier versions sent every runtime call to `/api/v1/...`, a prefix the runtime has never served.

---

## Core imports

```java
import dev.jamjet.workflow.Workflow;
import dev.jamjet.workflow.Step;
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
var registry = ToolRegistry.global().register(WebSearch.class);

// Export in OpenAI function-calling format
var functions = registry.toOpenAiFormat();
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

Configure a step with `Step.builder` to route on state. Conditions run in declaration order and the first match wins; `defaultNext` applies when none match.

```java
var wf = Workflow.<ResearchState>builder("branching")
        .state(ResearchState.class)
        .step("classify", Step.builder("classify")
                .handler((ResearchState s) -> s)
                .when("summarize", (ResearchState s) -> s.result() != null)
                .defaultNext("search")
                .build())
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
// Register with the runtime (see "Runtime client" below)
client.createWorkflow(ir.toMap());
```

---

## Task interface (simplest API)

For the simplest agent definition, annotate an interface with `@Task`. The annotation goes on the interface and carries the agent's configuration (`model`, `tools`, `strategy`, `maxIterations`, `instructions`):

```java
import dev.jamjet.agent.Task;
import dev.jamjet.agent.JamjetAgent;

@Task(instructions = "Research the given topic and return a summary", strategy = "react")
interface Researcher {
    String research(String topic);
}

// Runs the agent and returns its text output
String result = JamjetAgent.run(Researcher.class, "What is JamJet?");
```

---

## Runtime client

`JamjetClient` calls the runtime's REST API at its bare routes (`/workflows`, `/executions`, `/agents`). Every method returns the runtime's JSON response as a `Map`. A non-2xx response throws `JamjetApiException` (or `JamjetAuthException` for 401/403) with the status code and body.

```java
import dev.jamjet.JamjetClient;
import dev.jamjet.client.ClientConfig;

var client = new JamjetClient(ClientConfig.builder()
        .baseUrl("http://localhost:7700")
        .apiToken("your-token")   // or set JAMJET_TOKEN; not needed in dev mode
        .build());

// Register a workflow definition: POST /workflows {"ir": ...}
var ir = wf.compile();
client.createWorkflow(ir.toMap());   // {"workflow_id": "research", "version": "0.1.0"}

// Start an execution: POST /executions
var exec = client.startExecution(ir.id(), ir.version(), Map.of("question", "What is JamJet?"));
var executionId = exec.get("execution_id").toString();

// Read its status and event log
var status = client.getExecution(executionId).get("status");   // "running", "completed", ...
var events = client.getEvents(executionId).get("events");

// List executions: {"executions": [...]}
var recent = client.listExecutions("running", 20, 0);

// Cancel
client.cancelExecution(executionId);
```

The runtime has no route that lists workflow definitions, so `listWorkflows()` is deprecated and throws `UnsupportedOperationException` without calling the runtime.

Pass the version, as above. `startExecution(workflowId, input)` sends none, and the runtime then runs version `1.0.0`, but the `Workflow` builder defaults to `0.1.0`. For a builder workflow that keeps the default, the two-argument call answers 404 (`workflow research v1.0.0`).

A compiled Java workflow registers and starts, but its steps do not run on the runtime. The runtime queues each step as a `python_tool` work item, and there is no Java worker to claim it; a Python worker would claim it and fail to import the step. Step lambdas only run in-process, through `wf.run(...)`.

### Human approval

An approval decides one exact action. Read the pending request first, show the reviewer what it covers, then send back its `node_id` and `context.action_digest` with the decision:

```java
// GET /executions/:id/approvals -> {"pending": [...], "decided": [...]}
var approvals = client.getApprovals(executionId);

@SuppressWarnings("unchecked")
var pending = (List<Map<String, Object>>) approvals.get("pending");
var request = pending.get(0);

@SuppressWarnings("unchecked")
var context = (Map<String, Object>) request.get("context");

// ... the reviewer inspects `context` ...

client.approve(executionId, "approved",
        (String) request.get("node_id"),
        (String) context.get("action_digest"),
        "looks right");
```

Runtimes with approval integrity refuse an `approved` decision outside dev mode when the digest is missing (400) or no longer matches the pending action (409). Older runtimes ignore the field, so sending it is always safe. A rejection needs no digest.

### External events and agents

```java
// Deliver an event to an execution waiting on a correlation key
client.sendExternalEvent(executionId, "order-42", Map.of("status", "paid"));

// Agents: {"agents": [...]}
var agents = client.listAgents();
```

---

## IR validation

The `IrValidator` mirrors the Rust runtime validator to catch errors before submission:

```java
var ir = wf.compile();
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

Each line of the dataset is a JSON object. A row's `expected_contains` list is scored automatically:

```json
{"input": "What is JamJet?", "expected_contains": ["runtime"]}
```

```java
import dev.jamjet.eval.*;

var dataset = EvalDataset.fromFile(Path.of("eval/cases.jsonl"));

var results = EvalRunner.builder()
        .dataset(dataset)
        .agent(agent)                       // or .runFn(input -> ...) for any code
        .scorers(new Scorer.LatencyScorer(2_000_000, 10_000_000))   // target, max (µs)
        .parallelism(4)
        .failBelow(0.8)                     // throws EvalFailureException below this mean
        .build()
        .run();

results.forEach(r -> System.out.printf("%s: %.2f%n", r.input(), r.averageScore()));
```

---

## CLI

The Java CLI (`jamjet-cli` module) provides command-line access:

```bash
# Check runtime health
jamjet health

# Start an execution of a registered workflow (version 1.0.0 unless given)
jamjet run <workflow-id> [input.json]
jamjet run research input.json --workflow-version 0.1.0   # a Workflow-builder default

# Print the CLI version
jamjet -V

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
