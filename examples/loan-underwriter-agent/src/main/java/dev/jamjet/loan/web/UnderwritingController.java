package dev.jamjet.loan.web;

import dev.jamjet.loan.approval.ApprovalGate;
import dev.jamjet.loan.domain.LoanApplication;
import dev.jamjet.loan.domain.RunState;
import dev.jamjet.loan.durable.UnderwritingRunner;
import dev.jamjet.loan.receipt.AuditBundle;
import dev.jamjet.loan.receipt.DurableReceiptEmitter;
import dev.jamjet.runtime.core.event.ApprovalDecision;
import org.springframework.http.HttpStatus;
import org.springframework.web.bind.annotation.ExceptionHandler;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.PathVariable;
import org.springframework.web.bind.annotation.PostMapping;
import org.springframework.web.bind.annotation.RequestBody;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.ResponseStatus;
import org.springframework.web.bind.annotation.RestController;
import org.springframework.web.server.ResponseStatusException;

import java.util.LinkedHashMap;
import java.util.Map;

@RestController
@RequestMapping("/applications")
public class UnderwritingController {

    private final UnderwritingRunner runner;
    private final ApprovalGate approvalGate;
    private final DurableReceiptEmitter emitter;

    public UnderwritingController(
            UnderwritingRunner runner,
            ApprovalGate approvalGate,
            DurableReceiptEmitter emitter) {
        this.runner = runner;
        this.approvalGate = approvalGate;
        this.emitter = emitter;
    }

    /**
     * Start an underwriting run. The run suspends at the approval gate.
     * Returns 202 Accepted with the current state (AWAITING_APPROVAL).
     */
    @PostMapping
    @ResponseStatus(HttpStatus.ACCEPTED)
    public Map<String, Object> startApplication(@RequestBody LoanApplication app) {
        RunState state = runner.start(app);
        return Map.of(
                "applicationId", app.id(),
                "state", state.name()
        );
    }

    /**
     * Record a human decision and, if approved, disburse.
     * Returns 200 OK with the resulting state (COMPLETED or FAILED).
     */
    @PostMapping("/{id}/approve")
    public Map<String, Object> approve(
            @PathVariable("id") String id,
            @RequestBody ApproveRequest body) {
        if (body.decision() == null || body.decision().isBlank()) {
            throw new ResponseStatusException(HttpStatus.BAD_REQUEST,
                    "decision is required (approved|rejected|escalate)");
        }
        ApprovalDecision decision;
        try {
            decision = ApprovalDecision.fromValue(body.decision());
        } catch (IllegalArgumentException e) {
            throw new ResponseStatusException(HttpStatus.BAD_REQUEST,
                    "unknown decision: " + body.decision() + " (expected approved|rejected|escalate)");
        }
        approvalGate.decide(id, body.userId(), decision, body.comment());
        RunState state = runner.resume(id);
        return Map.of(
                "applicationId", id,
                "state", state.name()
        );
    }

    @ExceptionHandler(IllegalStateException.class)
    @ResponseStatus(HttpStatus.NOT_FOUND)
    public Map<String, Object> handleUnknownRun(IllegalStateException ex) {
        return Map.of("error", ex.getMessage());
    }

    /**
     * Return the audit bundle for an application: verified flag and the full receipt list.
     * Returns 200 OK. Each receipt is serialized via Jackson using the @JsonProperty
     * annotations on the ActionReceipt record and its sub-records.
     */
    @GetMapping("/{id}/receipts")
    public Map<String, Object> receipts(@PathVariable("id") String id) {
        AuditBundle bundle = emitter.bundleFor(id);
        Map<String, Object> response = new LinkedHashMap<>();
        response.put("applicationId", id);
        response.put("verified", bundle.verify());
        response.put("receipts", bundle.receipts());
        return response;
    }

    /** Request body for the approve endpoint. */
    record ApproveRequest(String userId, String decision, String comment) {}
}
