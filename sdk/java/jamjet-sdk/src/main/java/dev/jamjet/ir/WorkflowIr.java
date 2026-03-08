package dev.jamjet.ir;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.ObjectMapper;

import java.io.IOException;
import java.util.List;
import java.util.Map;

/**
 * Canonical JamJet Intermediate Representation (IR) for a workflow or agent.
 *
 * <p>Field names use snake_case (via {@link JsonProperty}) to match the Python SDK output
 * byte-for-byte. The IR is submitted to the runtime via {@link dev.jamjet.JamjetClient#createWorkflow}.
 */
@JsonIgnoreProperties(ignoreUnknown = true)
@JsonInclude(JsonInclude.Include.NON_NULL)
public record WorkflowIr(
        @JsonProperty("workflow_id") String id,
        @JsonProperty("version") String version,
        @JsonProperty("name") String name,
        @JsonProperty("description") String description,
        @JsonProperty("state_schema") String stateSchema,
        @JsonProperty("start_node") String startNode,
        @JsonProperty("nodes") Map<String, Object> nodes,
        @JsonProperty("edges") List<Map<String, Object>> edges,
        @JsonProperty("retry_policies") Map<String, Object> retryPolicies,
        @JsonProperty("timeouts") Map<String, Object> timeouts,
        @JsonProperty("models") Map<String, Object> models,
        @JsonProperty("tools") Map<String, Object> tools,
        @JsonProperty("mcp_servers") Map<String, Object> mcpServers,
        @JsonProperty("remote_agents") Map<String, Object> remoteAgents,
        @JsonProperty("labels") Map<String, Object> labels,
        @JsonProperty("strategy_metadata") Map<String, Object> strategyMetadata) {

    private static final ObjectMapper MAPPER = new ObjectMapper();

    /**
     * Serialize this IR to a JSON string.
     *
     * @return canonical JSON representation
     */
    public String toJson() {
        try {
            return MAPPER.writeValueAsString(this);
        } catch (IOException e) {
            throw new RuntimeException("Failed to serialize WorkflowIr to JSON", e);
        }
    }

    /**
     * Deserialize a {@link WorkflowIr} from a JSON string.
     *
     * @param json the JSON string
     * @return the deserialized IR
     */
    public static WorkflowIr fromJson(String json) {
        try {
            return MAPPER.readValue(json, WorkflowIr.class);
        } catch (IOException e) {
            throw new RuntimeException("Failed to deserialize WorkflowIr from JSON", e);
        }
    }

    /**
     * Convert to a plain {@code Map<String, Object>} for submission to the API.
     */
    @SuppressWarnings("unchecked")
    public Map<String, Object> toMap() {
        return MAPPER.convertValue(this, Map.class);
    }
}
