# Durable Agent Pattern

## What

Wrap AI agent calls with JamJet durable execution so every interaction is tracked, event-sourced, and crash-recoverable.

## Spring AI (recommended)

Add the starter dependency — durability is automatic via the advisor pattern:

```xml
<dependency>
    <groupId>dev.jamjet</groupId>
    <artifactId>jamjet-spring-boot-starter</artifactId>
    <version>0.1.0-SNAPSHOT</version>
</dependency>
```

```properties
spring.jamjet.runtime-url=http://localhost:7700
```

```java
@Bean
ChatClient chatClient(ChatClient.Builder builder) {
    // JamjetDurabilityAdvisor is auto-injected via ChatClientCustomizer
    return builder.build();
}

// Every call is now durable
String result = chatClient.prompt("Summarize the report").call().content();
```

No code changes needed. The `JamjetDurabilityAdvisor` intercepts every `ChatClient.call()`, compiles a workflow IR, starts an execution on the runtime, and records completion events.

## LangChain4j

Wrap any AiServices interface with `JamjetDurableAgent.wrap()`:

```xml
<dependency>
    <groupId>dev.jamjet</groupId>
    <artifactId>langchain4j-jamjet</artifactId>
    <version>0.1.0-SNAPSHOT</version>
</dependency>
```

```java
var client = new JamjetConfig()
    .runtimeUrl("http://localhost:7700")
    .buildClient();

MyAssistant raw = AiServices.builder(MyAssistant.class)
    .chatLanguageModel(model)
    .tools(searchTool)
    .build();

MyAssistant durable = JamjetDurableAgent.wrap(raw, MyAssistant.class, client);
```

## Configuration

```properties
spring.jamjet.runtime-url=http://localhost:7700
spring.jamjet.api-token=${JAMJET_API_TOKEN}
spring.jamjet.tenant-id=default
spring.jamjet.durability-enabled=true
```
