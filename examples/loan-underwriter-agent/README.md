# loan-underwriter-agent

A durable, auditable loan-underwriting agent on the JVM. Built on Spring Boot, the JamJet Java runtime, and AgentBoundary action receipts. Where Spring AI and LangChain4j stop (at stateless request/response), this demo picks up: every underwriting run checkpoints its steps to disk, survives a hard process kill, resumes without repeating work, requires a human officer approval before disbursement, and produces a verifiable signed receipt bundle that proves exactly what the agent did and who approved it.

## Prerequisites

- JDK 21 (the `dev.jamjet:*:0.3.1` artifacts are Java-21 bytecode)
- Maven 3.9+

Point `JAVA_HOME` at a JDK 21 install before building or running. For example:

```bash
# macOS + Homebrew:  brew install openjdk@21 && export JAVA_HOME="$(/usr/libexec/java_home -v 21)"
# SDKMAN (any OS):   sdk install java 21-tem  && export JAVA_HOME="$(sdk home java 21-tem)"
export JAVA_HOME=/path/to/your/jdk-21
```

## Run it

```bash
JAVA_HOME=$JAVA_HOME mvn spring-boot:run
```

The app starts on port 8080. Three commands cover the full lifecycle:

**1. Start an underwriting run**

```bash
curl -s -X POST http://localhost:8080/applications \
  -H "Content-Type: application/json" \
  -d '{"id":"loan-demo","applicantName":"Jane Smith","amountCents":1500000,"annualIncomeCents":9000000}'
```

Expected response (202 Accepted):

```json
{"applicationId":"loan-demo","state":"AWAITING_APPROVAL"}
```

**2. Approve (or reject): human-in-the-loop gate**

```bash
curl -s -X POST http://localhost:8080/applications/loan-demo/approve \
  -H "Content-Type: application/json" \
  -d '{"userId":"officer@bank","decision":"approved","comment":"within risk tolerance"}'
```

Expected response (200 OK):

```json
{"applicationId":"loan-demo","state":"COMPLETED"}
```

`decision` accepts `approved`, `rejected`, or `escalate`.

**3. Get the audit bundle**

```bash
curl -s http://localhost:8080/applications/loan-demo/receipts | python3 -m json.tool
```

Expected response (200 OK):

```json
{
  "applicationId": "loan-demo",
  "verified": true,
  "receipts": [ ... 4 receipts ... ]
}
```

`verified: true` means every receipt hash in the bundle checks out. Mutating any receipt field causes the hash check to fail. On the approve-and-disburse path the bundle contains 4 receipts (credit bureau, account history, underwriting score, and disbursement). A declined or rejected run produces fewer receipts because no disbursement receipt is emitted.

## The crash demo

```bash
bash scripts/demo.sh
```

The script:

1. Builds the jar once (`mvn -DskipTests package`).
2. Starts the app, submits application `loan-demo`, and waits until the checkpoint file appears on disk.
3. Sends `kill -9` to the app process, a hard kill with no graceful shutdown.
4. Restarts the app against the same state directories.
5. Re-submits `loan-demo`. Because `CheckpointStore` still holds the `credit` and `history` checkpoints from the first run, the agent replays from disk and returns `AWAITING_APPROVAL` without calling the credit bureau again.
6. Posts an officer approval; disbursement completes.
7. Fetches the receipt bundle and confirms `verified: true` with four receipts.

If `$JAVA_HOME` is not set, the script looks for the openjdk@21 Homebrew path automatically. If Java 21 is not found it exits early with instructions.

## How it works

| Guarantee | Class |
|---|---|
| Durability: each step is checkpointed to disk | `CheckpointStore` + `@DurableAgent` / `DurabilityContext` |
| Resume: replay skips completed steps on restart | `CheckpointStore.load()` sets replay mode before re-running |
| Signed receipts + verifiable audit bundle | `ReceiptFactory` (builds `ActionReceipt` with `receipt_hash`) / `AuditBundle.verify()` |
| Human-in-the-loop approval gate | `ApprovalGate` (disk-backed, survives restart) |
| Approval stamped on disbursement receipt | `UnderwritingRunner.resume()` calls `approvalGate.toApprovalBlock()` |

The project depends on the published `dev.jamjet:*:0.3.1` Maven artifacts (JamJet Java runtime + AgentBoundary SDK). To wire a real LLM: add a Spring AI model starter, set `loan.llm.enabled=true`, and point a `ChatClient` at the underwriting logic. The `ActionReceiptAdvisor` (from `jamjet-cloud-spring-boot-starter`) will emit receipts automatically for every model call, routed through the same `DurableReceiptEmitter`.
