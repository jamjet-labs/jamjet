package dev.jamjet.loan.service;

import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;

public class MockDisbursement implements DisbursementService {
    private final ConcurrentHashMap<String, String> ledger = new ConcurrentHashMap<>();
    private final AtomicLong counter = new AtomicLong();

    @Override
    public String disburse(String applicationId, long amountCents) {
        return ledger.computeIfAbsent(applicationId,
                id -> "REF-" + id + "-" + counter.incrementAndGet());
    }
}
