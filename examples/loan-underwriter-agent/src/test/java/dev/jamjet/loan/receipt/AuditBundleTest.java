package dev.jamjet.loan.receipt;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

class AuditBundleTest {

    @Test
    void bundlePreservesOrderAndVerifies() {
        var emitter = new CollectingReceiptEmitter();
        var factory = new ReceiptFactory("loan-underwriter", "dev");
        emitter.emit(factory.forToolCall("app-1", "credit_bureau.score", "{\"id\":\"app-1\"}", "720"));
        emitter.emit(factory.forToolCall("app-1", "disbursement.disburse", "{\"amount\":25000}", "ref-9"));

        AuditBundle bundle = emitter.bundleFor("app-1");
        assertEquals(2, bundle.receipts().size());
        assertEquals("credit_bureau.score", bundle.receipts().get(0).tool().name());
        assertTrue(bundle.verify(), "every receipt_hash must recompute to its stored value");
    }

    @Test
    void receiptIdIsDeterministicPerAppAndTool() {
        var factory = new ReceiptFactory("loan-underwriter", "dev");
        // Same (applicationId, toolName) -> same receipt_id, so a re-executed step overwrites
        // rather than duplicates. Differing args/output must not change the id.
        var first = factory.forToolCall("app-9", "credit_bureau.score", "{\"a\":1}", "720");
        var second = factory.forToolCall("app-9", "credit_bureau.score", "{\"a\":2}", "999");
        assertEquals(first.receiptId(), second.receiptId(),
                "receipt_id must be a pure function of (applicationId, toolName)");
        // Different tool -> different id.
        var other = factory.forToolCall("app-9", "account_history.months", "{\"a\":1}", "36");
        assertNotEquals(first.receiptId(), other.receiptId());
        // Different app -> different id.
        var otherApp = factory.forToolCall("app-X", "credit_bureau.score", "{\"a\":1}", "720");
        assertNotEquals(first.receiptId(), otherApp.receiptId());
        // Deterministic-id receipts still verify.
        var bundle = new AuditBundle(java.util.List.of(first, other));
        assertTrue(bundle.verify());
    }

    @Test
    void tamperingBreaksVerification() {
        var emitter = new CollectingReceiptEmitter();
        var factory = new ReceiptFactory("loan-underwriter", "dev");
        emitter.emit(factory.forToolCall("app-2", "disbursement.disburse", "{\"amount\":1}", "ref-1"));
        AuditBundle bundle = emitter.bundleFor("app-2");
        AuditBundle tampered = bundle.withMutatedOutput(0, "ref-HACKED");
        assertFalse(tampered.verify(), "mutated output must fail hash verification");
    }
}
