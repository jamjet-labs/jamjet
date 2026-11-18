# Step 07: the engine crashes

## Install

    pip install "jamjet[sidecar]==0.13.0"

Set the signing key from the series README, under "Before you start".
Set it in every terminal you open below. An `export` in one terminal
does not reach the others.

## What changed

The code goes back to step 05. Copy `agent.py`, `venue_tools.py` and
`script.yaml` from `step-05-ask-first` into this folder. Stop step 06's
`jamjet dev` first (Ctrl-C). This step is about what happens to a run
that is waiting for approval when the engine dies.

## Run it

Open three terminals, all in this step folder.

Terminal 1, the engine:

    jamjet dev --modules venue_tools

Terminal 2, the agent. It prints the model line, then waits:

    python agent.py

`agent.py` waits up to 5 minutes (the default `timeout_seconds=300`).
If it gives up, run it again, or approve faster.

Terminal 3, check that the run is waiting at the gate:

    jamjet approvals

Do not approve yet. Now kill the engine, hard. First, if another
`jamjet-server` is running on your machine, the next command would kill
it too, so run `pgrep -f jamjet-server` alone and check it prints one
id. If it prints nothing, the engine is not running and `kill` only
shows a usage message. Then, in terminal 3:

    kill -9 $(pgrep -f jamjet-server)

`pgrep -f jamjet-server` prints the engine's process id, and `kill -9`
ends it with no chance to clean up. Terminal 1 shows `jamjet dev`
stopping the rest of the stack.

Tested on macOS. Not yet tested on Linux.

## The agent waits

Terminal 2 prints one line on stderr now, and another after the restart.
It keeps waiting. It does not
fail and it does not pay:

    jamjet: lost the engine at http://127.0.0.1:7700; waiting for it to come back (execution exec_...)

Start the engine again, in terminal 1. Use the same folder, because
`.jamjet/` holds the run:

    jamjet dev --modules venue_tools

It finds the same database in `.jamjet/`, so it still knows about the
run. Terminal 2 prints:

    jamjet: the engine is back; resuming the wait

## Approve

In terminal 3, list the approvals again. The run can take a few
seconds to show up after the restart:

    jamjet approvals

Paste the `jamjet approve` command it prints and add `--as` with your
name, the same as in step 05.

## Expected output

Terminal 2 then prints:

    model: scripted (no API key)
    Booked Casa do Rio in Lisbon for 11 December. The 500 deposit is paid.
    execution: exec_...
    payments.log: 1 line(s)

One payment. The crash did not make it pay twice, and it did not lose
the run. Step 08 shows why.

## Notes

- With a real model, set the provider key in terminal 1 as well. The
  engine's worker makes the model call, not `agent.py`.
- A connection error means terminal 1 is not running `jamjet dev`.
