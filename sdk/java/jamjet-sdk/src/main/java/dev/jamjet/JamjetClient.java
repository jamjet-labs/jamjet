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
import java.net.http.HttpResponse;
import java.time.Duration;
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

    /** Create a workflow from a canonical IR map. */
    public Map<String, Object> createWorkflow(Map<String, Object> ir) {
        return post("/api/v1/workflows", ir);
    }

    /** List all registered workflows. */
    public List<Map<String, Object>> listWorkflows() {
        return getList("/api/v1/workflows");
    }

    // ── Executions ────────────────────────────────────────────────────────────

    /** Start a new workflow execution. */
    public Map<String, Object> startExecution(String workflowId, Map<String, Object> input) {
        return post("/api/v1/workflows/" + workflowId + "/execute", input);
    }

    /** Get execution status by ID. */
    public Map<String, Object> getExecution(String executionId) {
        return get("/api/v1/executions/" + executionId);
    }

    /** List executions, optionally filtered by status. */
    public Map<String, Object> listExecutions(String status, int limit, int offset) {
        var query = "?limit=" + limit + "&offset=" + offset;
        if (status != null && !status.isBlank()) {
            query += "&status=" + status;
        }
        return get("/api/v1/executions" + query);
    }

    /** Cancel a running execution. */
    public Map<String, Object> cancelExecution(String executionId) {
        return post("/api/v1/executions/" + executionId + "/cancel", Map.of());
    }

    /** Get the event stream for an execution. */
    public Map<String, Object> getEvents(String executionId) {
        return get("/api/v1/executions/" + executionId + "/events");
    }

    // ── Human-in-the-Loop ─────────────────────────────────────────────────────

    /**
     * Approve or reject a paused human-approval step.
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
        var body = new java.util.LinkedHashMap<String, Object>();
        body.put("decision", decision);
        if (comment != null) body.put("comment", comment);
        if (statePatch != null && !statePatch.isEmpty()) body.put("state_patch", statePatch);
        return post("/api/v1/executions/" + executionId + "/approve", body);
    }

    /** Send an external event to a waiting correlation key. */
    public Map<String, Object> sendExternalEvent(
            String executionId,
            String correlationKey,
            Map<String, Object> payload) {
        var body = Map.of("correlation_key", correlationKey, "payload", payload);
        return post("/api/v1/executions/" + executionId + "/events", body);
    }

    // ── Agents ────────────────────────────────────────────────────────────────

    /** Register an agent card. */
    public Map<String, Object> registerAgent(Map<String, Object> card) {
        return post("/api/v1/agents", card);
    }

    /** List all registered agents. */
    public Map<String, Object> listAgents() {
        return get("/api/v1/agents");
    }

    /** Get agent by ID. */
    public Map<String, Object> getAgent(String agentId) {
        return get("/api/v1/agents/" + agentId);
    }

    /** Activate an agent. */
    public Map<String, Object> activateAgent(String agentId) {
        return post("/api/v1/agents/" + agentId + "/activate", Map.of());
    }

    /** Deactivate an agent. */
    public Map<String, Object> deactivateAgent(String agentId) {
        return post("/api/v1/agents/" + agentId + "/deactivate", Map.of());
    }

    /** Discover and import a remote agent card from a URL. */
    public Map<String, Object> discoverAgent(String url) {
        return post("/api/v1/agents/discover", Map.of("url", url));
    }

    // ── Workers ───────────────────────────────────────────────────────────────

    /** List all connected workers. */
    public Map<String, Object> listWorkers() {
        return get("/api/v1/workers");
    }

    // ── Internal HTTP helpers ─────────────────────────────────────────────────

    @SuppressWarnings("unchecked")
    private Map<String, Object> get(String path) {
        var req = buildRequest(path).GET().build();
        return (Map<String, Object>) send(req, path, new TypeReference<Map<String, Object>>() {});
    }

    @SuppressWarnings("unchecked")
    private List<Map<String, Object>> getList(String path) {
        var req = buildRequest(path).GET().build();
        return (List<Map<String, Object>>) send(req, path, new TypeReference<List<Map<String, Object>>>() {});
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
                .header("User-Agent", "jamjet-java-sdk/0.1.0");
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
