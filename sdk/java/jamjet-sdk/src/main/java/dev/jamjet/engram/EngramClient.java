package dev.jamjet.engram;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.ObjectMapper;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.io.IOException;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.time.Duration;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Executors;

/**
 * Blocking HTTP client for the Engram memory REST API.
 *
 * <p>All methods are virtual-thread-friendly. Internally uses
 * {@link java.net.http.HttpClient} with a virtual-thread-per-task executor.
 *
 * <pre>{@code
 * try (var client = new EngramClient()) {
 *     client.add(List.of(Map.of("role", "user", "content", "I like pizza")), "alice", null, null);
 *     var facts = client.recall("pizza", "alice", null, 10);
 *     var ctx = client.context("recommend food", "alice", null, 2000, "system_prompt");
 *     System.out.println(ctx.get("text"));
 * }
 * }</pre>
 */
public final class EngramClient implements AutoCloseable {

    private static final Logger log = LoggerFactory.getLogger(EngramClient.class);

    private final EngramConfig config;
    private final HttpClient http;
    private final ObjectMapper mapper;

    public EngramClient(EngramConfig config) {
        this.config = config;
        this.mapper = new ObjectMapper()
                .enable(DeserializationFeature.FAIL_ON_TRAILING_TOKENS)
                .disable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES);
        this.http = HttpClient.newBuilder()
                .executor(Executors.newVirtualThreadPerTaskExecutor())
                .connectTimeout(Duration.ofSeconds(config.timeoutSeconds()))
                .build();
    }

    /** Construct with default config (localhost:9090, reads ENGRAM_TOKEN from env). */
    public EngramClient() {
        this(EngramConfig.defaults());
    }

    // ── Health ────────────────────────────────────────────────────────────────

    /** Check server health. */
    public Map<String, Object> health() {
        return get("/health");
    }

    // ── Add ───────────────────────────────────────────────────────────────────

    /** Extract and store facts from conversation messages. */
    public Map<String, Object> add(
            List<Map<String, String>> messages,
            String userId,
            String orgId,
            String sessionId) {
        var body = new HashMap<String, Object>();
        body.put("messages", messages);
        if (userId != null) body.put("user_id", userId);
        if (orgId != null) body.put("org_id", orgId);
        if (sessionId != null) body.put("session_id", sessionId);
        return post("/v1/memory", body);
    }

    // ── Recall ────────────────────────────────────────────────────────────────

    /** Semantic search over stored facts. */
    @SuppressWarnings("unchecked")
    public List<Map<String, Object>> recall(String query, String userId, String orgId, int maxResults) {
        var params = new StringBuilder("?q=").append(encode(query));
        if (userId != null) params.append("&user_id=").append(encode(userId));
        if (orgId != null) params.append("&org_id=").append(encode(orgId));
        params.append("&max_results=").append(maxResults);
        var result = get("/v1/memory/recall" + params);
        return (List<Map<String, Object>>) result.getOrDefault("results", List.of());
    }

    // ── Context ───────────────────────────────────────────────────────────────

    /** Assemble a token-budgeted context block for LLM prompts. */
    public Map<String, Object> context(
            String query, String userId, String orgId, int tokenBudget, String format) {
        var body = new HashMap<String, Object>();
        body.put("query", query);
        body.put("token_budget", tokenBudget);
        body.put("format", format != null ? format : "system_prompt");
        if (userId != null) body.put("user_id", userId);
        if (orgId != null) body.put("org_id", orgId);
        return post("/v1/memory/context", body);
    }

    // ── Forget ────────────────────────────────────────────────────────────────

    /** Soft-delete a fact by ID. */
    public Map<String, Object> forget(String factId, String reason) {
        var body = new HashMap<String, Object>();
        if (reason != null) body.put("reason", reason);
        return delete("/v1/memory/facts/" + factId, body.isEmpty() ? null : body);
    }

    // ── Search ────────────────────────────────────────────────────────────────

    /** Keyword search over stored facts (FTS5). */
    @SuppressWarnings("unchecked")
    public List<Map<String, Object>> search(String query, String userId, String orgId, int topK) {
        var params = new StringBuilder("?q=").append(encode(query));
        if (userId != null) params.append("&user_id=").append(encode(userId));
        if (orgId != null) params.append("&org_id=").append(encode(orgId));
        params.append("&top_k=").append(topK);
        var result = get("/v1/memory/search" + params);
        return (List<Map<String, Object>>) result.getOrDefault("results", List.of());
    }

    // ── Stats ─────────────────────────────────────────────────────────────────

    /** Return aggregate memory statistics. */
    public Map<String, Object> stats() {
        return get("/v1/memory/stats");
    }

    // ── Consolidate ───────────────────────────────────────────────────────────

    /** Run a consolidation cycle. */
    public Map<String, Object> consolidate(String userId, String orgId) {
        var body = new HashMap<String, Object>();
        if (userId != null) body.put("user_id", userId);
        if (orgId != null) body.put("org_id", orgId);
        return post("/v1/memory/consolidate", body);
    }

    // ── Delete user ───────────────────────────────────────────────────────────

    /** GDPR: delete all memory data for a user. */
    public Map<String, Object> deleteUser(String userId) {
        return delete("/v1/memory/users/" + userId, null);
    }

    // ── Internal HTTP helpers ─────────────────────────────────────────────────

    @SuppressWarnings("unchecked")
    private Map<String, Object> get(String path) {
        var req = buildRequest(path).GET().build();
        return (Map<String, Object>) send(req, path, new TypeReference<Map<String, Object>>() {});
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

    @SuppressWarnings("unchecked")
    private Map<String, Object> delete(String path, Map<String, Object> body) {
        try {
            HttpRequest req;
            if (body != null) {
                var json = mapper.writeValueAsBytes(body);
                req = buildRequest(path)
                        .method("DELETE", HttpRequest.BodyPublishers.ofByteArray(json))
                        .header("Content-Type", "application/json")
                        .build();
            } else {
                req = buildRequest(path).DELETE().build();
            }
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
                .header("User-Agent", "engram-java-sdk/0.4.0");
        if (config.apiToken() != null && !config.apiToken().isBlank()) {
            builder.header("Authorization", "Bearer " + config.apiToken());
        }
        return builder;
    }

    private Object send(HttpRequest req, String path, TypeReference<?> type) {
        try {
            log.debug("{} {}", req.method(), path);
            var resp = http.send(req, HttpResponse.BodyHandlers.ofString());
            if (resp.statusCode() == 401 || resp.statusCode() == 403) {
                throw new RuntimeException("Authentication failed for " + path + " (HTTP " + resp.statusCode() + ")");
            }
            if (resp.statusCode() >= 400) {
                throw new RuntimeException("Engram API error: HTTP " + resp.statusCode() + " for " + path + ": " + resp.body());
            }
            return mapper.readValue(resp.body(), type);
        } catch (IOException | InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new RuntimeException("HTTP error for " + path, e);
        }
    }

    private static String encode(String value) {
        return java.net.URLEncoder.encode(value, java.nio.charset.StandardCharsets.UTF_8);
    }

    @Override
    public void close() {
        // HttpClient has no close in JDK 11-17; in 21+ it's AutoCloseable
        // but we don't close it since we don't own the virtual-thread executor lifecycle
    }
}
