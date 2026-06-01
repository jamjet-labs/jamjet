package dev.jamjet.loan.durable;

import dev.jamjet.loan.domain.LoanApplication;
import dev.jamjet.loan.receipt.*;
import dev.jamjet.runtime.instrument.DurabilityContext;
import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

class AgentEmitsReceiptsTest {
    @Test
    void everyGuardedActionProducesAReceipt() {
        var emitter = new CollectingReceiptEmitter();
        var factory = new ReceiptFactory("loan-underwriter", "test");
        var agent = new UnderwritingAgent(app -> 720, app -> 36, emitter, factory);
        var app = new LoanApplication("app-7", "Grace", 20_000, 90_000);

        var ctx = DurabilityContext.create();
        DurabilityContext.setCurrent(ctx);
        try {
            agent.pullCredit(app);
            agent.pullHistory(app);
        } finally {
            DurabilityContext.clear();
        }

        var bundle = emitter.bundleFor("app-7");
        assertEquals(2, bundle.receipts().size());
        assertTrue(bundle.verify());
    }
}
