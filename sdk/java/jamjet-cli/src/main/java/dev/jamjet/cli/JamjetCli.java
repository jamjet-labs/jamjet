package dev.jamjet.cli;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.SerializationFeature;
import dev.jamjet.JamjetClient;
import dev.jamjet.client.ClientConfig;
import picocli.CommandLine;
import picocli.CommandLine.Command;
import picocli.CommandLine.Option;
import picocli.CommandLine.Parameters;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;
import java.util.concurrent.Callable;

/**
 * JamJet CLI — interact with the JamJet runtime from the command line.
 *
 * <p>Usage:
 * <pre>
 * jamjet health                       # check runtime health
 * jamjet run &lt;workflow-id&gt; [input.json]  # start a workflow execution
 * jamjet validate &lt;workflow.json&gt;     # validate a workflow IR file
 * jamjet dev                          # print dev server instructions
 * </pre>
 */
@Command(
        name = "jamjet",
        mixinStandardHelpOptions = true,
        version = "0.1.0",
        description = "JamJet runtime CLI",
        subcommands = {
                JamjetCli.HealthCommand.class,
                JamjetCli.RunCommand.class,
                JamjetCli.ValidateCommand.class,
                JamjetCli.DevCommand.class,
                JamjetCli.AgentsCommand.class,
                JamjetCli.ExecutionsCommand.class,
        }
)
public class JamjetCli implements Callable<Integer> {

    @Option(names = {"-u", "--url"}, description = "Runtime base URL", defaultValue = "http://localhost:7700")
    String url;

    @Option(names = {"-t", "--token"}, description = "API token (overrides JAMJET_TOKEN env)")
    String token;

    private static final ObjectMapper MAPPER = new ObjectMapper()
            .enable(SerializationFeature.INDENT_OUTPUT);

    @Override
    public Integer call() {
        CommandLine.usage(this, System.out);
        return 0;
    }

    /** Build a {@link JamjetClient} from CLI options. */
    JamjetClient buildClient() {
        var config = ClientConfig.builder()
                .baseUrl(url)
                .apiToken(token)
                .build();
        return new JamjetClient(config);
    }

    static void printJson(Object obj) {
        try {
            System.out.println(MAPPER.writeValueAsString(obj));
        } catch (Exception e) {
            System.err.println("Failed to serialize response: " + e.getMessage());
        }
    }

    // ── Subcommands ───────────────────────────────────────────────────────────

    @Command(name = "health", description = "Check runtime health status")
    static class HealthCommand implements Callable<Integer> {

        @CommandLine.ParentCommand
        JamjetCli parent;

        @Override
        public Integer call() {
            try (var client = parent.buildClient()) {
                var health = client.health();
                printJson(health);
                return 0;
            } catch (Exception e) {
                System.err.println("Health check failed: " + e.getMessage());
                return 1;
            }
        }
    }

    @Command(name = "run", description = "Start a workflow execution")
    static class RunCommand implements Callable<Integer> {

        @CommandLine.ParentCommand
        JamjetCli parent;

        @Parameters(index = "0", description = "Workflow ID")
        String workflowId;

        @Parameters(index = "1", arity = "0..1", description = "Path to input JSON file")
        Path inputFile;

        @Override
        public Integer call() {
            try (var client = parent.buildClient()) {
                Map<String, Object> input;
                if (inputFile != null) {
                    var content = Files.readString(inputFile);
                    //noinspection unchecked
                    input = MAPPER.readValue(content, Map.class);
                } else {
                    input = Map.of();
                }
                var result = client.startExecution(workflowId, input);
                printJson(result);
                return 0;
            } catch (Exception e) {
                System.err.println("Run failed: " + e.getMessage());
                return 1;
            }
        }
    }

    @Command(name = "validate", description = "Validate a workflow IR JSON file")
    static class ValidateCommand implements Callable<Integer> {

        @Parameters(index = "0", description = "Path to workflow IR JSON file")
        Path workflowFile;

        @Override
        public Integer call() {
            try {
                var content = Files.readString(workflowFile);
                var parsed = MAPPER.readTree(content);

                // Basic structural validation
                var errors = new java.util.ArrayList<String>();
                if (!parsed.has("workflow_id") && !parsed.has("id")) {
                    errors.add("Missing 'workflow_id' field");
                }
                if (!parsed.has("start_node")) {
                    errors.add("Missing 'start_node' field");
                }
                if (!parsed.has("nodes")) {
                    errors.add("Missing 'nodes' field");
                }
                if (!parsed.has("edges")) {
                    errors.add("Missing 'edges' field");
                }

                if (errors.isEmpty()) {
                    System.out.println("✓ Workflow IR is valid");
                    var workflowId = parsed.has("workflow_id")
                            ? parsed.get("workflow_id").asText()
                            : parsed.get("id").asText();
                    System.out.println("  id:         " + workflowId);
                    System.out.println("  nodes:      " + parsed.get("nodes").size());
                    System.out.println("  edges:      " + parsed.get("edges").size());
                    System.out.println("  start_node: " + parsed.get("start_node").asText());
                    return 0;
                } else {
                    System.err.println("✗ Validation errors:");
                    for (var err : errors) {
                        System.err.println("  - " + err);
                    }
                    return 1;
                }
            } catch (Exception e) {
                System.err.println("Validation failed: " + e.getMessage());
                return 1;
            }
        }
    }

    @Command(name = "dev", description = "Print development server instructions")
    static class DevCommand implements Callable<Integer> {

        @Override
        public Integer call() {
            System.out.println("""
                    JamJet Development Server
                    ─────────────────────────
                    Start the runtime:
                      cargo run --bin jamjet-server

                    Or with Docker:
                      docker run -p 7700:7700 jamjet/runtime:latest

                    Environment variables:
                      JAMJET_TOKEN      — API authentication token
                      OPENAI_API_KEY    — OpenAI-compatible API key for Agent.run()
                      OPENAI_BASE_URL   — Override base URL (default: https://api.openai.com/v1)

                    Health check:
                      jamjet health

                    Docs: https://jamjet.dev/docs
                    """);
            return 0;
        }
    }

    @Command(name = "agents", description = "Manage agents", subcommands = {
            AgentsCommand.ListAgents.class,
            AgentsCommand.GetAgent.class,
    })
    static class AgentsCommand implements Callable<Integer> {

        @CommandLine.ParentCommand
        JamjetCli parent;

        @Override
        public Integer call() {
            CommandLine.usage(this, System.out);
            return 0;
        }

        @Command(name = "list", description = "List registered agents")
        static class ListAgents implements Callable<Integer> {
            @CommandLine.ParentCommand
            AgentsCommand agentsCmd;

            @Override
            public Integer call() {
                try (var client = agentsCmd.parent.buildClient()) {
                    printJson(client.listAgents());
                    return 0;
                } catch (Exception e) {
                    System.err.println("Failed: " + e.getMessage());
                    return 1;
                }
            }
        }

        @Command(name = "get", description = "Get agent details")
        static class GetAgent implements Callable<Integer> {
            @CommandLine.ParentCommand
            AgentsCommand agentsCmd;

            @Parameters(index = "0", description = "Agent ID")
            String agentId;

            @Override
            public Integer call() {
                try (var client = agentsCmd.parent.buildClient()) {
                    printJson(client.getAgent(agentId));
                    return 0;
                } catch (Exception e) {
                    System.err.println("Failed: " + e.getMessage());
                    return 1;
                }
            }
        }
    }

    @Command(name = "executions", description = "Manage workflow executions", subcommands = {
            ExecutionsCommand.ListExecutions.class,
            ExecutionsCommand.GetExecution.class,
            ExecutionsCommand.CancelExecution.class,
    })
    static class ExecutionsCommand implements Callable<Integer> {

        @CommandLine.ParentCommand
        JamjetCli parent;

        @Override
        public Integer call() {
            CommandLine.usage(this, System.out);
            return 0;
        }

        @Command(name = "list", description = "List executions")
        static class ListExecutions implements Callable<Integer> {
            @CommandLine.ParentCommand
            ExecutionsCommand execCmd;

            @Option(names = "--status", description = "Filter by status")
            String status;

            @Option(names = "--limit", defaultValue = "20")
            int limit;

            @Option(names = "--offset", defaultValue = "0")
            int offset;

            @Override
            public Integer call() {
                try (var client = execCmd.parent.buildClient()) {
                    printJson(client.listExecutions(status, limit, offset));
                    return 0;
                } catch (Exception e) {
                    System.err.println("Failed: " + e.getMessage());
                    return 1;
                }
            }
        }

        @Command(name = "get", description = "Get execution details")
        static class GetExecution implements Callable<Integer> {
            @CommandLine.ParentCommand
            ExecutionsCommand execCmd;

            @Parameters(index = "0", description = "Execution ID")
            String executionId;

            @Override
            public Integer call() {
                try (var client = execCmd.parent.buildClient()) {
                    printJson(client.getExecution(executionId));
                    return 0;
                } catch (Exception e) {
                    System.err.println("Failed: " + e.getMessage());
                    return 1;
                }
            }
        }

        @Command(name = "cancel", description = "Cancel a running execution")
        static class CancelExecution implements Callable<Integer> {
            @CommandLine.ParentCommand
            ExecutionsCommand execCmd;

            @Parameters(index = "0", description = "Execution ID")
            String executionId;

            @Override
            public Integer call() {
                try (var client = execCmd.parent.buildClient()) {
                    printJson(client.cancelExecution(executionId));
                    return 0;
                } catch (Exception e) {
                    System.err.println("Failed: " + e.getMessage());
                    return 1;
                }
            }
        }
    }

    // ── Entry point ───────────────────────────────────────────────────────────

    public static void main(String[] args) {
        var exitCode = new CommandLine(new JamjetCli()).execute(args);
        System.exit(exitCode);
    }
}
