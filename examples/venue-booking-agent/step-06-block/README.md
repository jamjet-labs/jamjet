# Step 06: the engine blocks a cancellation

## Install

    pip install "jamjet[sidecar]==0.13.0"

Set the signing key from the series README, under "Before you start".
Set it in every terminal you open below. An `export` in one terminal
does not reach the others.

## What changed

The policy gains a second rule, and the agent gets a new tool:

    policy={
        "require_approval_for": ["pay_deposit"],
        "blocked_tools": ["cancel_*"],
    },

`cancel_booking` is in `venue_tools.py`. It adds a line to
`cancellations.log` each time it really runs. The pattern `cancel_*`
blocks every tool whose name starts with `cancel_`.

`agent.py` takes an optional prompt. With none, it asks to cancel
the booking at Casa do Rio. The scripted model asks to cancel whatever
prompt you pass, so the argument only matters with a real model.

## Run it

Open two terminals, both in this step folder.

Terminal 1, the engine:

    jamjet dev --modules venue_tools

Terminal 2, the agent:

    python agent.py

## Expected output

    model: scripted (no API key)
    run failed: durable agent run ended in non-completed state: failed (execution exec_...)
    execution: exec_...
    cancellations.log: 0 line(s)

The run failed, and nothing was cancelled.

## Who stopped it

The engine stopped the action, not the model. The model asked to
cancel, and the engine refused before the tool ran. The model gets no
turn to explain, so the run just ends as failed.

The `run failed: ...` line from `run_durable` gives no reason. The
reason, `matches blocked pattern 'cancel_*'`, appears only in
`jamjet audit`, so ask the engine why. Use the id the agent printed:

    jamjet audit exec_...

You see:

    model asked for check_dates         call_0_0 {"venue":"Casa do Rio"}
    ran check_dates                     call_0_0 -> ["2026-12-04", "2026-12-11"]
    model asked for cancel_booking      call_1_0 {"venue":"Casa do Rio"}
    cancel_booking blocked              tool 'cancel_booking' matches blocked pattern 'cancel_*'
    run failed: ...

(Each line starts with a time, left out here.)

## Notes

- `agent.py` finds the engine at `http://127.0.0.1:7700`. To use another
  address, set `JAMJET_RUNTIME_URL`.
- With a real model, pass your own prompt: `python agent.py "your request"`.
- With a real model, set the provider key in terminal 1 as well. The
  engine's worker makes the model call, not `agent.py`.
