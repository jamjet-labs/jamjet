package dev.jamjet.cli;

import com.sun.net.httpserver.HttpServer;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import picocli.CommandLine;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.PrintStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Runs the CLI against a stub runtime and checks what the user sees. */
class JamjetCliTest {

    private HttpServer server;
    private String url;
    private volatile int replyStatus;
    private volatile String replyBody;

    private final ByteArrayOutputStream out = new ByteArrayOutputStream();
    private final ByteArrayOutputStream err = new ByteArrayOutputStream();
    private PrintStream oldOut;
    private PrintStream oldErr;

    @BeforeEach
    void start() throws IOException {
        server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/", exchange -> {
            exchange.getRequestBody().readAllBytes();
            var bytes = replyBody.getBytes(StandardCharsets.UTF_8);
            exchange.getResponseHeaders().add("Content-Type", "application/json");
            exchange.sendResponseHeaders(replyStatus, bytes.length == 0 ? -1 : bytes.length);
            if (bytes.length > 0) exchange.getResponseBody().write(bytes);
            exchange.close();
        });
        server.start();
        url = "http://127.0.0.1:" + server.getAddress().getPort();
        oldOut = System.out;
        oldErr = System.err;
        System.setOut(new PrintStream(out, true, StandardCharsets.UTF_8));
        System.setErr(new PrintStream(err, true, StandardCharsets.UTF_8));
    }

    @AfterEach
    void stop() {
        System.setOut(oldOut);
        System.setErr(oldErr);
        server.stop(0);
    }

    private int run(String... args) {
        return new CommandLine(new JamjetCli()).execute(args);
    }

    private String stderr() {
        return err.toString(StandardCharsets.UTF_8);
    }

    @Test
    void runPrintsTheRuntimeReasonOn404() {
        replyStatus = 404;
        replyBody = "{\"error\":\"workflow research v1.0.0\"}";
        assertEquals(1, run("-u", url, "run", "research"));
        assertTrue(stderr().contains("workflow research v1.0.0"), stderr());
    }

    @Test
    void cancelPrintsTheRuntimeReasonOn409() {
        replyStatus = 409;
        replyBody = "{\"error\":\"execution exec_1 is already in terminal state: Cancelled\"}";
        assertEquals(1, run("-u", url, "executions", "cancel", "exec_1"));
        assertTrue(stderr().contains("already in terminal state: Cancelled"), stderr());
    }
}
