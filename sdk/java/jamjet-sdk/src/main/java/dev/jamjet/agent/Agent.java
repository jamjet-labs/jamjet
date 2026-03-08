package dev.jamjet.agent;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.ObjectMapper;
import dev.jamjet.ir.IrCompiler;
import dev.jamjet.ir.WorkflowIr;
import dev.jamjet.tool.ToolCall;
import dev.jamjet.tool.ToolDefinition;
import dev.jamjet.tool.ToolRegistry;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.io.IOException;
import java.lang.reflect.Constructor;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Executors;

/**
 * Fluent agent builder and in-process executor.
 *
 * <p>An agent wraps a model, a set of tools, and a reasoning strategy. It can be compiled to
 * canonical IR for submission to the runtime, or executed in-process via {@link #run(String)}.
 *
 * <pre>{@code
 * var agent = Agent.builder("researcher")
 *     .model("gpt-4o")
 *     .tools(WebSearch.class)
 *     .instructions("Research and summarize the topic.")
 *     .strategy("react")
 *     .build();
 *
 * var result = agent.run("What is JamJet?");
 * System.out.println(result.output());
 * }</pre>
 */
public final class Agent {

    private static final Logger log = LoggerFactory.getLogger(Agent.class);
    private static final ObjectMapper MAPPER = new ObjectMapper();

    private final String name;
    private final String model;
    private final List<ToolDefinition> tools;
    private final String instructions;
    private final String strategy;
    private final int maxIterations;
    private final double maxCostUsd;
    private final int timeoutSeconds;
    private final ToolRegistry registry;

    private Agent(Builder builder) {
        this.name = builder.name;
        this.model = builder.model;
        this.instructions = builder.instructions;
        this.strategy = builder.strategy;
        this.maxIterations = builder.maxIterations;
        this.maxCostUsd = builder.maxCostUsd;
        this.timeoutSeconds = builder.timeoutSeconds;
        this.registry = builder.registry;
        this.tools = List.copyOf(registry.all().values());
    }

    public static Builder builder(String name) {
        return new Builder(name);
    }

    public String name() { return name; }
    public String model() { return model; }
    public String instructions() { return instructions; }
    public String strategy() { return strategy; }
    public int maxIterations() { return maxIterations; }
    public double maxCostUsd() { return maxCostUsd; }
    public int timeoutSeconds() { return timeoutSeconds; }
    public List<String> toolNames() { return tools.stream().map(ToolDefinition::name).toList(); }

    /** Compile this agent to canonical IR. */
    public WorkflowIr compile() {
        return IrCompiler.compileAgent(this);
    }

    /**
     * Run this agent in-process using the OpenAI-compatible API.
     *
     * <p>Reads {@code OPENAI_API_KEY} and {@code OPENAI_BASE_URL} from the environment.
     * Dispatches to the configured strategy executor.
     */
    public AgentResult run(String prompt) {
        var apiKey = System.getenv().getOrDefault("OPENAI_API_KEY", "");
        var baseUrl = System.getenv().getOrDefault("OPENAI_BASE_URL", "https://api.openai.com/v1");

        var client = HttpClient.newBuilder()
                .executor(Executors.newVirtualThreadPerTaskExecutor())
                .connectTimeout(Duration.ofSeconds(timeoutSeconds))
                .build();

        var toolMap = new LinkedHashMap<String, ToolDefinition>();
        for (var td : tools) toolMap.put(td.name(), td);

        var toolCallsLog = new ArrayList<Map<String, Object>>();
        var t0 = System.nanoTime();
        var ir = compile().toMap();

        var output = switch (strategy) {
            case "plan-and-execute" -> runPlanAndExecute(client, baseUrl, apiKey, prompt, toolMap, toolCallsLog);
            case "react" -> runReact(client, baseUrl, apiKey, prompt, toolMap, toolCallsLog);
            case "critic" -> runCritic(client, baseUrl, apiKey, prompt, toolMap, toolCallsLog);
            default -> throw new IllegalArgumentException(
                    "Unknown strategy '" + strategy + "'. Valid options: plan-and-execute, react, critic");
        };

        var totalUs = (System.nanoTime() - t0) / 1000L;
        return new AgentResult(output, Collections.unmodifiableList(toolCallsLog), ir, totalUs);
    }

    // ── Strategy executors ────────────────────────────────────────────────────

    private String runPlanAndExecute(
            HttpClient client, String baseUrl, String apiKey,
            String prompt, Map<String, ToolDefinition> toolMap,
            List<Map<String, Object>> toolCallsLog) {

        var system = instructions.isBlank() ? "You are a helpful assistant." : instructions;

        // 1. Generate plan
        var planMessages = new ArrayList<Map<String, Object>>();
        planMessages.add(msg("system", system));
        planMessages.add(msg("user", """
                Goal: %s

                Before executing, write a concise numbered plan (3-5 steps) \
                that you will follow to complete this goal. \
                Return only the numbered list, nothing else.""".formatted(prompt)));

        var planText = callModel(client, baseUrl, apiKey, planMessages, null).content();

        var steps = new ArrayList<String>();
        for (var line : planText.lines().toList()) {
            var trimmed = line.strip();
            if (!trimmed.isEmpty() && !trimmed.isEmpty() && Character.isDigit(trimmed.charAt(0))) {
                steps.add(trimmed);
            }
        }
        if (steps.isEmpty()) steps.add(planText);

        // 2. Execute each step
        var stepResults = new ArrayList<String>();
        for (var step : steps.subList(0, Math.min(steps.size(), maxIterations))) {
            var stepMessages = new ArrayList<Map<String, Object>>();
            stepMessages.add(msg("system", system));
            stepMessages.add(msg("user", """
                    Overall goal: %s

                    Execute this step: %s

                    Use any available tools as needed. Return the result of this step only."""
                    .formatted(prompt, step)));

            var openAiTools = toOpenAiTools();
            for (int i = 0; i < maxIterations; i++) {
                var modelMsg = callModel(client, baseUrl, apiKey, stepMessages,
                        openAiTools.isEmpty() ? null : openAiTools);
                if (modelMsg.toolCalls() == null || modelMsg.toolCalls().isEmpty()) {
                    stepResults.add(modelMsg.content() != null ? modelMsg.content() : "");
                    break;
                }
                stepMessages.add(modelMsg.toMap());
                var toolResults = executeToolCalls(modelMsg.toolCalls(), toolMap, toolCallsLog);
                stepMessages.addAll(toolResults);
                if (i == maxIterations - 1) stepResults.add("");
            }
        }

        // 3. Synthesize
        var sb = new StringBuilder();
        for (int i = 0; i < stepResults.size(); i++) {
            sb.append("Step ").append(i + 1).append(": ").append(stepResults.get(i)).append("\n\n");
        }
        var synthMessages = new ArrayList<Map<String, Object>>();
        synthMessages.add(msg("system", system));
        synthMessages.add(msg("user", """
                Goal: %s

                Plan executed:
                %s

                Step results:
                %s
                Synthesize these results into a final, well-structured answer."""
                .formatted(prompt, planText, sb)));

        return callModel(client, baseUrl, apiKey, synthMessages, null).content();
    }

    private String runReact(
            HttpClient client, String baseUrl, String apiKey,
            String prompt, Map<String, ToolDefinition> toolMap,
            List<Map<String, Object>> toolCallsLog) {

        var messages = new ArrayList<Map<String, Object>>();
        if (!instructions.isBlank()) messages.add(msg("system", instructions));
        messages.add(msg("user", prompt));

        var openAiTools = toOpenAiTools();
        for (int i = 0; i < maxIterations; i++) {
            var modelMsg = callModel(client, baseUrl, apiKey, messages,
                    openAiTools.isEmpty() ? null : openAiTools);
            if (modelMsg.toolCalls() == null || modelMsg.toolCalls().isEmpty()) {
                return modelMsg.content() != null ? modelMsg.content() : "";
            }
            messages.add(modelMsg.toMap());
            var toolResults = executeToolCalls(modelMsg.toolCalls(), toolMap, toolCallsLog);
            messages.addAll(toolResults);
        }

        // Max iterations — return last assistant content
        for (int i = messages.size() - 1; i >= 0; i--) {
            var m = messages.get(i);
            if ("assistant".equals(m.get("role")) && m.get("content") != null) {
                return (String) m.get("content");
            }
        }
        return "";
    }

    private String runCritic(
            HttpClient client, String baseUrl, String apiKey,
            String prompt, Map<String, ToolDefinition> toolMap,
            List<Map<String, Object>> toolCallsLog) {

        var system = instructions.isBlank() ? "You are a helpful assistant." : instructions;
        var openAiTools = toOpenAiTools();
        var draft = "";

        for (int roundI = 0; roundI < maxIterations; roundI++) {
            // Draft (or revise)
            String draftPrompt;
            if (roundI == 0) {
                draftPrompt = prompt;
            } else {
                draftPrompt = """
                        Original goal: %s

                        Previous draft:
                        %s

                        Revise the draft to address the critic's feedback above. \
                        Return only the improved draft.""".formatted(prompt, draft);
            }

            var draftMessages = new ArrayList<Map<String, Object>>();
            draftMessages.add(msg("system", system));
            draftMessages.add(msg("user", draftPrompt));

            for (int i = 0; i < maxIterations; i++) {
                var modelMsg = callModel(client, baseUrl, apiKey, draftMessages,
                        openAiTools.isEmpty() ? null : openAiTools);
                if (modelMsg.toolCalls() == null || modelMsg.toolCalls().isEmpty()) {
                    draft = modelMsg.content() != null ? modelMsg.content() : "";
                    break;
                }
                draftMessages.add(modelMsg.toMap());
                var toolResults = executeToolCalls(modelMsg.toolCalls(), toolMap, toolCallsLog);
                draftMessages.addAll(toolResults);
            }

            // Critic evaluation
            var criticMessages = List.of(
                    msg("system", """
                            You are a strict critic. Evaluate the draft against the goal. \
                            Reply with either PASS (if the draft is good enough) or \
                            REVISE: <specific feedback>."""),
                    msg("user", "Goal: " + prompt + "\n\nDraft:\n" + draft));

            var verdict = callModel(client, baseUrl, apiKey, new ArrayList<>(criticMessages), null)
                    .content().strip();
            if (verdict.toUpperCase().startsWith("PASS")) break;
            draft = draft + "\n\n[Critic feedback]: " + verdict;
        }

        return draft;
    }

    // ── Tool execution ────────────────────────────────────────────────────────

    private List<Map<String, Object>> executeToolCalls(
            List<ToolCallRef> toolCalls,
            Map<String, ToolDefinition> toolMap,
            List<Map<String, Object>> log) {

        var results = new ArrayList<Map<String, Object>>();
        for (var tc : toolCalls) {
            var td = toolMap.get(tc.functionName());
            String resultStr;
            if (td == null) {
                resultStr = "Error: unknown tool '" + tc.functionName() + "'";
            } else {
                var t0 = System.nanoTime();
                try {
                    var args = MAPPER.readValue(
                            tc.functionArguments() != null ? tc.functionArguments() : "{}",
                            new TypeReference<Map<String, Object>>() {});
                    resultStr = String.valueOf(invokeTool(td, args));
                    var durationUs = (System.nanoTime() - t0) / 1000L;
                    log.add(Map.of(
                            "tool", td.name(),
                            "input", args,
                            "output", resultStr,
                            "duration_us", durationUs));
                } catch (Exception e) {
                    resultStr = "Error executing tool '" + tc.functionName() + "': " + e.getMessage();
                }
            }
            results.add(Map.of(
                    "role", "tool",
                    "tool_call_id", tc.id(),
                    "content", resultStr));
        }
        return results;
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    private Object invokeTool(ToolDefinition td, Map<String, Object> args) throws Exception {
        var cls = td.cls();
        // Find the canonical constructor (record constructor)
        Constructor<?>[] constructors = cls.getDeclaredConstructors();
        if (constructors.length == 0) {
            throw new IllegalStateException("No constructor found for tool " + cls.getName());
        }
        var ctor = constructors[0];
        ctor.setAccessible(true);
        var params = ctor.getParameters();
        var ctorArgs = new Object[params.length];
        for (int i = 0; i < params.length; i++) {
            var p = params[i];
            var val = args.get(p.getName());
            ctorArgs[i] = coerce(val, p.getType());
        }
        var instance = ctor.newInstance(ctorArgs);
        if (instance instanceof ToolCall tc) {
            return tc.execute();
        }
        throw new IllegalStateException(cls.getName() + " does not implement ToolCall");
    }

    private Object coerce(Object val, Class<?> type) {
        if (val == null) return null;
        if (type == String.class) return String.valueOf(val);
        if (type == Integer.class || type == int.class) return ((Number) val).intValue();
        if (type == Long.class || type == long.class) return ((Number) val).longValue();
        if (type == Double.class || type == double.class) return ((Number) val).doubleValue();
        if (type == Boolean.class || type == boolean.class) return val;
        return val;
    }

    // ── HTTP/model call ───────────────────────────────────────────────────────

    private ModelMessage callModel(
            HttpClient client, String baseUrl, String apiKey,
            List<Map<String, Object>> messages,
            List<Map<String, Object>> toolsParam) {
        try {
            var body = new LinkedHashMap<String, Object>();
            body.put("model", model);
            body.put("messages", messages);
            if (toolsParam != null && !toolsParam.isEmpty()) {
                body.put("tools", toolsParam);
            }

            var json = MAPPER.writeValueAsBytes(body);
            var req = HttpRequest.newBuilder(URI.create(baseUrl + "/chat/completions"))
                    .POST(HttpRequest.BodyPublishers.ofByteArray(json))
                    .header("Content-Type", "application/json")
                    .header("Authorization", "Bearer " + apiKey)
                    .header("User-Agent", "jamjet-java-sdk/0.1.0")
                    .timeout(Duration.ofSeconds(timeoutSeconds))
                    .build();

            var resp = client.send(req, HttpResponse.BodyHandlers.ofString());
            if (resp.statusCode() >= 400) {
                throw new RuntimeException("Model API error " + resp.statusCode() + ": " + resp.body());
            }

            var parsed = MAPPER.readValue(resp.body(), new TypeReference<Map<String, Object>>() {});
            @SuppressWarnings("unchecked")
            var choices = (List<Map<String, Object>>) parsed.get("choices");
            if (choices == null || choices.isEmpty()) {
                return new ModelMessage("", null);
            }
            @SuppressWarnings("unchecked")
            var msgMap = (Map<String, Object>) choices.get(0).get("message");
            var content = (String) msgMap.get("content");

            @SuppressWarnings("unchecked")
            var rawToolCalls = (List<Map<String, Object>>) msgMap.get("tool_calls");
            List<ToolCallRef> toolCallRefs = null;
            if (rawToolCalls != null) {
                toolCallRefs = new ArrayList<>();
                for (var tc : rawToolCalls) {
                    @SuppressWarnings("unchecked")
                    var fn = (Map<String, Object>) tc.get("function");
                    toolCallRefs.add(new ToolCallRef(
                            (String) tc.get("id"),
                            (String) fn.get("name"),
                            (String) fn.get("arguments")));
                }
            }

            return new ModelMessage(content != null ? content : "", toolCallRefs);
        } catch (IOException | InterruptedException e) {
            if (e instanceof InterruptedException) Thread.currentThread().interrupt();
            throw new RuntimeException("Model call failed", e);
        }
    }

    private List<Map<String, Object>> toOpenAiTools() {
        var result = new ArrayList<Map<String, Object>>();
        for (var td : tools) {
            result.add(Map.of(
                    "type", "function",
                    "function", Map.of(
                            "name", td.name(),
                            "description", td.description(),
                            "parameters", td.inputSchema())));
        }
        return result;
    }

    private static Map<String, Object> msg(String role, String content) {
        return Map.of("role", role, "content", content);
    }

    // ── Internal types ────────────────────────────────────────────────────────

    private record ModelMessage(String content, List<ToolCallRef> toolCalls) {
        Map<String, Object> toMap() {
            var m = new LinkedHashMap<String, Object>();
            m.put("role", "assistant");
            m.put("content", content);
            if (toolCalls != null) {
                var tcs = new ArrayList<Map<String, Object>>();
                for (var tc : toolCalls) {
                    tcs.add(Map.of(
                            "id", tc.id(),
                            "type", "function",
                            "function", Map.of(
                                    "name", tc.functionName(),
                                    "arguments", tc.functionArguments() != null ? tc.functionArguments() : "{}")));
                }
                m.put("tool_calls", tcs);
            }
            return m;
        }
    }

    private record ToolCallRef(String id, String functionName, String functionArguments) {}

    // ── Builder ───────────────────────────────────────────────────────────────

    public static final class Builder {

        private final String name;
        private String model = "gpt-4o";
        private String instructions = "";
        private String strategy = "plan-and-execute";
        private int maxIterations = 10;
        private double maxCostUsd = 1.0;
        private int timeoutSeconds = 300;
        private final ToolRegistry registry = new ToolRegistry();

        private Builder(String name) {
            if (name == null || name.isBlank()) throw new IllegalArgumentException("Agent name must not be blank");
            this.name = name;
        }

        public Builder model(String model) {
            this.model = model;
            return this;
        }

        /** Register tool classes in this agent's private registry. */
        public Builder tools(Class<?>... toolClasses) {
            for (var cls : toolClasses) {
                registry.register(cls);
            }
            return this;
        }

        public Builder instructions(String instructions) {
            this.instructions = instructions;
            return this;
        }

        public Builder strategy(String strategy) {
            this.strategy = strategy;
            return this;
        }

        public Builder maxIterations(int maxIterations) {
            this.maxIterations = maxIterations;
            return this;
        }

        public Builder maxCostUsd(double maxCostUsd) {
            this.maxCostUsd = maxCostUsd;
            return this;
        }

        public Builder timeoutSeconds(int timeoutSeconds) {
            this.timeoutSeconds = timeoutSeconds;
            return this;
        }

        public Agent build() {
            return new Agent(this);
        }
    }
}
