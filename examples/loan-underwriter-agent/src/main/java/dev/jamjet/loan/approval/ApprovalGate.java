package dev.jamjet.loan.approval;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.ObjectMapper;
import dev.jamjet.cloud.agentboundary.Approval;
import dev.jamjet.loan.domain.RunState;
import dev.jamjet.runtime.core.event.ApprovalDecision;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.time.Instant;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * Disk-backed human-in-the-loop approval gate. A run that reaches a guarded action
 * (e.g. {@code disbursement.disburse}) suspends by writing a pending approval record;
 * an out-of-band {@link #decide} call records the human decision. Because every read
 * re-reads the on-disk record, a fresh {@code ApprovalGate} (after a process restart)
 * observes prior state: suspend/await/resume survives restart.
 *
 * <p>Each run is stored as {@code <baseDir>/<runId>.approval.json}. Writes are atomic
 * (temp file + {@code ATOMIC_MOVE}, mirroring {@code CheckpointStore}), so a crash
 * mid-write cannot corrupt an existing record.
 *
 * <p>The lifecycle mirrors the runtime event kinds:
 * {@code requireApproval} → {@code EventKind.ToolApprovalRequired};
 * {@code decide} → {@code EventKind.ApprovalReceived}.
 */
public final class ApprovalGate {

    private static final ObjectMapper MAPPER = new ObjectMapper();
    private static final TypeReference<LinkedHashMap<String, String>> MAP_TYPE =
            new TypeReference<>() {};
    private static final String APPROVAL_EXT = ".approval.json";

    // Record field keys.
    private static final String K_RUN_ID    = "runId";
    private static final String K_TOOL_NAME = "toolName";
    private static final String K_APPROVER  = "approver";
    private static final String K_STATE     = "state";
    private static final String K_USER_ID   = "userId";
    private static final String K_DECISION  = "decision";
    private static final String K_COMMENT   = "comment";
    private static final String K_APPROVED_AT = "approvedAt";

    private final Path baseDir;

    public ApprovalGate(Path baseDir) {
        try {
            Files.createDirectories(baseDir);
        } catch (IOException e) {
            throw new UncheckedIOException("Cannot create approval directory: " + baseDir, e);
        }
        this.baseDir = baseDir;
    }

    /**
     * Suspend the run pending human approval of {@code toolName}. Persists a pending
     * record in state {@link RunState#AWAITING_APPROVAL}. Mirrors
     * {@code EventKind.ToolApprovalRequired}.
     *
     * <p>Idempotent-safe: if a record already exists for {@code runId} and it already
     * carries a terminal decision (approved or rejected), this method returns without
     * overwriting it. This makes a redundant {@code start()}/{@code requireApproval}
     * call after a process restart safe: a human's prior decision is preserved.
     */
    public void requireApproval(String runId, String toolName, String approver) {
        RunState existing = stateOf(runId);
        if (existing != null && existing != RunState.AWAITING_APPROVAL) {
            // A decision has already been recorded; do not overwrite it.
            return;
        }
        Map<String, String> record = new LinkedHashMap<>();
        record.put(K_RUN_ID, runId);
        record.put(K_TOOL_NAME, toolName);
        record.put(K_APPROVER, approver);
        record.put(K_STATE, RunState.AWAITING_APPROVAL.name());
        write(runId, record);
    }

    /**
     * Record a human decision against a pending run. Mirrors {@code EventKind.ApprovalReceived}.
     * State mapping: APPROVED → RUNNING; REJECTED → FAILED; ESCALATE → AWAITING_APPROVAL.
     */
    public void decide(String runId, String userId, ApprovalDecision decision, String comment) {
        Map<String, String> record = read(runId);
        if (record == null) {
            throw new IllegalStateException("No pending approval for run: " + runId);
        }
        record.put(K_USER_ID, userId);
        record.put(K_DECISION, decision.getValue());
        record.put(K_COMMENT, comment);
        record.put(K_APPROVED_AT, Instant.now().toString());
        record.put(K_STATE, stateFor(decision).name());
        write(runId, record);
    }

    /** The persisted run state, or {@code null} if no record exists for {@code runId}. */
    public RunState stateOf(String runId) {
        Map<String, String> record = read(runId);
        if (record == null) {
            return null;
        }
        String state = record.get(K_STATE);
        return state == null ? null : RunState.valueOf(state);
    }

    /** True iff a decision has been recorded and it is APPROVED. */
    public boolean isCleared(String runId) {
        Map<String, String> record = read(runId);
        if (record == null) {
            return false;
        }
        String decision = record.get(K_DECISION);
        return decision != null && ApprovalDecision.fromValue(decision) == ApprovalDecision.APPROVED;
    }

    /**
     * Build an AgentBoundary {@link Approval} block from the persisted decision, suitable
     * for stamping onto the guarded action's receipt. Returns {@code null} if no decision
     * has been recorded yet.
     */
    public Approval toApprovalBlock(String runId) {
        Map<String, String> record = read(runId);
        if (record == null || record.get(K_DECISION) == null) {
            return null;
        }
        Approval.Approver approver = new Approval.Approver(record.get(K_USER_ID), null, "approver");
        return new Approval(approver, record.get(K_APPROVED_AT), record.get(K_COMMENT));
    }

    private static RunState stateFor(ApprovalDecision decision) {
        return switch (decision) {
            case APPROVED -> RunState.RUNNING;
            case REJECTED -> RunState.FAILED;
            case ESCALATE -> RunState.AWAITING_APPROVAL;
        };
    }

    private Path pathFor(String runId) {
        return baseDir.resolve(runId + APPROVAL_EXT);
    }

    private Map<String, String> read(String runId) {
        Path target = pathFor(runId);
        if (!Files.exists(target)) {
            return null;
        }
        try {
            return MAPPER.readValue(target.toFile(), MAP_TYPE);
        } catch (IOException e) {
            throw new UncheckedIOException("Failed to load approval for run: " + runId, e);
        }
    }

    private void write(String runId, Map<String, String> record) {
        Path target = pathFor(runId);
        try {
            Path tmp = Files.createTempFile(baseDir, ".ap-" + runId + "-", ".tmp");
            try {
                MAPPER.writerWithDefaultPrettyPrinter().writeValue(tmp.toFile(), record);
                Files.move(tmp, target,
                        StandardCopyOption.ATOMIC_MOVE,
                        StandardCopyOption.REPLACE_EXISTING);
            } catch (IOException e) {
                try { Files.deleteIfExists(tmp); } catch (IOException ignored) {}
                throw e;
            }
        } catch (IOException e) {
            throw new UncheckedIOException("Failed to save approval for run: " + runId, e);
        }
    }
}
