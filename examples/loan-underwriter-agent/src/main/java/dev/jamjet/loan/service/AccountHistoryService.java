package dev.jamjet.loan.service;

import dev.jamjet.loan.domain.LoanApplication;

public interface AccountHistoryService {
    int monthsHistory(LoanApplication app);
}
