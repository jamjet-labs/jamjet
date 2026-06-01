package dev.jamjet.loan.service;

import dev.jamjet.loan.domain.LoanApplication;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

public class MockAccountHistory implements AccountHistoryService {
    private static final Logger log = LoggerFactory.getLogger(MockAccountHistory.class);

    private static final int MAX_HISTORY_MONTHS = 120; // 10 years

    @Override
    public int monthsHistory(LoanApplication app) {
        log.info("account history CALLED for application {}", app.id());
        // Deterministic stub: maps the application id into a months-of-history range [0, 120).
        return Math.floorMod(app.id().hashCode(), MAX_HISTORY_MONTHS);
    }
}
