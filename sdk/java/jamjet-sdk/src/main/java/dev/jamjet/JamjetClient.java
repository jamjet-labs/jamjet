package dev.jamjet;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.ObjectMapper;
import dev.jamjet.client.ClientConfig;
import dev.jamjet.client.JamjetApiException;
import dev.jamjet.client.JamjetAuthException;
import dev.jamjet.client.JamjetTimeoutException;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.io.IOException;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.URLEncoder;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Executors;

/**
 * Blocking HTTP client for the JamJet runtime REST API.
 *
 * <p>All methods are virtual-thread-friendly — use them freely from platform threads,
 * virtual threads, or structured concurrency scopes. Internally uses
 * {@link java.net.http.HttpClient} with a virtual-thread-per-task executor.
 *
 * <pre>{@code
 * var client = new JamjetClient(ClientConfig.defaults());
 * var health = client.health();
 * var exec   = client.startExecution("wf-123", Map.of("prompt", "Hello"));
 * }</pre>
 */
public final class JamjetClient implements AutoCloseable {

    private static final Logger log = LoggerFactory.getLogger(JamjetClient.class);

    private final ClientConfig config;
    private final HttpClient http;
    private final ObjectMapper mapper;

    public JamjetClient(ClientConfig config) {
        this.config = config;
        this.mapper = new ObjectMapper()
                .enable(DeserializationFeature.FAIL_ON_TRAILING_TOKENS)
                .disable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES);
        this.http = HttpClient.newBuilder()
                .executor(Executors.newVirtualThreadPerTaskExecutor())
                .connectTimeout(Duration.ofSeconds(config.timeoutSeconds()))
                .build();
    }

    /** Construct with default config (localhost:7700, reads JAMJET_TOKEN from env). */
    public JamjetClient() {
        this(ClientConfig.defaults());
    }

    // ── Health ────────────────────────────────────────────────────────────────

    /** Check runtime health. Returns the health status map. */
    public Map<String, Object> health() {
        return get("/health");
    }

    // ── Workflows ─────────────────────────────────────────────────────────────

    /**
     * Register a workflow definition from its canonical IR.
     *
     * <p>Sends {@code POST /workflows} with body {@code {"ir": ir}}. The IR must carry
     * {@code workflow_id} and {@code version}; the runtime answers 201 with
     * {@code {"workflow_id", "version"}} and 400 for IR it cannot load.
     */
    public Map<String, Object> createWorkflow(Map<String, Object> ir) {
        var body = new LinkedHashMap<String, Object>();
        body.put("ir", ir);
        return post("/workflows", body);
    }

    /**
     * Not supported: the runtime has no route that lists workflow definitions.
     *
     * @throws UnsupportedOperationException always, without contacting the runtime
     * @deprecated the runtime has no workflow list route. Keep the IDs you register
     *             with {@link #createWorkflow(Map)}, or list executions with
     *             {@link #listExecutions(String, int, int)}.
     */
    @Deprecated
    public List<Map<String, Object>> listWorkflows() {
        throw new UnsupportedOperationException(
                "the JamJet runtime has no route that lists workflows (GET /workflows is not served); "
                        + "keep the workflow IDs you register, or use listExecutions()");
    }

    // ── Executions ────────────────────────────────────────────────────────────

    /**
     * Start an execution without naming a version, so the runtime runs version
     * {@code 1.0.0}.
     *
     * <p>A workflow built with {@link dev.jamjet.workflow.Workflow}'s builder defaults to
     * version {@code 0.1.0}, so this overload answers 404 for it unless the builder set
     * {@code 1.0.0}. Use {@link #startExecution(String, String, Map)} with
     * {@code ir.version()} for those.
     *
     * <p>Sends {@code POST /executions} with {@code {"workflow_id", "input"}}; the
     * runtime answers 201 with {@code {"execution_id"}}.
     */
    public Map<String, Object> startExecution(String workflowId, Map<String, Object> input) {
        return startExecution(workflowId, null, input);
    }

    /**
     * Start an execution of a specific workflow version.
     *
     * @param workflowId       the registered workflow ID
     * @param workflowVersion  the version to run, such as {@code ir.version()}, or {@code null}
     *                         for the runtime default ({@code 1.0.0})
     * @param input            the initial input
     */
    public Map<String, Object> startExecution(
            String workflowId, String workflowVersion, Map<String, Object> input) {
        var body = new LinkedHashMap<String, Object>();
        body.put("workflow_id", workflowId);
        if (workflowVersion != null) body.put("workflow_version", workflowVersion);
        body.put("input", input == null ? Map.of() : input);
        return post("/executions", body);
    }

    /** Get an execution by ID ({@code GET /executions/:id}). */
    public Map<String, Object> getExecution(String executionId) {
        return get("/executions/" + seg(executionId));
    }

    /**
     * List executions, optionally filtered by status ({@code GET /executions}).
     * The response is {@code {"executions": [...]}}.
     */
    public Map<String, Object> listExecutions(String status, int limit, int offset) {
        var query = "?limit=" + limit + "&offset=" + offset;
        if (status != null && !status.isBlank()) {
            query += "&status=" + URLEncoder.encode(status, StandardCharsets.UTF_8);
        }
        return get("/executions" + query);
    }

    /** Cancel a running execution ({@code POST /executions/:id/cancel}). */
    public Map<String, Object> cancelExecution(String executionId) {
        return post("/executions/" + seg(executionId) + "/cancel", Map.of());
    }

    /**
     * Get the event log for an execution ({@code GET /executions/:id/events}).
     * The response is {@code {"events": [...]}}.
     */
    public Map<String, Object> getEvents(String executionId) {
        return get("/executions/" + seg(executionId) + "/events");
    }

    // ── Human-in-the-Loop ─────────────────────────────────────────────────────

    /**
     * List the approvals of an execution ({@code GET /executions/:id/approvals}).
     *
     * <p>The response is {@code {"pending": [...], "decided": [...]}}. Each pending
     * entry carries {@code node_id} and the {@code context} the reviewer is deciding
     * on; pass that entry's {@code node_id} and {@code context.action_digest} to
     * {@link #approve(String, String, String, String, String)}.
     */
    public Map<String, Object> getApprovals(String executionId) {
        return get("/executions/" + seg(executionId) + "/approvals");
    }

    /**
     * Approve or reject a pending approval, naming the node and the action reviewed.
     *
     * <p>Read the pending request with {@link #getApprovals(String)} first and echo its
     * {@code node_id} and {@code context.action_digest}. Runtimes with approval
     * integrity refuse an {@code approved} decision without a matching digest outside
     * dev mode; older runtimes ignore the field.
     *
     * @param executionId   the execution ID
     * @param decision      "approved" or "rejected"
     * @param nodeId        the node the decision is for; may be {@code null} when only
     *                      one approval is pending
     * @param actionDigest  the reviewed request's {@code action_digest} (or
     *                      {@code calls_hash} for an agent tool dispatch); may be
     *                      {@code null} for a rejection
     * @param comment       optional human comment
     */
    public Map<String, Object> approve(
            String executionId,
            String decision,
            String nodeId,
            String actionDigest,
            String comment) {
        var body = new LinkedHashMap<String, Object>();
        body.put("decision", decision);
        if (nodeId != null) body.put("node_id", nodeId);
        if (actionDigest != null) body.put("action_digest", actionDigest);
        if (comment != null) body.put("comment", comment);
        return post("/executions/" + seg(executionId) + "/approve", body);
    }

    /**
     * Approve or reject the single pending approval of an execution
     * ({@code POST /executions/:id/approve}).
     *
     * @param executionId   the execution ID
     * @param decision      "approved" or "rejected"
     * @param comment       optional human comment
     * @param statePatch    optional state patch to apply
     */
    public Map<String, Object> approve(
            String executionId,
            String decision,
            String comment,
            Map<String, Object> statePatch) {
        var body = new LinkedHashMap<String, Object>();
        body.put("decision", decision);
        if (comment != null) body.put("comment", comment);
        if (statePatch != null && !statePatch.isEmpty()) body.put("state_patch", statePatch);
        return post("/executions/" + seg(executionId) + "/approve", body);
    }

    /**
     * Deliver an external event to an execution waiting on a correlation key
     * ({@code POST /executions/:id/external-event}).
     */
    public Map<String, Object> sendExternalEvent(
            String executionId,
            String correlationKey,
            Map<String, Object> payload) {
        var body = new LinkedHashMap<String, Object>();
        body.put("correlation_key", correlationKey);
        body.put("payload", payload);
        return post("/executions/" + seg(executionId) + "/external-event", body);
    }

    // ── Agents ────────────────────────────────────────────────────────────────

    /** Register an agent card ({@code POST /agents}). */
    public Map<String, Object> registerAgent(Map<String, Object> card) {
        return post("/agents", card);
    }

    /** List registered agents ({@code GET /agents}); the response is {@code {"agents": [...]}}. */
    public Map<String, Object> listAgents() {
        return get("/agents");
    }

    /** Get an agent by ID ({@code GET /agents/:id}). */
    public Map<String, Object> getAgent(String agentId) {
        return get("/agents/" + seg(agentId));
    }

    /** Activate an agent ({@code POST /agents/:id/activate}). */
    public Map<String, Object> activateAgent(String agentId) {
        return post("/agents/" + seg(agentId) + "/activate", Map.of());
    }

    /** Deactivate an agent ({@code POST /agents/:id/deactivate}). */
    public Map<String, Object> deactivateAgent(String agentId) {
        return post("/agents/" + seg(agentId) + "/deactivate", Map.of());
    }

    /** Fetch a remote Agent Card from a URL and register it ({@code POST /agents/discover}). */
    public Map<String, Object> discoverAgent(String url) {
        return post("/agents/discover", Map.of("url", url));
    }

    // ── Workers ───────────────────────────────────────────────────────────────

    /** List connected workers ({@code GET /workers}); the response is {@code {"workers": [...]}}. */
    public Map<String, Object> listWorkers() {
        return get("/workers");
    }

    // ── Internal HTTP helpers ─────────────────────────────────────────────────

    @SuppressWarnings("unchecked")
    private Map<String, Object> get(String path) {
        var req = buildRequest(path).GET().build();
        return (Map<String, Object>) send(req, path, new TypeReference<Map<String, Object>>() {});
    }

    /** Percent-encode one path segment so an ID can never change the route. */
    private static String seg(String value) {
        if (value == null || value.isEmpty()) {
            throw new IllegalArgumentException("path segment must not be empty");
        }
        return URLEncoder.encode(value, StandardCharsets.UTF_8).replace("+", "%20");
    }

    @SuppressWarnings("unchecked")
    private Map<String, Object> post(String path, Map<String, Object> body) {
        try {
            var json = mapper.writeValueAsBytes(body);
            var req = buildRequest(path)
                    .POST(HttpRequest.BodyPublishers.ofByteArray(json))
                    .header("Content-Type", "application/json")
                    .build();
            return (Map<String, Object>) send(req, path, new TypeReference<Map<String, Object>>() {});
        } catch (IOException e) {
            throw new RuntimeException("Failed to serialize request body for " + path, e);
        }
    }

    private HttpRequest.Builder buildRequest(String path) {
        var uri = URI.create(config.baseUrl() + path);
        var builder = HttpRequest.newBuilder(uri)
                .timeout(Duration.ofSeconds(config.timeoutSeconds()))
                .header("Accept", "application/json")
                .header("User-Agent", "jamjet-java-sdk/" + JamjetVersion.get());
        if (config.apiToken() != null && !config.apiToken().isBlank()) {
            builder.header("Authorization", "Bearer " + config.apiToken());
        }
        return builder;
    }

    private <T> T send(HttpRequest request, String path, TypeReference<T> typeRef) {
        try {
            log.debug("→ {} {}", request.method(), path);
            var response = http.send(request, HttpResponse.BodyHandlers.ofString());
            var statusCode = response.statusCode();
            var body = response.body();
            log.debug("← {} {} ({})", statusCode, path, body.length() + " bytes");

            if (statusCode == 401 || statusCode == 403) {
                throw new JamjetAuthException(statusCode, body, path);
            }
            if (statusCode < 200 || statusCode >= 300) {
                throw new JamjetApiException(statusCode, body, path);
            }

            if (body == null || body.isBlank()) {
                return null;
            }
            var contentType = response.headers().firstValue("Content-Type").orElse("");
            if (contentType.startsWith("text/html")) {
                // Runtimes before 0.5.1 answer any unknown path with their web UI's
                // index.html and 200. Say so instead of failing to parse it as JSON.
                throw new JamjetApiException(statusCode, body, path,
                        "JamJet runtime answered " + path + " with an HTML page, not JSON: "
                                + "the base URL or path is wrong (the runtime serves bare routes such as /executions)");
            }
            return mapper.readValue(body, typeRef);
        } catch (JamjetAuthException | JamjetApiException e) {
            throw e;
        } catch (java.net.http.HttpTimeoutException e) {
            throw new JamjetTimeoutException(path, e);
        } catch (IOException | InterruptedException e) {
            if (e instanceof InterruptedException) Thread.currentThread().interrupt();
            throw new RuntimeException("HTTP request failed for " + path, e);
        }
    }

    @Override
    public void close() {
        // HttpClient is managed by the JVM GC; executor is virtual-thread-per-task (no pool to shut down)
    }
}
