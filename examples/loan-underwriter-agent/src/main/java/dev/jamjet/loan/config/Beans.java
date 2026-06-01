package dev.jamjet.loan.config;

import dev.jamjet.loan.approval.ApprovalGate;
import dev.jamjet.loan.durable.CheckpointStore;
import dev.jamjet.loan.durable.UnderwritingAgent;
import dev.jamjet.loan.durable.UnderwritingRunner;
import dev.jamjet.loan.receipt.DurableReceiptEmitter;
import dev.jamjet.loan.receipt.ReceiptFactory;
import dev.jamjet.loan.service.AccountHistoryService;
import dev.jamjet.loan.service.CreditBureauService;
import dev.jamjet.loan.service.DisbursementService;
import dev.jamjet.loan.service.MockAccountHistory;
import dev.jamjet.loan.service.MockCreditBureau;
import dev.jamjet.loan.service.MockDisbursement;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

import java.nio.file.Path;

@Configuration
public class Beans {

    @Bean
    public CreditBureauService creditBureauService() {
        return new MockCreditBureau();
    }

    @Bean
    public AccountHistoryService accountHistoryService() {
        return new MockAccountHistory();
    }

    @Bean
    public DisbursementService disbursementService() {
        return new MockDisbursement();
    }

    /**
     * Single shared, disk-backed emitter. Both UnderwritingAgent and UnderwritingRunner receive
     * this same instance so that all receipts (credit, history, score, disbursement) for a given
     * application persist under one directory and reload into one bundle. Persisting to disk is
     * what makes the audit bundle survive a process crash: an in-memory emitter would lose every
     * pre-crash receipt.
     */
    @Bean
    public DurableReceiptEmitter durableReceiptEmitter(
            @Value("${loan.receipts-dir}") String receiptsDir) {
        return new DurableReceiptEmitter(Path.of(receiptsDir));
    }

    /**
     * Single shared factory. Same instance injected into both agent and runner so that
     * receipts carry a consistent system/environment label.
     */
    @Bean
    public ReceiptFactory receiptFactory(
            @Value("${spring.application.name:loan-underwriter}") String system,
            @Value("${loan.environment:dev}") String environment) {
        return new ReceiptFactory(system, environment);
    }

    @Bean
    public CheckpointStore checkpointStore(
            @Value("${loan.checkpoint-dir}") String checkpointDir) {
        return new CheckpointStore(Path.of(checkpointDir));
    }

    @Bean
    public ApprovalGate approvalGate(
            @Value("${loan.approval-dir}") String approvalDir) {
        return new ApprovalGate(Path.of(approvalDir));
    }

    @Bean
    public UnderwritingAgent underwritingAgent(
            CreditBureauService creditBureauService,
            AccountHistoryService accountHistoryService,
            DurableReceiptEmitter durableReceiptEmitter,
            ReceiptFactory receiptFactory) {
        return new UnderwritingAgent(creditBureauService, accountHistoryService,
                durableReceiptEmitter, receiptFactory);
    }

    @Bean
    public UnderwritingRunner underwritingRunner(
            UnderwritingAgent underwritingAgent,
            CheckpointStore checkpointStore,
            ApprovalGate approvalGate,
            DisbursementService disbursementService,
            DurableReceiptEmitter durableReceiptEmitter,
            ReceiptFactory receiptFactory) {
        return new UnderwritingRunner(underwritingAgent, checkpointStore, approvalGate,
                disbursementService, durableReceiptEmitter, receiptFactory);
    }
}
