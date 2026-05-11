# 02 — Human approval for risky actions

Show JamJet pausing a tool call until a human approves.

## Run

```bash
pnpm install
pnpm start
```

## Expected output

```text
Tool: payments.refund
Policy: payments.* requires approval
Decision: WAITING_FOR_APPROVAL
Approve via your control plane, then resume.
The model is mocked. The enforcement path is real.
```

## What this proves

The agent (mocked) plans `payments.refund`. The runtime policy matches
`payments.*` with `require_approval` and pauses. No API key, no network.

In a live `init()`-backed app the policy evaluator runs in-process; the
companion `requireApproval()` helper then polls JamJet Cloud until a human
approves or rejects via the dashboard.
