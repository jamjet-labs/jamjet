# Step 05: ask first

## Install

    pip install "jamjet[sidecar]==0.13.0"

Set the signing key from the series README, under "Before you start".
Set it in every terminal you open below, and in the one where you run
`python try_in_process.py`. An `export` in one terminal does not reach
the others.

## The mistake

The policy `require_approval_for=["pay_deposit"]` says a person must
approve that tool. Run it in process and see:

    python try_in_process.py

    ...
    jamjet.agents.governance.ApprovalNotEnforceableError: Agent 'venue-booker'.run(): an approval gate is declared ...

Without the engine nothing can hold the run at the gate, so JamJet
refuses to run it. Nothing is paid.

## Run it with approval

The run is durable, so the engine holds it at the gate. You need three
terminals. Open every one in this step folder, terminal 3 too.

Terminal 1, the engine:

    jamjet dev --modules venue_tools

Terminal 2, the agent. It prints the model line, then waits:

    python agent.py

Terminal 3, the approval:

    jamjet approvals

It lists the waiting run and prints a ready `jamjet approve` command.
Paste it and add `--as` with your name:

    jamjet approve exec_... --decision approved --node-id=... --action-digest=... --as sunil

## Expected output

Terminal 2 then prints:

    model: scripted (no API key)
    Booked Casa do Rio in Lisbon for 11 December. The 500 deposit is paid.
    execution: exec_...
    payments.log: 1 line(s)

One payment, and only after you said yes.

## Notes

- `agent.py` finds the engine at `http://127.0.0.1:7700`. To use another
  address, set `JAMJET_RUNTIME_URL`.
- With a real model, set the provider key in terminal 1 as well. The
  engine's worker makes the model call, not `agent.py`.
