package dev.jamjet.loan.durable;

import dev.jamjet.cloud.agentboundary.ActionReceiptEmitter;
import dev.jamjet.cloud.agentboundary.Approval;
import dev.jamjet.loan.approval.ApprovalGate;
import dev.jamjet.loan.domain.Decision;
import dev.jamjet.loan.domain.LoanApplication;
import dev.jamjet.loan.domain.RunState;
import dev.jamjet.loan.receipt.ReceiptFactory;
import dev.jamjet.loan.service.DisbursementService;
import dev.jamjet.runtime.instrument.DurabilityContext;

import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Orchestrates a single human-in-the-loop underwriting run.
 *
 * <p>{@link #start} runs the durable credit → history → score pipeline (each step
 * checkpointed via {@link CheckpointStore}) and then <em>suspends</em> at the disbursement
 * step by registering a pending approval with the {@link ApprovalGate}. It does not
 * disburse. A human records a decision out of band ({@code gate.decide(...)}); a later
 * {@link #resume} call either disburses (if approved) or fails (if rejected).
 *
 * <p>Disbursement is guarded against repetition two ways: the run id is tracked in a
 * {@code completed} set so {@link #resume} short-circuits once done, and the underlying
 * {@link DisbursementService} is idempotent by application id. The disbursement receipt is
 * emitted exactly once, carrying the {@link Approval} block from {@link ApprovalGate#toApprovalBlock}.
 *
 * <p>Cross-restart note: the tracked-application map and completed-set are in-memory; after
 * a restart, call start(app) again (it resumes from the durable CheckpointStore) before resume().
 */
public final class UnderwritingRunner {

    private static final String TOOL_DISBURSE = "disbursement.disburse";
    private static final String DEFAULT_APPROVER = "approver@bank";

    private final UnderwritingAgent agent;
    private final CheckpointStore checkpoints;
    private final ApprovalGate approvalGate;
    private final DisbursementService disbursement;
    private final ActionReceiptEmitter emitter;
    private final ReceiptFactory factory;

    // Run id is the loan application id throughout (one run per application here).
    private final Map<String, LoanApplication> applications = new ConcurrentHashMap<>();
    // In-process only (not persisted). After a process restart, completion is re-established
    // by calling start() again before resume(); the idempotent DisbursementService prevents double-pay.
    private final Set<String> completed = ConcurrentHashMap.newKeySet();
    // Tracks runs that were hard-declined by the underwriting scorer. Persisted as in-memory
    // only; after a restart, start() re-derives the decision (score is a pure function of the
    // already-durable credit + history values) and re-populates this set.
    private final Set<String> declined = ConcurrentHashMap.newKeySet();

    public UnderwritingRunner(
            UnderwritingAgent agent,
            CheckpointStore checkpoints,
            ApprovalGate approvalGate,
            DisbursementService disbursement,
            ActionReceiptEmitter emitter,
            ReceiptFactory factory) {
        this.agent = agent;
        this.checkpoints = checkpoints;
        this.approvalGate = approvalGate;
        this.disbursement = disbursement;
        this.emitter = emitter;
        this.factory = factory;
    }

    /**
     * Run the durable underwriting pipeline for {@code app}, persisting a checkpoint per
     * step, then gate on the underwriting decision:
     * <ul>
     *   <li>DECLINE: terminal immediately. No approval is requested, no disbursement possible.
     *       Returns {@link RunState#DECLINED}.</li>
     *   <li>REFER or APPROVE: suspend pending human approval. Returns
     *       {@link RunState#AWAITING_APPROVAL} without disbursing.</li>
     * </ul>
     * The credit, history, and score receipts are emitted in all cases.
     */
    public RunState start(LoanApplication app) {
        String runId = app.id();
        applications.put(runId, app);

        // Resume an in-flight context in replay mode if one was checkpointed; else fresh.
        DurabilityContext ctx = checkpoints.load(runId);
        if (ctx == null) {
            ctx = DurabilityContext.create();
        }
        DurabilityContext.setCurrent(ctx);
        Decision decision;
        try {
            int credit = agent.pullCredit(app);
            checkpoints.save(runId, ctx);
            int history = agent.pullHistory(app);
            checkpoints.save(runId, ctx);
            // score is a pure recompute (not a durable @Checkpoint), so it adds nothing to the
            // context and there is nothing new to persist after it; this also keeps the
            // non-primitive Decision out of the on-disk checkpoint.
            decision = agent.score(app, credit, history);
        } finally {
            DurabilityContext.clear();
        }

        // Hard decline: terminal. Do not ask for approval, do not allow disbursement.
        if (decision.outcome() == Decision.Outcome.DECLINE) {
            declined.add(runId);
            return RunState.DECLINED;
        }

        // REFER or APPROVE: suspend at the guarded disbursement step pending human sign-off.
        approvalGate.requireApproval(runId, TOOL_DISBURSE, DEFAULT_APPROVER);
        return RunState.AWAITING_APPROVAL;
    }

    /**
     * Resume a suspended run once a human decision has been recorded.
     * <ul>
     *   <li>Declined (hard-decline from the scorer): returns {@link RunState#DECLINED}
     *       immediately with no disbursement. A declined run can never be resumed into
     *       disbursement.</li>
     *   <li>Approved: disburse once (idempotent), emit the disbursement receipt with the
     *       approval block stamped, mark {@link RunState#COMPLETED}.</li>
     *   <li>Rejected (or any non-approved terminal state): {@link RunState#FAILED}, no
     *       disbursement, no receipt.</li>
     * </ul>
     * Calling {@code resume} again after completion is a no-op that returns COMPLETED and
     * does not disburse a second time.
     */
    public RunState resume(String runId) {
        // A hard-decline is terminal: no disbursement path exists.
        if (declined.contains(runId)) {
            return RunState.DECLINED;
        }

        if (completed.contains(runId)) {
            return RunState.COMPLETED;
        }

        // Reject calls for completely unknown runs: no approval record and not tracked in-process.
        RunState gateState = approvalGate.stateOf(runId);
        if (gateState == null && !applications.containsKey(runId)) {
            throw new IllegalStateException(
                    "Unknown or unstarted run: " + runId + ". Call start() first.");
        }

        if (!approvalGate.isCleared(runId)) {
            // Not approved: surface the gate's state. A recorded rejection maps to FAILED.
            return gateState == RunState.FAILED ? RunState.FAILED : gateState;
        }

        LoanApplication app = applications.get(runId);
        if (app == null) {
            throw new IllegalStateException("No application tracked for run: " + runId);
        }

        // Idempotent disbursement (service is keyed by application id).
        String reference = disbursement.disburse(runId, app.amountCents());

        // Stamp the human approval onto the disbursement receipt.
        Approval approval = approvalGate.toApprovalBlock(runId);
        emitter.emit(factory.forToolCall(
                runId,
                TOOL_DISBURSE,
                "{\"applicationId\":\"" + runId + "\",\"amountCents\":" + app.amountCents() + "}",
                reference,
                approval));

        completed.add(runId);
        return RunState.COMPLETED;
    }

    /**
     * Best-effort current state for {@code runId}: DECLINED for a hard-decline, COMPLETED
     * once disbursed, otherwise the approval gate's persisted state (AWAITING_APPROVAL,
     * RUNNING, FAILED), or {@code null} if unknown.
     */
    public RunState stateOf(String runId) {
        if (declined.contains(runId)) {
            return RunState.DECLINED;
        }
        if (completed.contains(runId)) {
            return RunState.COMPLETED;
        }
        return approvalGate.stateOf(runId);
    }
}
