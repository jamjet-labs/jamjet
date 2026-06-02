package dev.jamjet.loan.durable;

import dev.jamjet.loan.approval.ApprovalGate;
import dev.jamjet.loan.domain.LoanApplication;
import dev.jamjet.loan.domain.RunState;
import dev.jamjet.loan.receipt.AuditBundle;
import dev.jamjet.loan.receipt.CollectingReceiptEmitter;
import dev.jamjet.loan.receipt.ReceiptFactory;
import dev.jamjet.loan.service.AccountHistoryService;
import dev.jamjet.loan.service.CreditBureauService;
import dev.jamjet.loan.service.DisbursementService;
import dev.jamjet.runtime.core.event.ApprovalDecision;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import java.nio.file.Path;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;

import static org.junit.jupiter.api.Assertions.*;

class UnderwritingRunnerTest {

    /** Counting, idempotent-by-application-id disbursement fake. */
    private static final class CountingDisbursement implements DisbursementService {
        final AtomicInteger calls = new AtomicInteger();
        private final ConcurrentHashMap<String, String> ledger = new ConcurrentHashMap<>();

        @Override
        public String disburse(String applicationId, long amountCents) {
            calls.incrementAndGet();
            return ledger.computeIfAbsent(applicationId, id -> "REF-" + id);
        }
    }

    private UnderwritingRunner newRunner(Path cpDir, Path apDir,
                                         CollectingReceiptEmitter emitter,
                                         DisbursementService disbursement) {
        CreditBureauService credit = app -> 720;
        AccountHistoryService history = app -> 36;
        ReceiptFactory factory = new ReceiptFactory("loan-underwriter", "dev");
        UnderwritingAgent agent = new UnderwritingAgent(credit, history, emitter, factory);
        CheckpointStore checkpoints = new CheckpointStore(cpDir);
        ApprovalGate gate = new ApprovalGate(apDir);
        return new UnderwritingRunner(agent, checkpoints, gate, disbursement, emitter, factory);
    }

    private static boolean hasDisbursementReceipt(AuditBundle bundle) {
        return bundle.receipts().stream()
                .anyMatch(r -> "disbursement.disburse".equals(r.tool().name()));
    }

    @Test
    void startSuspendsForApprovalAndDoesNotDisburse(@TempDir Path cpDir, @TempDir Path apDir) {
        var emitter = new CollectingReceiptEmitter();
        var disbursement = new CountingDisbursement();
        var runner = newRunner(cpDir, apDir, emitter, disbursement);
        var app = new LoanApplication("app-start", "Ada", 20_000, 90_000);

        RunState state = runner.start(app);

        assertEquals(RunState.AWAITING_APPROVAL, state);
        assertEquals(0, disbursement.calls.get(), "must not disburse before approval");
        assertFalse(hasDisbursementReceipt(emitter.bundleFor("app-start")),
                "no disbursement receipt before approval");
    }

    @Test
    void approvedResumeDisbursesOnceWithStampedApprovalAndVerifies(@TempDir Path cpDir, @TempDir Path apDir) {
        var emitter = new CollectingReceiptEmitter();
        var disbursement = new CountingDisbursement();
        var runner = newRunner(cpDir, apDir, emitter, disbursement);
        var app = new LoanApplication("app-ok", "Ada", 20_000, 90_000);

        assertEquals(RunState.AWAITING_APPROVAL, runner.start(app));

        // Human approves out of band.
        var gate = new ApprovalGate(apDir);
        gate.decide("app-ok", "officer@bank", ApprovalDecision.APPROVED, "ok");

        RunState resumed = runner.resume("app-ok");
        assertEquals(RunState.COMPLETED, resumed);
        assertEquals(1, disbursement.calls.get(), "approved resume disburses exactly once");

        AuditBundle bundle = emitter.bundleFor("app-ok");
        var disbursementReceipt = bundle.receipts().stream()
                .filter(r -> "disbursement.disburse".equals(r.tool().name()))
                .findFirst()
                .orElseThrow(() -> new AssertionError("expected a disbursement receipt"));
        assertNotNull(disbursementReceipt.approval(), "disbursement receipt carries the approval block");
        assertEquals("officer@bank", disbursementReceipt.approval().approver().id());
        assertTrue(bundle.verify(), "bundle still verifies with the approval block stamped");

        // Idempotency: a second resume must not disburse again.
        RunState resumedAgain = runner.resume("app-ok");
        assertEquals(RunState.COMPLETED, resumedAgain);
        assertEquals(1, disbursement.calls.get(), "second resume must not disburse again");
        // And it must not emit a duplicate disbursement receipt.
        long disbursementReceipts = emitter.bundleFor("app-ok").receipts().stream()
                .filter(r -> "disbursement.disburse".equals(r.tool().name()))
                .count();
        assertEquals(1, disbursementReceipts, "exactly one disbursement receipt");
    }

    @Test
    void persistedCheckpointHoldsOnlyPrimitivesNotTheDecision(@TempDir Path cpDir, @TempDir Path apDir) {
        var emitter = new CollectingReceiptEmitter();
        var runner = newRunner(cpDir, apDir, emitter, new CountingDisbursement());
        var app = new LoanApplication("app-cp", "Ada", 20_000, 90_000);

        runner.start(app);

        // The on-disk checkpoint must rehydrate cleanly across a fresh store and contain
        // only the int checkpoints (credit, history): never the non-primitive Decision.
        var reloaded = new CheckpointStore(cpDir).load("app-cp");
        assertNotNull(reloaded, "checkpoint must have been persisted");
        assertEquals(java.util.List.of("credit", "history"), reloaded.getCheckpointIds(),
                "only credit and history are durable; the Decision is recomputed, never persisted");
        assertInstanceOf(Integer.class, reloaded.getRecordedResult("credit"));
        assertInstanceOf(Integer.class, reloaded.getRecordedResult("history"));
    }

    /**
     * A declined application (credit score below 580) must terminate in start() with
     * RunState.DECLINED. No approval must be requested, and no disbursement receipt may
     * appear. A subsequent resume() must not disburse either.
     *
     * <p>Verified score: "app-ok".hashCode() = -1634616714;
     * Math.floorMod(-1634616714, 551) = 108; credit = 300 + 108 = 408 (DECLINE).
     */
    @Test
    void declinedApplicationTerminatesWithNoApprovalAndNoDisbursement(
            @TempDir Path cpDir, @TempDir Path apDir) {
        var emitter = new CollectingReceiptEmitter();
        var disbursement = new CountingDisbursement();

        // Use the real MockCreditBureau so the formula (300 + floorMod(hash, 551)) applies.
        ReceiptFactory factory = new ReceiptFactory("loan-underwriter", "dev");
        CreditBureauService credit = new dev.jamjet.loan.service.MockCreditBureau();
        AccountHistoryService history = new dev.jamjet.loan.service.MockAccountHistory();
        UnderwritingAgent agent = new UnderwritingAgent(credit, history, emitter, factory);
        CheckpointStore checkpoints = new CheckpointStore(cpDir);
        ApprovalGate gate = new ApprovalGate(apDir);
        var runner = new UnderwritingRunner(agent, checkpoints, gate, disbursement, emitter, factory);

        // "app-ok" -> credit = 408 -> DECLINE
        var app = new LoanApplication("app-ok", "Bob", 20_000, 90_000);
        RunState state = runner.start(app);

        assertEquals(RunState.DECLINED, state, "start() must return DECLINED for a hard-decline application");
        assertNull(gate.stateOf("app-ok"), "approval gate must not have been touched for a declined application");
        assertEquals(0, disbursement.calls.get(), "declined application must not trigger disbursement");
        assertFalse(hasDisbursementReceipt(emitter.bundleFor("app-ok")),
                "declined application must not produce a disbursement receipt");

        // resume() on a declined run must also not disburse
        RunState resumed = runner.resume("app-ok");
        assertEquals(RunState.DECLINED, resumed, "resume() on a declined run must return DECLINED");
        assertEquals(0, disbursement.calls.get(), "resume() on a declined run must not disburse");
    }

    @Test
    void rejectedResumeFailsAndDoesNotDisburse(@TempDir Path cpDir, @TempDir Path apDir) {
        var emitter = new CollectingReceiptEmitter();
        var disbursement = new CountingDisbursement();
        var runner = newRunner(cpDir, apDir, emitter, disbursement);
        var app = new LoanApplication("app-no", "Ada", 20_000, 90_000);

        assertEquals(RunState.AWAITING_APPROVAL, runner.start(app));

        var gate = new ApprovalGate(apDir);
        gate.decide("app-no", "officer@bank", ApprovalDecision.REJECTED, "income unverified");

        RunState resumed = runner.resume("app-no");
        assertEquals(RunState.FAILED, resumed);
        assertEquals(0, disbursement.calls.get(), "rejected run must not disburse");
        assertFalse(hasDisbursementReceipt(emitter.bundleFor("app-no")),
                "no disbursement receipt for a rejected run");
    }
}
