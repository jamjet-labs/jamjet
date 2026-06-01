package dev.jamjet.loan.domain;

import java.util.List;

public record Decision(
        String applicationId,
        Outcome outcome,
        int riskScore,
        List<String> reasons
) {
    public Decision {
        reasons = List.copyOf(reasons);
    }

    public enum Outcome {
        APPROVE, DECLINE, REFER
    }
}
