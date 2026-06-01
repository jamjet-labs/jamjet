package dev.jamjet.loan.service;

import dev.jamjet.loan.domain.LoanApplication;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

public class MockCreditBureau implements CreditBureauService {
    private static final Logger log = LoggerFactory.getLogger(MockCreditBureau.class);

    private static final int MIN_SCORE = 300;
    private static final int SCORE_SPAN = 551; // count of distinct scores from 300..850 inclusive

    @Override
    public int score(LoanApplication app) {
        log.info("credit bureau CALLED for application {}", app.id());
        // Deterministic stub: maps the application id into the FICO range [300, 850].
        return MIN_SCORE + Math.floorMod(app.id().hashCode(), SCORE_SPAN);
    }
}
