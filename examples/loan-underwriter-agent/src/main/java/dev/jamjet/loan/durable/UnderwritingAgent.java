package dev.jamjet.loan.durable;

import dev.jamjet.cloud.agentboundary.ActionReceiptEmitter;
import dev.jamjet.loan.domain.Decision;
import dev.jamjet.loan.domain.Decision.Outcome;
import dev.jamjet.loan.domain.LoanApplication;
import dev.jamjet.loan.receipt.CollectingReceiptEmitter;
import dev.jamjet.loan.receipt.ReceiptFactory;
import dev.jamjet.loan.service.AccountHistoryService;
import dev.jamjet.loan.service.CreditBureauService;
import dev.jamjet.runtime.instrument.DurabilityContext;
import dev.jamjet.runtime.instrument.annotations.Checkpoint;
import dev.jamjet.runtime.instrument.annotations.DurableAgent;

import java.util.ArrayList;
import java.util.List;

@DurableAgent("loan-underwriting")
public class UnderwritingAgent {

    private static final int SCORE_HARD_DECLINE = 580;
    private static final int SCORE_APPROVE_MIN  = 680;
    private static final int MIN_HISTORY_MONTHS  = 24;

    private static final String TOOL_CREDIT  = "credit_bureau.score";
    private static final String TOOL_HISTORY = "account_history.months";
    private static final String TOOL_SCORE   = "underwriting.score";

    private final CreditBureauService creditService;
    private final AccountHistoryService historyService;
    private final ActionReceiptEmitter emitter;
    private final ReceiptFactory factory;

    /**
     * Convenience constructor for tests and simple wiring that do not need receipt
     * observation. Receipts are still emitted into a private {@link CollectingReceiptEmitter}
     * but no caller holds a reference to it, so they are effectively discarded.
     */
    public UnderwritingAgent(CreditBureauService creditService, AccountHistoryService historyService) {
        this(creditService, historyService,
             new CollectingReceiptEmitter(),
             new ReceiptFactory("loan-underwriter", "dev"));
    }

    /**
     * Full constructor. Receipts for every guarded action are emitted to {@code emitter}
     * (any {@link ActionReceiptEmitter}, in-memory for tests or disk-backed in production)
     * using {@code factory} to build them.
     */
    public UnderwritingAgent(
            CreditBureauService creditService,
            AccountHistoryService historyService,
            ActionReceiptEmitter emitter,
            ReceiptFactory factory) {
        this.creditService = creditService;
        this.historyService = historyService;
        this.emitter = emitter;
        this.factory = factory;
    }

    private static dev.jamjet.runtime.instrument.DurabilityContext requireContext() {
        var ctx = dev.jamjet.runtime.instrument.DurabilityContext.current();
        if (ctx == null) {
            throw new IllegalStateException(
                "No DurabilityContext bound to this thread; wrap the call in DurabilityContext.setCurrent(...)/clear().");
        }
        return ctx;
    }

    @Checkpoint("credit")
    public int pullCredit(LoanApplication app) {
        return requireContext().replayOrExecute("credit", () -> {
            int value = creditService.score(app);
            emitter.emit(factory.forToolCall(
                app.id(),
                TOOL_CREDIT,
                "{\"applicationId\":\"" + app.id() + "\"}",
                String.valueOf(value)));
            return value;
        });
    }

    @Checkpoint("history")
    public int pullHistory(LoanApplication app) {
        return requireContext().replayOrExecute("history", () -> {
            int value = historyService.monthsHistory(app);
            emitter.emit(factory.forToolCall(
                app.id(),
                TOOL_HISTORY,
                "{\"applicationId\":\"" + app.id() + "\"}",
                String.valueOf(value)));
            return value;
        });
    }

    /**
     * Compute the underwriting decision and emit its receipt.
     *
     * <p>Deliberately NOT a durable {@code @Checkpoint}: the decision is a pure function of
     * {@code creditScore} + {@code monthsHistory} (both already durable) and is cheap to
     * recompute, so persisting it buys nothing. Keeping it out of the {@link DurabilityContext}
     * also guarantees the non-primitive {@link Decision} never reaches the on-disk
     * {@link CheckpointStore}, so a checkpoint reloaded after a restart only ever holds JSON
     * scalars and rehydrates without a {@code ClassCastException}. The receipt is still emitted
     * once per call (idempotent at the receipt-id level once persisted via a durable emitter).
     */
    public Decision score(LoanApplication app, int creditScore, int monthsHistory) {
        List<String> reasons = new ArrayList<>();
        Outcome outcome;

        if (creditScore < SCORE_HARD_DECLINE) {
            outcome = Outcome.DECLINE;
            reasons.add("credit score " + creditScore + " below " + SCORE_HARD_DECLINE);
        } else if (creditScore >= SCORE_APPROVE_MIN
                && monthsHistory >= MIN_HISTORY_MONTHS
                && app.amountCents() * 5 <= app.annualIncomeCents() * 2) {
            outcome = Outcome.APPROVE;
            reasons.add("credit score " + creditScore + " meets threshold of " + SCORE_APPROVE_MIN);
            reasons.add("account history " + monthsHistory + " months meets minimum of " + MIN_HISTORY_MONTHS);
            reasons.add("loan amount within 40% of annual income");
        } else {
            outcome = Outcome.REFER;
            if (creditScore < SCORE_APPROVE_MIN) {
                reasons.add("credit score " + creditScore + " is borderline (below " + SCORE_APPROVE_MIN + ")");
            }
            if (monthsHistory < MIN_HISTORY_MONTHS) {
                reasons.add("thin history: " + monthsHistory + " months (minimum " + MIN_HISTORY_MONTHS + ")");
            }
            if (app.amountCents() * 5 > app.annualIncomeCents() * 2) {
                reasons.add("high amount-to-income: loan " + app.amountCents()
                        + " exceeds 40% of annual income " + app.annualIncomeCents());
            }
        }

        Decision decision = new Decision(app.id(), outcome, creditScore, reasons);
        emitter.emit(factory.forToolCall(
            app.id(),
            TOOL_SCORE,
            "{\"creditScore\":" + creditScore + ",\"monthsHistory\":" + monthsHistory + "}",
            decision.outcome().name()));
        return decision;
    }
}
