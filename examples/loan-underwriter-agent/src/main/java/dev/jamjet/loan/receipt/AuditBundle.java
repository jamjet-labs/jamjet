package dev.jamjet.loan.receipt;

import dev.jamjet.cloud.agentboundary.ActionReceipt;
import dev.jamjet.cloud.agentboundary.Execution;
import dev.jamjet.cloud.agentboundary.ReceiptHashes;

import java.util.ArrayList;
import java.util.List;
import java.util.Objects;

/**
 * An ordered, immutable collection of {@link ActionReceipt}s for one loan application,
 * with independent hash verification.
 *
 * <p>{@link #verify()} recomputes each receipt's {@code receipt_hash} from the same
 * canonical content the SDK hashes (version, receipt_id, issued_at, actor, agent, tool,
 * target, arguments_hash, policy, approval, execution) and compares it to the stored
 * {@code receipt_hash}. The bundle verifies only if every receipt matches.
 */
public final class AuditBundle {

    private final List<ActionReceipt> receipts;

    public AuditBundle(List<ActionReceipt> receipts) {
        this.receipts = List.copyOf(Objects.requireNonNull(receipts, "receipts must not be null"));
    }

    /** The receipts in emission order; immutable. */
    public List<ActionReceipt> receipts() {
        return receipts;
    }

    /**
     * Recompute every receipt's {@code receipt_hash} over its canonical content and compare
     * to the stored value.
     *
     * <p>An empty bundle vacuously returns true; callers that require receipts to be present
     * should check receipts().isEmpty() separately.
     *
     * @return {@code true} iff all receipts recompute to their stored {@code receipt_hash}
     */
    public boolean verify() {
        for (ActionReceipt r : receipts) {
            String recomputed = ReceiptHashes.computeReceiptHash(
                r.version(), r.receiptId(), r.issuedAt(),
                r.actor(), r.agent(), r.tool(), r.target(),
                r.argumentsHash(), r.policy(), r.approval(), r.execution());
            if (!recomputed.equals(r.receiptHash())) {
                return false;
            }
        }
        return true;
    }

    /**
     * Test-only tamper injection: returns a copy whose receipt[idx] has a forged result_ref
     * while keeping the original receipt_hash, so verify() detects the tamper. Not part of
     * the public API.
     *
     * <p>The mutated field is {@link Execution#resultRef()} ({@code execution.result_ref}),
     * which {@link ReceiptFactory} uses to record the tool output. Because {@code execution}
     * is part of the canonical content fed to {@code receipt_hash}, the resulting receipt's
     * stored hash no longer matches its content, so {@link #verify()} returns {@code false}.
     *
     * @param idx      index of the receipt to tamper
     * @param newValue the forged tool output to write into {@code execution.result_ref}
     * @return a new tampered {@link AuditBundle}
     */
    AuditBundle withMutatedOutput(int idx, String newValue) {
        List<ActionReceipt> mutated = new ArrayList<>(receipts);
        ActionReceipt original = mutated.get(idx);
        Execution oldExec = original.execution();
        Execution forgedExec = new Execution(
            oldExec.status(), oldExec.completedAt(), oldExec.errorCode(), newValue);

        // Rebuild the receipt with the forged execution but the ORIGINAL receipt_hash retained.
        ActionReceipt tampered = new ActionReceipt(
            original.version(),
            original.receiptId(),
            original.issuedAt(),
            original.actor(),
            original.agent(),
            original.tool(),
            original.target(),
            original.argumentsHash(),
            original.policy(),
            original.approval(),
            forgedExec,
            original.receiptHash()   // stale hash from before the mutation; this is the tamper
        );
        mutated.set(idx, tampered);
        return new AuditBundle(mutated);
    }
}
