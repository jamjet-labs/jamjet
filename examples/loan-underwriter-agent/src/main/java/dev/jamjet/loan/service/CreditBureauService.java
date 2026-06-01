package dev.jamjet.loan.service;

import dev.jamjet.loan.domain.LoanApplication;

public interface CreditBureauService {
    int score(LoanApplication app);
}
