package dev.jamjet.loan.durable;

import dev.jamjet.loan.domain.LoanApplication;
import dev.jamjet.loan.service.AccountHistoryService;
import dev.jamjet.loan.service.CreditBureauService;
import dev.jamjet.runtime.instrument.DurabilityContext;
import org.junit.jupiter.api.Test;
import java.util.concurrent.atomic.AtomicInteger;
import static org.junit.jupiter.api.Assertions.*;

class UnderwritingAgentResumeTest {
    @Test
    void resumeDoesNotRepeatCompletedCheckpoints() {
        var creditCalls = new AtomicInteger();
        CreditBureauService credit = app -> { creditCalls.incrementAndGet(); return 720; };
        AccountHistoryService history = app -> 36;
        var agent = new UnderwritingAgent(credit, history);
        var app = new LoanApplication("app-1", "Ada", 25_000, 60_000);

        // First pass: run the credit step, persist into a context, simulate crash before scoring.
        var ctx = DurabilityContext.create();
        DurabilityContext.setCurrent(ctx);
        try {
            agent.pullCredit(app);
        } finally {
            DurabilityContext.clear();
        }
        assertEquals(1, creditCalls.get());

        // Resume: rehydrate ctx in replay mode and run the full pipeline.
        ctx.setReplayMode(true);
        DurabilityContext.setCurrent(ctx);
        try {
            agent.pullCredit(app);          // replays: must NOT call the service again
            agent.pullHistory(app);
        } finally {
            DurabilityContext.clear();
        }
        assertEquals(1, creditCalls.get(), "credit pull must not repeat on resume");
    }

    @Test
    void scoreAppliesDeterministicRule() {
        var agent = new UnderwritingAgent(app -> 720, app -> 36);
        var ctx = DurabilityContext.create();   // non-replay: suppliers run
        DurabilityContext.setCurrent(ctx);
        try {
            // APPROVE: credit>=680, history>=24, amount 20k <= 40% of 90k (=36k)
            var approve = agent.score(new dev.jamjet.loan.domain.LoanApplication("a", "n", 20_000, 90_000), 720, 36);
            assertEquals(dev.jamjet.loan.domain.Decision.Outcome.APPROVE, approve.outcome());
            // DECLINE: credit below 580
            var decline = agent.score(new dev.jamjet.loan.domain.LoanApplication("b", "n", 20_000, 90_000), 500, 36);
            assertEquals(dev.jamjet.loan.domain.Decision.Outcome.DECLINE, decline.outcome());
            // REFER: mid credit (580<=score<680)
            var refer = agent.score(new dev.jamjet.loan.domain.LoanApplication("c", "n", 20_000, 90_000), 650, 36);
            assertEquals(dev.jamjet.loan.domain.Decision.Outcome.REFER, refer.outcome());
        } finally {
            DurabilityContext.clear();
        }
    }
}
