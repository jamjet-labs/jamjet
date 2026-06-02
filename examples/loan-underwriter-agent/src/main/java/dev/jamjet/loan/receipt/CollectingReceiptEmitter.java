package dev.jamjet.loan.receipt;

import dev.jamjet.cloud.agentboundary.ActionReceipt;
import dev.jamjet.cloud.agentboundary.ActionReceiptEmitter;

import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;

/**
 * Thread-safe {@link ActionReceiptEmitter} that collects receipts in memory, grouped by
 * loan application id, preserving emission order.
 *
 * <p>The grouping key is read from {@code target.resource_id}, the same field
 * {@link ReceiptFactory} writes the {@code applicationId} into. Receipts whose
 * {@code target.resource_id} is null are ignored for grouping (they cannot be addressed
 * by {@link #bundleFor(String)}).
 */
public final class CollectingReceiptEmitter implements ActionReceiptEmitter {

    private final Map<String, List<ActionReceipt>> byApplication = new ConcurrentHashMap<>();

    @Override
    public void emit(ActionReceipt receipt) {
        if (receipt == null) {
            return;
        }
        if (receipt.target() == null) {
            return;
        }
        String applicationId = receipt.target().resourceId();
        if (applicationId == null) {
            return;
        }
        byApplication
            .computeIfAbsent(applicationId, k -> new CopyOnWriteArrayList<>())
            .add(receipt);
    }

    /**
     * Return an ordered, immutable {@link AuditBundle} of all receipts emitted for the
     * given application id (empty bundle if none).
     */
    public AuditBundle bundleFor(String applicationId) {
        List<ActionReceipt> receipts = byApplication.getOrDefault(applicationId, List.of());
        return new AuditBundle(receipts);
    }
}
