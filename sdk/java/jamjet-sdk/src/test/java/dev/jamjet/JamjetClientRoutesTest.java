package dev.jamjet;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.sun.net.httpserver.HttpServer;
import dev.jamjet.client.ClientConfig;
import dev.jamjet.client.JamjetApiException;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import java.io.IOException;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Pins every {@link JamjetClient} call to the exact route the Rust runtime serves
 * (runtime/api/src/routes.rs).
 *
 * <p>The stub answers ONE expected (method, path) pair per call and returns 404 for
 * anything else, so a wrong path fails the call instead of being answered. Every
 * assertion compares the full method, the full raw path (including the query) and
 * the parsed JSON body for equality.
 */
class JamjetClientRoutesTest {

    private static final ObjectMapper MAPPER = new ObjectMapper();

    private record Recorded(String method, String path, String body, String userAgent, String authorization) {}

    private HttpServer server;
    private JamjetClient client;
    private final List<Recorded> requests = new ArrayList<>();

    private volatile String expectMethod;
    private volatile String expectPath;
    private volatile int replyStatus = 200;
    private volatile String replyContentType = "application/json";
    private volatile String replyBody = "{}";

    @BeforeEach
    void start() throws IOException {
        server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/", exchange -> {
            var body = new String(exchange.getRequestBody().readAllBytes(), StandardCharsets.UTF_8);
            var rawPath = exchange.getRequestURI().getRawPath();
            var rawQuery = exchange.getRequestURI().getRawQuery();
            var full = rawQuery == null ? rawPath : rawPath + "?" + rawQuery;
            var method = exchange.getRequestMethod();
            synchronized (requests) {
                requests.add(new Recorded(method, full, body,
                        exchange.getRequestHeaders().getFirst("User-Agent"),
                        exchange.getRequestHeaders().getFirst("Authorization")));
            }
            byte[] out;
            int status;
            String ctype;
            if (method.equals(expectMethod) && full.equals(expectPath)) {
                status = replyStatus;
                ctype = replyContentType;
                out = replyBody.getBytes(StandardCharsets.UTF_8);
            } else {
                status = 404;
                ctype = "application/json";
                out = "{\"error\":\"not found\"}".getBytes(StandardCharsets.UTF_8);
            }
            exchange.getResponseHeaders().add("Content-Type", ctype);
            exchange.sendResponseHeaders(status, out.length == 0 ? -1 : out.length);
            if (out.length > 0) {
                exchange.getResponseBody().write(out);
            }
            exchange.close();
        });
        server.start();
        client = new JamjetClient(ClientConfig.builder()
                .baseUrl("http://127.0.0.1:" + server.getAddress().getPort())
                .apiToken("test-token")
                .timeoutSeconds(5)
                .build());
    }

    @AfterEach
    void stop() {
        server.stop(0);
    }

    private void expect(String method, String path, String reply) {
        expectMethod = method;
        expectPath = path;
        replyBody = reply;
    }

    private Recorded only() {
        synchronized (requests) {
            assertEquals(1, requests.size(), "exactly one request expected, got " + requests);
            return requests.get(0);
        }
    }

    private void assertRequest(String method, String path, Object expectedBody) throws IOException {
        var r = only();
        assertEquals(method, r.method(), "method");
        assertEquals(path, r.path(), "path");
        if (expectedBody == null) {
            assertTrue(r.body().isEmpty(), "expected no body, got " + r.body());
        } else {
            assertEquals(MAPPER.valueToTree(expectedBody), MAPPER.readTree(r.body()), "body");
        }
    }

    // ── Health ───────────────────────────────────────────────────────────────

    @Test
    void health() throws IOException {
        expect("GET", "/health", "{\"status\":\"ok\",\"version\":\"0.5.0\"}");
        assertEquals("ok", client.health().get("status"));
        assertRequest("GET", "/health", null);
    }

    // ── Workflows ────────────────────────────────────────────────────────────

    @Test
    void createWorkflowWrapsIrInIrField() throws IOException {
        Map<String, Object> ir = Map.of("workflow_id", "wf", "version", "1.0.0");
        expect("POST", "/workflows", "{\"workflow_id\":\"wf\",\"version\":\"1.0.0\"}");
        var res = client.createWorkflow(ir);
        assertEquals("wf", res.get("workflow_id"));
        assertRequest("POST", "/workflows", Map.of("ir", ir));
    }

    @Test
    @SuppressWarnings("deprecation")
    void listWorkflowsHasNoRouteAndSendsNothing() {
        var e = assertThrows(UnsupportedOperationException.class, () -> client.listWorkflows());
        assertTrue(e.getMessage().contains("no route"), e.getMessage());
        synchronized (requests) {
            assertEquals(0, requests.size(), "listWorkflows must not call the runtime");
        }
    }

    // ── Executions ───────────────────────────────────────────────────────────

    @Test
    void startExecutionPostsToExecutions() throws IOException {
        Map<String, Object> input = Map.of("q", "hi");
        expect("POST", "/executions", "{\"execution_id\":\"exec_1\"}");
        var res = client.startExecution("wf", input);
        assertEquals("exec_1", res.get("execution_id"));
        assertRequest("POST", "/executions", Map.of("workflow_id", "wf", "input", input));
    }

    @Test
    void startExecutionWithVersion() throws IOException {
        Map<String, Object> input = Map.of("q", "hi");
        expect("POST", "/executions", "{\"execution_id\":\"exec_1\"}");
        client.startExecution("wf", "2.0.0", input);
        assertRequest("POST", "/executions",
                Map.of("workflow_id", "wf", "workflow_version", "2.0.0", "input", input));
    }

    @Test
    void getExecution() throws IOException {
        expect("GET", "/executions/exec_1", "{\"execution_id\":\"exec_1\",\"status\":\"running\"}");
        assertEquals("running", client.getExecution("exec_1").get("status"));
        assertRequest("GET", "/executions/exec_1", null);
    }

    @Test
    void pathSegmentsAreEncoded() throws IOException {
        expect("GET", "/executions/a%2Fb%3Fc", "{}");
        client.getExecution("a/b?c");
        assertRequest("GET", "/executions/a%2Fb%3Fc", null);
    }

    @Test
    void listExecutionsWithStatus() throws IOException {
        expect("GET", "/executions?limit=10&offset=5&status=running", "{\"executions\":[]}");
        client.listExecutions("running", 10, 5);
        assertRequest("GET", "/executions?limit=10&offset=5&status=running", null);
    }

    @Test
    void listExecutionsWithoutStatus() throws IOException {
        expect("GET", "/executions?limit=20&offset=0", "{\"executions\":[]}");
        client.listExecutions(null, 20, 0);
        assertRequest("GET", "/executions?limit=20&offset=0", null);
    }

    @Test
    void cancelExecution() throws IOException {
        expect("POST", "/executions/exec_1/cancel", "{\"execution_id\":\"exec_1\",\"status\":\"cancelled\"}");
        assertEquals("cancelled", client.cancelExecution("exec_1").get("status"));
        assertRequest("POST", "/executions/exec_1/cancel", Map.of());
    }

    @Test
    void getEvents() throws IOException {
        expect("GET", "/executions/exec_1/events", "{\"events\":[]}");
        client.getEvents("exec_1");
        assertRequest("GET", "/executions/exec_1/events", null);
    }

    // ── Human-in-the-loop ────────────────────────────────────────────────────

    @Test
    void getApprovals() throws IOException {
        expect("GET", "/executions/exec_1/approvals", "{\"pending\":[],\"decided\":[]}");
        var res = client.getApprovals("exec_1");
        assertEquals(List.of(), res.get("pending"));
        assertRequest("GET", "/executions/exec_1/approvals", null);
    }

    @Test
    void approveWithNodeAndDigest() throws IOException {
        expect("POST", "/executions/exec_1/approve",
                "{\"execution_id\":\"exec_1\",\"node_id\":\"review\",\"accepted\":true}");
        client.approve("exec_1", "approved", "review", "sha256:abc", "looks right");
        var expected = new LinkedHashMap<String, Object>();
        expected.put("decision", "approved");
        expected.put("node_id", "review");
        expected.put("action_digest", "sha256:abc");
        expected.put("comment", "looks right");
        assertRequest("POST", "/executions/exec_1/approve", expected);
    }

    @Test
    void approveWithStatePatch() throws IOException {
        expect("POST", "/executions/exec_1/approve",
                "{\"execution_id\":\"exec_1\",\"node_id\":\"review\",\"accepted\":true}");
        client.approve("exec_1", "rejected", "no", Map.of("k", "v"));
        assertRequest("POST", "/executions/exec_1/approve",
                Map.of("decision", "rejected", "comment", "no", "state_patch", Map.of("k", "v")));
    }

    @Test
    void sendExternalEventUsesExternalEventRoute() throws IOException {
        expect("POST", "/executions/exec_1/external-event", "{\"execution_id\":\"exec_1\",\"accepted\":true}");
        client.sendExternalEvent("exec_1", "order-42", Map.of("ok", true));
        assertRequest("POST", "/executions/exec_1/external-event",
                Map.of("correlation_key", "order-42", "payload", Map.of("ok", true)));
    }

    // ── Agents and workers ───────────────────────────────────────────────────

    @Test
    void registerAgent() throws IOException {
        Map<String, Object> card = Map.of("name", "a");
        expect("POST", "/agents", "{\"agent_id\":\"ag_1\"}");
        client.registerAgent(card);
        assertRequest("POST", "/agents", card);
    }

    @Test
    void listAgents() throws IOException {
        expect("GET", "/agents", "{\"agents\":[]}");
        client.listAgents();
        assertRequest("GET", "/agents", null);
    }

    @Test
    void getAgent() throws IOException {
        expect("GET", "/agents/ag_1", "{}");
        client.getAgent("ag_1");
        assertRequest("GET", "/agents/ag_1", null);
    }

    @Test
    void activateAgent() throws IOException {
        expect("POST", "/agents/ag_1/activate", "{\"agent_id\":\"ag_1\",\"status\":\"active\"}");
        client.activateAgent("ag_1");
        assertRequest("POST", "/agents/ag_1/activate", Map.of());
    }

    @Test
    void deactivateAgent() throws IOException {
        expect("POST", "/agents/ag_1/deactivate", "{\"agent_id\":\"ag_1\",\"status\":\"deactivated\"}");
        client.deactivateAgent("ag_1");
        assertRequest("POST", "/agents/ag_1/deactivate", Map.of());
    }

    @Test
    void discoverAgent() throws IOException {
        expect("POST", "/agents/discover", "{}");
        client.discoverAgent("https://example.test/agent");
        assertRequest("POST", "/agents/discover", Map.of("url", "https://example.test/agent"));
    }

    @Test
    void listWorkers() throws IOException {
        expect("GET", "/workers", "{\"workers\":[]}");
        client.listWorkers();
        assertRequest("GET", "/workers", null);
    }

    // ── Headers ──────────────────────────────────────────────────────────────

    @Test
    void apiTokenIsSentAsBearer() {
        // The contract IT runs in dev mode, where the runtime ignores this header, so
        // this is the only check that an auth-mode runtime would receive the token.
        expect("GET", "/executions/exec_1", "{}");
        client.getExecution("exec_1");
        assertEquals("Bearer test-token", only().authorization());
    }

    @Test
    void userAgentCarriesTheBuildVersion() {
        var expected = System.getProperty("jamjet.expectedVersion");
        assertTrue(expected != null && !expected.isBlank(), "surefire must pass jamjet.expectedVersion");
        expect("GET", "/health", "{\"status\":\"ok\"}");
        client.health();
        assertEquals("jamjet-java-sdk/" + expected, only().userAgent());
    }

    // ── Failure modes ────────────────────────────────────────────────────────

    @Test
    void unknownPathSurfacesAsApiError() {
        expect("GET", "/nothing-here", "{}");
        var e = assertThrows(JamjetApiException.class, () -> client.getExecution("exec_1"));
        assertEquals(404, e.statusCode());
        assertEquals("/executions/exec_1", e.path());
    }

    @Test
    void runtimeErrorFieldIsInTheMessage404() {
        // The handler's own 404 explains itself; the user must see that text.
        expect("POST", "/executions", "{\"error\":\"workflow research v1.0.0\"}");
        replyStatus = 404;
        var e = assertThrows(JamjetApiException.class, () -> client.startExecution("research", Map.of()));
        assertEquals(404, e.statusCode());
        assertEquals("JamJet API error 404 on /executions: workflow research v1.0.0", e.getMessage());
        assertEquals("{\"error\":\"workflow research v1.0.0\"}", e.body());
    }

    @Test
    void runtimeErrorFieldIsInTheMessage409() {
        expect("POST", "/executions/exec_1/approve",
                "{\"error\":\"approval for node review was already decided\"}");
        replyStatus = 409;
        var e = assertThrows(JamjetApiException.class,
                () -> client.approve("exec_1", "approved", "review", "sha256:abc", null));
        assertEquals(409, e.statusCode());
        assertEquals("JamJet API error 409 on /executions/exec_1/approve: "
                + "approval for node review was already decided", e.getMessage());
    }

    @Test
    void nonJsonErrorBodyIsInTheMessageAndCapped() {
        expect("GET", "/executions/exec_1", "x".repeat(2000));
        replyStatus = 500;
        replyContentType = "text/plain";
        var e = assertThrows(JamjetApiException.class, () -> client.getExecution("exec_1"));
        var prefix = "JamJet API error 500 on /executions/exec_1: ";
        assertTrue(e.getMessage().startsWith(prefix + "xxxx"), e.getMessage());
        assertTrue(e.getMessage().endsWith("... (truncated)"), e.getMessage());
        assertTrue(e.getMessage().length() < prefix.length() + 600, "length " + e.getMessage().length());
        assertEquals(2000, e.body().length(), "the full body stays on body()");
    }

    @Test
    void emptyErrorBodyKeepsTheBareMessage() {
        expect("GET", "/executions/exec_1", "");
        replyStatus = 500;
        var e = assertThrows(JamjetApiException.class, () -> client.getExecution("exec_1"));
        assertEquals("JamJet API error 500 on /executions/exec_1", e.getMessage());
    }

    @Test
    void authFailureCarriesTheRuntimeReason() {
        expect("GET", "/executions/exec_1", "{\"error\":\"token expired\"}");
        replyStatus = 401;
        var e = assertThrows(dev.jamjet.client.JamjetAuthException.class, () -> client.getExecution("exec_1"));
        assertEquals("JamJet authentication failed (401) on /executions/exec_1. Check your API token: token expired",
                e.getMessage());
    }

    @Test
    void htmlReplyIsReportedAsWrongEndpoint() {
        // A runtime older than the JSON 404 fallback answers unknown paths with its
        // web UI's index.html and 200. That must not read as a JSON parse failure.
        expect("GET", "/executions/exec_1", "<!doctype html><title>jamjet</title>");
        replyContentType = "text/html";
        var e = assertThrows(JamjetApiException.class, () -> client.getExecution("exec_1"));
        assertEquals(200, e.statusCode());
        assertTrue(e.getMessage().contains("HTML"), e.getMessage());
    }

    @Test
    void emptyBodyReturnsNull() {
        expect("POST", "/executions/exec_1/cancel", "");
        assertNull(client.cancelExecution("exec_1"));
    }
}
