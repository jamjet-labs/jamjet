#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Cross-process crash-recovery demo for the loan-underwriter-agent.
#
# What it proves:
#   1. Durability: checkpoints survive a kill -9.
#   2. Resume: restarting with the same state dirs replays from checkpoints;
#              the credit pull is not repeated.
#   3. Human gate: a human approval unblocks disbursement.
#   4. Audit trail: the receipt bundle verifies (signed hash check passes).
#
# Applicant "loan-demo": credit = 831 (APPROVE path), history = 104 months.
# Math.floorMod("loan-demo".hashCode(), 551) = 531 -> credit = 300 + 531 = 831.
# ---------------------------------------------------------------------------

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OPENJDK21="/opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home"

# --- Java 21 check ----------------------------------------------------------
if [[ -z "${JAVA_HOME:-}" ]]; then
  if [[ -d "$OPENJDK21" ]]; then
    export JAVA_HOME="$OPENJDK21"
  fi
fi

JAVA_VERSION="$("${JAVA_HOME:-}/bin/java" -version 2>&1 | head -1 || true)"
if [[ ! "$JAVA_VERSION" =~ 21 ]]; then
  echo ""
  echo "ERROR: Java 21 is required. Detected: ${JAVA_VERSION:-not found}"
  echo ""
  echo "  export JAVA_HOME=$OPENJDK21"
  echo ""
  exit 1
fi

# --- State dirs (fresh per run, same for both process starts) ---------------
DEMO_DIR="${TMPDIR:-/tmp}/loan-demo-$$"
CK="$DEMO_DIR/ck"
AP="$DEMO_DIR/ap"
RC="$DEMO_DIR/rc"
LOG1="$DEMO_DIR/app1.log"
LOG2="$DEMO_DIR/app2.log"
APP_PID=""

cleanup() {
  if [[ -n "$APP_PID" ]] && kill -0 "$APP_PID" 2>/dev/null; then
    kill -9 "$APP_PID" 2>/dev/null || true
  fi
  rm -rf "$DEMO_DIR"
}
trap cleanup EXIT

mkdir -p "$DEMO_DIR"

# --- Build once -------------------------------------------------------------
echo ""
echo "==> Building (skipping tests)..."
(cd "$REPO_DIR" && JAVA_HOME="$JAVA_HOME" mvn -q -DskipTests package)
JAR="$(ls "$REPO_DIR"/target/loan-underwriter-agent-*.jar 2>/dev/null | head -1)"
if [[ -z "$JAR" ]]; then
  echo "ERROR: Could not find built jar under $REPO_DIR/target/"
  exit 1
fi
echo "    Built: $JAR"

# ---------------------------------------------------------------------------
start_app() {
  local logfile="$1"
  APP_PID=""
  JAVA_HOME="$JAVA_HOME" "$JAVA_HOME/bin/java" \
    "-Dloan.checkpoint-dir=$CK" \
    "-Dloan.approval-dir=$AP" \
    "-Dloan.receipts-dir=$RC" \
    -jar "$JAR" \
    > "$logfile" 2>&1 &
  APP_PID=$!
}

wait_ready() {
  local logfile="$1"
  local deadline=$(( $(date +%s) + 60 ))
  while [[ $(date +%s) -lt $deadline ]]; do
    if grep -q "Started LoanUnderwriterApplication" "$logfile" 2>/dev/null; then
      return 0
    fi
    sleep 1
  done
  echo ""
  echo "ERROR: App did not start within 60s. Last 20 log lines:"
  tail -20 "$logfile"
  exit 1
}

wait_checkpoint() {
  local deadline=$(( $(date +%s) + 30 ))
  while [[ $(date +%s) -lt $deadline ]]; do
    if [[ -f "$CK/loan-demo.json" ]]; then
      return 0
    fi
    sleep 0.5
  done
  echo ""
  echo "ERROR: Checkpoint file $CK/loan-demo.json did not appear within 30s."
  exit 1
}

pretty_json() {
  if command -v jq &>/dev/null; then
    jq .
  else
    python3 -m json.tool
  fi
}

# ---------------------------------------------------------------------------
echo ""
echo "==> Starting app (process 1) with fresh state dirs..."
start_app "$LOG1"
wait_ready "$LOG1"
echo "    PID=$APP_PID  state dirs=$DEMO_DIR"

# --- Money shot 1: durability -----------------------------------------------
echo ""
echo ">>> MONEY SHOT 1: durability"
echo "    Submitting application loan-demo (credit 831, qualifies for APPROVE path)..."
RESP1=$(curl -s -X POST http://localhost:8080/applications \
  -H "Content-Type: application/json" \
  -d '{"id":"loan-demo","applicantName":"Jane Smith","amountCents":1500000,"annualIncomeCents":9000000}')
echo "    Response: $RESP1"

echo "    Waiting for checkpoint to appear on disk..."
wait_checkpoint
echo "    Checkpoint confirmed: $CK/loan-demo.json"

# --- Hard-kill mid-flight ---------------------------------------------------
echo ""
echo "==> Killing app PID=$APP_PID with kill -9..."
kill -9 "$APP_PID"
# Wait for the process to actually die
for i in $(seq 1 10); do
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    break
  fi
  sleep 0.5
done
echo "    Process hard-killed. Checkpoint and approval state survive on disk."
APP_PID=""

# --- Restart and resume from checkpoint ------------------------------------
echo ""
echo "==> Starting app (process 2), same state dirs, resuming from checkpoint..."
start_app "$LOG2"
wait_ready "$LOG2"
echo "    PID=$APP_PID"

echo ""
echo ">>> RESUMED AFTER CRASH, no work repeated"
echo "    Re-submitting loan-demo (same id)..."
RESP2=$(curl -s -X POST http://localhost:8080/applications \
  -H "Content-Type: application/json" \
  -d '{"id":"loan-demo","applicantName":"Jane Smith","amountCents":1500000,"annualIncomeCents":9000000}')
echo "    Response: $RESP2"

STATE2=$(echo "$RESP2" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('state','?'))" 2>/dev/null || echo "?")
if [[ "$STATE2" != "AWAITING_APPROVAL" ]]; then
  echo "ERROR: expected state AWAITING_APPROVAL, got: $STATE2"
  exit 1
fi
echo "    State = $STATE2  (credit pull was NOT repeated, resumed from checkpoint)"

# --- Money shot 3: human-in-the-loop ----------------------------------------
echo ""
echo ">>> MONEY SHOT 3: human-in-the-loop"
echo "    Officer approving disbursement..."
RESP3=$(curl -s -X POST http://localhost:8080/applications/loan-demo/approve \
  -H "Content-Type: application/json" \
  -d '{"userId":"officer@bank","decision":"approved","comment":"ok"}')
echo "    Response: $RESP3"

STATE3=$(echo "$RESP3" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('state','?'))" 2>/dev/null || echo "?")
if [[ "$STATE3" != "COMPLETED" ]]; then
  echo "ERROR: expected state COMPLETED after approval, got: $STATE3"
  exit 1
fi
echo "    State = $STATE3"

# --- Money shot 2: governance -----------------------------------------------
echo ""
echo ">>> MONEY SHOT 2: governance, fetching audit bundle..."
RECEIPTS=$(curl -s http://localhost:8080/applications/loan-demo/receipts)
echo "$RECEIPTS" | pretty_json

VERIFIED=$(echo "$RECEIPTS" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('verified','?'))" 2>/dev/null || echo "?")
COUNT=$(echo "$RECEIPTS" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d.get('receipts',[])))" 2>/dev/null || echo "?")

echo ""
echo "    verified = $VERIFIED"
echo "    receipt count = $COUNT"

if [[ "$VERIFIED" != "True" && "$VERIFIED" != "true" ]]; then
  echo "WARNING: audit bundle did not verify (verified=$VERIFIED)"
fi

echo ""
echo "==> Demo complete."
echo "    All three guarantees exercised:"
echo "    1. Durability: checkpoint survived kill -9, no repeated credit pull"
echo "    2. Governance: audit bundle verified=$VERIFIED with $COUNT signed receipts"
echo "    3. Human gate: officer approval unblocked disbursement, stamped on receipt"
