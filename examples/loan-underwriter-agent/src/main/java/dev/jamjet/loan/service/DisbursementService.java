package dev.jamjet.loan.service;

public interface DisbursementService {
    String disburse(String applicationId, long amountCents);
}
