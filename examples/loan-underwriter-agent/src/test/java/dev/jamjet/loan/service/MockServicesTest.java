package dev.jamjet.loan.service;

import dev.jamjet.loan.domain.LoanApplication;
import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

class MockServicesTest {
    @Test
    void creditScoreIsDeterministicPerApplicant() {
        var svc = new MockCreditBureau();
        var app = new LoanApplication("app-1", "Ada Lovelace", 25_000, 60_000);
        int first = svc.score(app);
        assertEquals(first, svc.score(app), "same applicant -> same score");
        assertTrue(first >= 300 && first <= 850, "score in FICO range, was " + first);
    }

    @Test
    void accountHistoryIsDeterministicPerApplicant() {
        var svc = new MockAccountHistory();
        var app = new dev.jamjet.loan.domain.LoanApplication("app-1", "Ada", 25_000, 60_000);
        int first = svc.monthsHistory(app);
        assertEquals(first, svc.monthsHistory(app), "same applicant -> same history");
        assertTrue(first >= 0 && first < 120, "history in [0,120) months, was " + first);
    }

    /**
     * Regression guard: the demo and e2e applicant ids must produce a non-DECLINE credit score
     * (>= 580) so the demo exercises the full approval-to-disbursement money-shot path.
     *
     * <p>Verified by formula: credit = 300 + Math.floorMod(id.hashCode(), 551).
     * <ul>
     *   <li>"loan-demo": floorMod = 531, credit = 831 (APPROVE)</li>
     *   <li>"loan-e2e":  floorMod = 411, credit = 711 (APPROVE)</li>
     *   <li>"app-ok":    floorMod = 108, credit = 408 (DECLINE) -- used in unit test only</li>
     * </ul>
     */
    @Test
    void demoAndE2eApplicantsHaveNonDeclineCreditScores() {
        var bureau = new MockCreditBureau();
        var appDemo = new dev.jamjet.loan.domain.LoanApplication("loan-demo", "Jane Smith", 1_500_000, 9_000_000);
        var appE2e  = new dev.jamjet.loan.domain.LoanApplication("loan-e2e",  "Ada",        20_000,    90_000);
        var appDecline = new dev.jamjet.loan.domain.LoanApplication("app-ok", "Bob", 20_000, 90_000);

        assertTrue(bureau.score(appDemo) >= 580,
                "loan-demo must not be a hard-decline; score was " + bureau.score(appDemo));
        assertTrue(bureau.score(appE2e) >= 580,
                "loan-e2e must not be a hard-decline; score was " + bureau.score(appE2e));
        assertTrue(bureau.score(appDecline) < 580,
                "app-ok is used as the decline test case and must be below 580; score was " + bureau.score(appDecline));
    }

    @Test
    void disbursementIsIdempotentByApplicationId() {
        var svc = new MockDisbursement();
        var ref1 = svc.disburse("app-1", 25_000);
        var ref2 = svc.disburse("app-1", 25_000);
        assertEquals(ref1, ref2, "same application id must not double-disburse");
    }
}
