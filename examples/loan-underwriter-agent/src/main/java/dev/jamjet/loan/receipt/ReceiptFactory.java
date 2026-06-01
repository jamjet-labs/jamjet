package dev.jamjet.loan.receipt;

import dev.jamjet.cloud.agentboundary.ActionReceipt;
import dev.jamjet.cloud.agentboundary.Actor;
import dev.jamjet.cloud.agentboundary.ActorType;
import dev.jamjet.cloud.agentboundary.Agent;
import dev.jamjet.cloud.agentboundary.Approval;
import dev.jamjet.cloud.agentboundary.Environment;
import dev.jamjet.cloud.agentboundary.Execution;
import dev.jamjet.cloud.agentboundary.ExecutionStatus;
import dev.jamjet.cloud.agentboundary.Policy;
import dev.jamjet.cloud.agentboundary.PolicyDecision;
import dev.jamjet.cloud.agentboundary.ReceiptHashes;
import dev.jamjet.cloud.agentboundary.Target;
import dev.jamjet.cloud.agentboundary.Tool;

import java.nio.charset.StandardCharsets;
import java.time.Instant;
import java.util.UUID;

/**
 * Builds fully-valid AgentBoundary v0.1 {@link ActionReceipt}s for the loan
 * underwriter's tool calls.
 *
 * <p>Construction mirrors
 * {@code dev.jamjet.cloud.spring.ActionReceiptAdvisor#after(..)}: every
 * sub-record the advisor sets is set here, the {@code arguments_hash} and
 * {@code receipt_hash} are computed via {@link ReceiptHashes}, and the receipt
 * is built with {@code policy.decision = ALLOW} and {@code execution.status = SUCCESS}.
 *
 * <p>Two deliberate differences from the advisor, both within the spec:
 * <ul>
 *   <li><b>applicationId carrier:</b> we put the loan {@code applicationId} into
 *       {@link Target#resourceId()} ({@code target.resource_id}). The advisor passes
 *       {@code null} there; the spec defines {@code resource_id} as the identifier of
 *       the resource the action affected, which is exactly the loan application. Using
 *       an existing receipt field avoids adding any field to the SDK records and lets
 *       the emitter group receipts by re-reading {@code target.resourceId()}.</li>
 *   <li><b>tool output:</b> we record the tool result in
 *       {@link Execution#resultRef()} ({@code execution.result_ref}). The advisor has no
 *       result at intercept time so it passes {@code null}; here we have the output, and
 *       {@code result_ref} is the spec's slot for "a reference to the action's result".
 *       Because {@code execution} feeds {@code receipt_hash}, mutating this field is what
 *       the tamper test detects.</li>
 * </ul>
 */
public final class ReceiptFactory {

    private static final String ACTOR_ID = "loan-underwriter-agent";
    private static final String AGENT_FRAMEWORK = "jamjet-runtime-java";
    private static final String AGENT_FRAMEWORK_VERSION = "0.3.1";
    private static final String AGENT_MODEL = "deterministic-mock";

    private final String system;
    private final Environment environment;

    /**
     * @param system      the target system name (e.g. {@code "loan-underwriter"});
     *                    recorded in {@code target.system}
     * @param environment one of {@code "prod"}, {@code "staging"}, {@code "dev"}
     *                    (case-insensitive); anything else falls back to {@code dev}
     */
    public ReceiptFactory(String system, String environment) {
        if (system == null || system.isBlank()) {
            throw new IllegalArgumentException("system must not be blank");
        }
        this.system = system;
        this.environment = resolveEnvironment(environment);
    }

    /**
     * Build a fully-valid Action Receipt for a single tool call.
     *
     * @param applicationId loan application id; carried in {@code target.resource_id}
     * @param toolName       the tool/capability invoked (e.g. {@code credit_bureau.score})
     * @param argumentsJson  raw JSON arguments string (hashed into {@code arguments_hash})
     * @param output         the tool result; recorded in {@code execution.result_ref}
     * @return a validated {@link ActionReceipt} with a correct {@code receipt_hash}
     */
    public ActionReceipt forToolCall(String applicationId, String toolName, String argumentsJson, String output) {
        return forToolCall(applicationId, toolName, argumentsJson, output, null);
    }

    /**
     * Build a fully-valid Action Receipt for a single tool call, with an optional
     * {@code approval} block stamped onto the receipt.
     *
     * <p>Identical to the 4-arg overload except that {@code approval} is recorded as the
     * receipt's approval sub-record. Because {@code approval} is part of the canonical
     * content fed to {@code receipt_hash} (see {@link ReceiptHashes#computeReceiptHash}),
     * a receipt carrying an approval block still verifies: the hash is computed over the
     * same content the bundle re-hashes.
     *
     * @param applicationId loan application id; carried in {@code target.resource_id}
     * @param toolName       the tool/capability invoked (e.g. {@code disbursement.disburse})
     * @param argumentsJson  raw JSON arguments string (hashed into {@code arguments_hash})
     * @param output         the tool result; recorded in {@code execution.result_ref}
     * @param approval       human-in-the-loop approval block, or {@code null} for none
     * @return a validated {@link ActionReceipt} with a correct {@code receipt_hash}
     */
    public ActionReceipt forToolCall(
            String applicationId,
            String toolName,
            String argumentsJson,
            String output,
            Approval approval) {
        String argsHash = ReceiptHashes.computeArgumentsHash(argumentsJson);
        // Deterministic receipt_id derived from (applicationId, toolName). Each tool runs once
        // per application here, so there is no legitimate collision; a re-executed step (e.g.
        // after a crash + resume) produces the SAME id, so a disk-backed emitter overwrites it
        // rather than appending a duplicate. The id is still inside receipt_hash, so the receipt
        // verifies: deterministic is just as valid as random.
        String receiptId = UUID.nameUUIDFromBytes(
                (applicationId + "|" + toolName).getBytes(StandardCharsets.UTF_8)).toString();
        String issuedAt = Instant.now().toString();
        String completedAt = Instant.now().toString();

        Actor actor = new Actor(ActorType.AGENT, ACTOR_ID, null);
        Agent agent = new Agent(AGENT_FRAMEWORK, AGENT_FRAMEWORK_VERSION, AGENT_MODEL, null);
        // tool.name == tool.capability for these single-purpose tools (mirrors the advisor,
        // which sets capability = tc.name()).
        Tool tool = new Tool(toolName, null, toolName);
        // applicationId carried in target.resource_id (see class doc).
        Target target = new Target(system, environment, applicationId);
        Policy policy = new Policy("default.allow", "1", PolicyDecision.ALLOW);
        // tool output carried in execution.result_ref (see class doc); part of receipt_hash.
        Execution execution = new Execution(ExecutionStatus.SUCCESS, completedAt, null, output);

        // receipt_hash = SHA-256 of canonical JSON of all receipt fields EXCEPT receipt_hash.
        // The approval block (when present) is part of that canonical content.
        String receiptHash = ReceiptHashes.computeReceiptHash(
            ActionReceipt.CURRENT_VERSION, receiptId, issuedAt,
            actor, agent, tool, target, argsHash, policy, approval, execution);

        return new ActionReceipt(
            ActionReceipt.CURRENT_VERSION,
            receiptId,
            issuedAt,
            actor,
            agent,
            tool,
            target,
            argsHash,
            policy,
            approval,
            execution,
            receiptHash
        );
    }

    private static Environment resolveEnvironment(String environment) {
        if (environment == null) {
            return Environment.DEV;
        }
        if ("prod".equalsIgnoreCase(environment)) return Environment.PROD;
        if ("staging".equalsIgnoreCase(environment)) return Environment.STAGING;
        return Environment.DEV;
    }
}
