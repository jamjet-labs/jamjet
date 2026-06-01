package dev.jamjet.loan.approval;

import dev.jamjet.loan.domain.RunState;
import dev.jamjet.runtime.core.event.ApprovalDecision;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import java.nio.file.Path;
import static org.junit.jupiter.api.Assertions.*;

class ApprovalGateTest {
    @Test
    void runIsAwaitingUntilDecisionArrives(@TempDir Path dir) {
        var gate = new ApprovalGate(dir);
        gate.requireApproval("run-1", "disbursement.disburse", "approver@bank");
        assertEquals(RunState.AWAITING_APPROVAL, gate.stateOf("run-1"));
        assertFalse(gate.isCleared("run-1"));

        gate.decide("run-1", "officer@bank", ApprovalDecision.APPROVED, "looks good");
        assertTrue(gate.isCleared("run-1"));
        assertEquals(RunState.RUNNING, gate.stateOf("run-1"));
    }

    @Test
    void rejectionBlocksDisbursement(@TempDir Path dir) {
        var gate = new ApprovalGate(dir);
        gate.requireApproval("run-2", "disbursement.disburse", "approver@bank");
        gate.decide("run-2", "officer@bank", ApprovalDecision.REJECTED, "income unverified");
        assertFalse(gate.isCleared("run-2"));
        assertEquals(RunState.FAILED, gate.stateOf("run-2"));
    }

    @Test
    void pendingStateSurvivesReload(@TempDir Path dir) {
        new ApprovalGate(dir).requireApproval("run-3", "disbursement.disburse", "a@bank");
        // A fresh gate instance (simulating a process restart) sees the pending request.
        assertEquals(RunState.AWAITING_APPROVAL, new ApprovalGate(dir).stateOf("run-3"));
    }

    @Test
    void requireApprovalDoesNotEraseAnExistingDecision(@TempDir Path dir) {
        var gate = new ApprovalGate(dir);
        gate.requireApproval("run-x", "disbursement.disburse", "approver@bank");
        gate.decide("run-x", "officer@bank", ApprovalDecision.APPROVED, "ok");
        // A redundant requireApproval (e.g. after a restart + re-start) must NOT wipe the decision.
        gate.requireApproval("run-x", "disbursement.disburse", "approver@bank");
        assertTrue(gate.isCleared("run-x"), "prior APPROVED decision must survive a redundant requireApproval");
        assertEquals(RunState.RUNNING, gate.stateOf("run-x"));
    }
}
