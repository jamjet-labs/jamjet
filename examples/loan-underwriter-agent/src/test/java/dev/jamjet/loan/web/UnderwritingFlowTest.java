package dev.jamjet.loan.web;

import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.autoconfigure.web.servlet.AutoConfigureMockMvc;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.http.MediaType;
import org.springframework.test.context.DynamicPropertyRegistry;
import org.springframework.test.context.DynamicPropertySource;
import org.springframework.test.web.servlet.MockMvc;
import java.io.IOException;
import java.nio.file.Files;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.*;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.*;

@SpringBootTest
@AutoConfigureMockMvc
class UnderwritingFlowTest {
    @Autowired MockMvc mvc;

    @DynamicPropertySource
    static void tempDirs(DynamicPropertyRegistry registry) throws IOException {
        registry.add("loan.checkpoint-dir", () -> createTmp("ck"));
        registry.add("loan.approval-dir", () -> createTmp("ap"));
        registry.add("loan.receipts-dir", () -> createTmp("rc"));
    }
    private static String createTmp(String prefix) {
        try { return Files.createTempDirectory(prefix).toString(); }
        catch (IOException e) { throw new RuntimeException(e); }
    }

    @Test
    void startThenApproveThenVerifiableBundle() throws Exception {
        // "loan-e2e": Math.floorMod("loan-e2e".hashCode(), 551) = 411 -> credit = 711 (APPROVE).
        // history = Math.floorMod("loan-e2e".hashCode(), 120) = 83 months. Amount*5=100000 <= income*2=180000 OK.

        // 1. Start a run -> suspends at the approval gate.
        mvc.perform(post("/applications").contentType(MediaType.APPLICATION_JSON)
                .content("{\"id\":\"loan-e2e\",\"applicantName\":\"Ada\",\"amountCents\":20000,\"annualIncomeCents\":90000}"))
            .andExpect(status().isAccepted())
            .andExpect(jsonPath("$.state").value("AWAITING_APPROVAL"));

        // 2. Approve -> disbursement runs.
        mvc.perform(post("/applications/loan-e2e/approve").contentType(MediaType.APPLICATION_JSON)
                .content("{\"userId\":\"officer@bank\",\"decision\":\"approved\",\"comment\":\"ok\"}"))
            .andExpect(status().isOk())
            .andExpect(jsonPath("$.state").value("COMPLETED"));

        // 3. Audit bundle is present and verifies.
        mvc.perform(get("/applications/loan-e2e/receipts"))
            .andExpect(status().isOk())
            .andExpect(jsonPath("$.verified").value(true))
            .andExpect(jsonPath("$.receipts.length()").value(org.hamcrest.Matchers.greaterThanOrEqualTo(3)));
    }

    @Test
    void unknownDecisionReturns400() throws Exception {
        // start a run so the id exists
        mvc.perform(post("/applications").contentType(MediaType.APPLICATION_JSON)
                .content("{\"id\":\"app-400\",\"applicantName\":\"Bob\",\"amountCents\":1000,\"annualIncomeCents\":50000}"))
            .andExpect(status().isAccepted());
        // a bogus decision string must be a 400, not a 500
        mvc.perform(post("/applications/app-400/approve").contentType(MediaType.APPLICATION_JSON)
                .content("{\"userId\":\"x\",\"decision\":\"YES_PLEASE\",\"comment\":\"\"}"))
            .andExpect(status().isBadRequest());
    }
}
