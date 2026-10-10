package dev.jamjet;

import com.fasterxml.jackson.databind.ObjectMapper;
import dev.jamjet.client.ClientConfig;
import dev.jamjet.client.JamjetApiException;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.util.List;
import java.util.Map;
import java.util.UUID;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Contract test against a REAL JamJet runtime in dev mode.
 *
 * <p>Not part of the unit run: surefire skips {@code *IT} classes. Run it with
 * {@code mvn -P runtime-contract verify -Djamjet.runtime.url=http://127.0.0.1:7700}
 * after starting {@code JAMJET_DEV_MODE=true jamjet-server}. With the profile on,
 * a missing or unreachable runtime FAILS the build rather than skipping.
 *
 * <p>Every step asserts a side effect the runtime reports back (the stored
 * workflow ID, the execution's status, the events it wrote), never only the
 * absence of an exception.
 *
 * <p>Auth is not covered here. In dev mode the runtime skips {@code require_auth}, and
 * it has no route or flag that issues a token ({@code create_token} is reachable only
 * from the state backend), so this run never presents a token for checking. The
 * unit test {@code JamjetClientRoutesTest.apiTokenIsSentAsBearer} pins the header the
 * client sends.
 */
class RuntimeContractIT {

    private static final ObjectMapper MAPPER = new ObjectMapper();

    private static String baseUrl;
    private static JamjetClient client;

    @BeforeAll
    static void connect() {
        baseUrl = System.getProperty("jamjet.runtime.url");
        assertNotNull(baseUrl, "set -Djamjet.runtime.url to a running dev-mode runtime");
        client = new JamjetClient(ClientConfig.builder().baseUrl(baseUrl).timeoutSeconds(10).build());
        assertEquals("ok", client.health().get("status"), "runtime must be healthy at " + baseUrl);
    }

    @AfterAll
    static void close() {
        if (client != null) client.close();
    }

    /**
     * A minimal valid workflow: one {@code python_fn} node. Its work item goes to the
     * {@code python_tool} queue, which only an external worker claims, so the
     * execution stays live long enough to read and cancel.
     */
    private static Map<String, Object> minimalIr(String workflowId) {
        return Map.ofEntries(
                Map.entry("workflow_id", workflowId),
                Map.entry("version", "1.0.0"),
                Map.entry("state_schema", "{}"),
                Map.entry("start_node", "step"),
                Map.entry("nodes", Map.of("step", Map.of(
                        "id", "step",
                        "kind", Map.of(
                                "type", "python_fn",
                                "module", "contract_test",
                                "function", "noop",
                                "output_schema", "")))),
                Map.entry("edges", List.of()),
                Map.entry("retry_policies", Map.of()),
                Map.entry("models", Map.of()),
                Map.entry("tools", Map.of()),
                Map.entry("mcp_servers", Map.of()),
                Map.entry("remote_agents", Map.of()));
    }

    /**
     * The runtime spells one execution ID two ways: {@code POST /executions} returns
     * {@code exec_<32 hex>}, while response bodies carry the hyphenated UUID. Both are
     * accepted as path input. Compare on the UUID's hex digits.
     */
    private static String canonical(Object executionId) {
        assertNotNull(executionId);
        return executionId.toString().replaceFirst("^exec_", "").replace("-", "");
    }

    @SuppressWarnings("unchecked")
    private static List<Map<String, Object>> events(String executionId) {
        var res = client.getEvents(executionId);
        return (List<Map<String, Object>>) res.get("events");
    }

    @SuppressWarnings("unchecked")
    private static List<String> eventTypes(String executionId) {
        return events(executionId).stream()
                .map(e -> (String) ((Map<String, Object>) e.get("kind")).get("type"))
                .toList();
    }

    @Test
    @SuppressWarnings("unchecked")
    void registerStartReadSignalAndCancel() {
        var workflowId = "java-contract-" + UUID.randomUUID();

        // Register: the runtime echoes what it stored.
        var created = client.createWorkflow(minimalIr(workflowId));
        assertEquals(workflowId, created.get("workflow_id"));
        assertEquals("1.0.0", created.get("version"));

        // Start, then read it back.
        var started = client.startExecution(workflowId, "1.0.0", Map.of("question", "contract"));
        var executionId = (String) started.get("execution_id");
        assertNotNull(executionId, "start must return an execution_id: " + started);

        var exec = client.getExecution(executionId);
        assertEquals(canonical(executionId), canonical(exec.get("execution_id")));
        assertEquals(workflowId, exec.get("workflow_id"));
        assertEquals("1.0.0", exec.get("workflow_version"));
        assertEquals(Map.of("question", "contract"), exec.get("initial_input"));
        var status = (String) exec.get("status");
        assertTrue(List.of("pending", "running", "paused").contains(status),
                "a fresh execution must not be terminal, got " + status);

        // Its event log exists and belongs to it.
        var log = events(executionId);
        assertFalse(log.isEmpty(), "a started execution must have events");
        for (var e : log) {
            assertEquals(canonical(executionId), canonical(e.get("execution_id")));
        }
        assertTrue(eventTypes(executionId).contains("workflow_started"), eventTypes(executionId).toString());

        // It is listed.
        var listed = (List<Map<String, Object>>) client.listExecutions(null, 200, 0).get("executions");
        assertTrue(listed.stream().anyMatch(e -> canonical(executionId).equals(canonical(e.get("execution_id")))),
                "listExecutions must include " + executionId);

        // No approval is pending on a workflow with no approval node.
        var approvals = client.getApprovals(executionId);
        assertEquals(List.of(), approvals.get("pending"));

        // An external event lands in the log.
        var ack = client.sendExternalEvent(executionId, "contract-key", Map.of("ok", true));
        assertEquals(Boolean.TRUE, ack.get("accepted"));
        assertTrue(eventTypes(executionId).contains("external_event_received"), eventTypes(executionId).toString());

        // Cancel, and the runtime agrees it is cancelled.
        var cancelled = client.cancelExecution(executionId);
        assertEquals("cancelled", cancelled.get("status"));
        assertEquals("cancelled", client.getExecution(executionId).get("status"));
        assertTrue(eventTypes(executionId).contains("workflow_cancelled"), eventTypes(executionId).toString());

        // A second cancel is refused: the first one really changed state.
        var again = assertThrows(JamjetApiException.class, () -> client.cancelExecution(executionId));
        assertEquals(400, again.statusCode());
    }

    @Test
    void invalidIrIsRefusedByTheHandler() {
        // A 400 from create_workflow proves the {"ir": ...} envelope reached the
        // handler: a raw IR body would be rejected earlier with 422.
        var e = assertThrows(JamjetApiException.class,
                () -> client.createWorkflow(Map.of("workflow_id", "x", "version", "1.0.0")));
        assertEquals(400, e.statusCode(), e.body());
        assertTrue(e.body().contains("invalid workflow IR"), e.body());
    }

    @Test
    void startingAnUnknownWorkflowIs404() {
        var missing = "no-such-workflow-" + UUID.randomUUID();
        var e = assertThrows(JamjetApiException.class, () -> client.startExecution(missing, Map.of()));
        assertEquals(404, e.statusCode(), e.body());
        // The handler's own 404 names the workflow; a route miss would not.
        assertTrue(e.body().contains(missing), e.body());
        // And the user sees that reason in the message, not only on body().
        assertTrue(e.getMessage().contains(missing), e.getMessage());
    }

    @Test
    void oldApiV1PathIsAJson404() throws Exception {
        // What the published 0.5.0 SDK sends. The runtime must refuse it loudly.
        var http = HttpClient.newHttpClient();
        var res = http.send(HttpRequest.newBuilder(URI.create(baseUrl + "/api/v1/workflows"))
                        .header("Content-Type", "application/json")
                        .POST(HttpRequest.BodyPublishers.ofString("{}"))
                        .build(),
                HttpResponse.BodyHandlers.ofString());
        assertEquals(404, res.statusCode(), res.body());
        assertTrue(res.headers().firstValue("Content-Type").orElse("").startsWith("application/json"));
        var body = MAPPER.readTree(res.body());
        assertEquals("not found", body.get("error").asText());
        assertTrue(body.get("hint").asText().contains("/executions"), res.body());
    }
}
