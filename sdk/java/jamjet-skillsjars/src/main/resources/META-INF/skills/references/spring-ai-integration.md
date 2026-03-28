# Spring AI Integration Quickstart

## Step 1: Add dependency

```xml
<dependency>
    <groupId>dev.jamjet</groupId>
    <artifactId>jamjet-spring-boot-starter</artifactId>
    <version>0.1.0-SNAPSHOT</version>
</dependency>
```

## Step 2: Start the JamJet runtime

```bash
docker run -p 7700:7700 ghcr.io/jamjet-labs/jamjet:latest
```

## Step 3: Configure

```properties
spring.jamjet.runtime-url=http://localhost:7700
```

## Step 4: Use ChatClient normally

```java
@Bean
ChatClient chatClient(ChatClient.Builder builder) {
    return builder.build();
}

@Bean
CommandLineRunner demo(ChatClient chatClient) {
    return args -> {
        String result = chatClient.prompt("Summarize AI trends").call().content();
        System.out.println(result);
    };
}
```

## What you get

| Feature | Default | Config |
|---------|---------|--------|
| Durable execution | On | `spring.jamjet.durability-enabled` |
| Audit trails | On | `spring.jamjet.audit.enabled` |
| Human approval | Off | `spring.jamjet.approval.enabled` |
| Micrometer metrics | On (with actuator) | `spring.jamjet.observability.micrometer` |
| OpenTelemetry spans | Off | `spring.jamjet.observability.opentelemetry` |

## Requirements

- Java 21+, Spring Boot 3.4+, Spring AI 1.0+, JamJet runtime
