package dev.jamjet.loan.domain;

public record LoanApplication(
        String id,
        String applicantName,
        long amountCents,
        long annualIncomeCents
) {}
