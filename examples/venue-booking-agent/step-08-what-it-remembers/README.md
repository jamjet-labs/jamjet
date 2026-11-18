# Step 08: what the engine remembers

## Install

    pip install "jamjet[sidecar]==0.13.0"

Set the signing key from the series README, under "Before you start".
Set it in every terminal you open below.

## What changed

Nothing. The code is the step 05 code again. This step only reads. Run
step 07 first, so there is a finished run to look at, and keep the
engine running in terminal 1.

## List the runs

In another terminal, in this step folder:

    jamjet executions

You see:

    ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━━━━━━━┳━━━━━━━━━━━┳━━━━━━━━━━━━━━━━━━━━━┓
    ┃ execution                             ┃ agent        ┃ status    ┃ started (local)     ┃
    ┡━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━━━━━━━╇━━━━━━━━━━━╇━━━━━━━━━━━━━━━━━━━━━┩
    │ exec_a66351f02255436598ab4986fb4db6d6 │ venue-booker │ completed │ 2026-11-18 11:57:43 │
    └───────────────────────────────────────┴──────────────┴───────────┴─────────────────────┘

The run is `completed`, even though the engine died in the middle of
it. (The start time is in your local time. The audit in step 09 uses
UTC.)

## Read the event log

Use the id from the list:

    jamjet events exec_...

The engine writes every step of a run to a log before it acts on it.
Here is the part that matters, with the dull rows cut:

                     Events: exec_a66351f02255436598ab4986fb4db6d6
    ┏━━━━━┳━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━━━━━━━━━━┳━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
    ┃ Seq ┃ Type                   ┃ Node            ┃ Created at                  ┃
    ┡━━━━━╇━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━━━━━━━━━━╇━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┩
    │ 1   │ workflow_started       │ —               │ 2026-11-18T06:27:43.593956Z │
    │ 2   │ node_scheduled         │ __model_0__     │ 2026-11-18T06:27:43.594009Z │
    │ 3   │ node_started           │ __model_0__     │ 2026-11-18T06:27:44.065817Z │
    │ 4   │ node_completed         │ __model_0__     │ 2026-11-18T06:27:44.077678Z │
       ...
    │ 24  │ node_scheduled         │ __tools_2__     │ 2026-11-18T06:27:50.109108Z │
    │ 25  │ tool_approval_required │ __tools_2__     │ 2026-11-18T06:27:51.057782Z │
    │ 26  │ approval_received      │ __tools_2__     │ 2026-11-18T06:28:01.090721Z │
    │ 27  │ node_scheduled         │ __tools_2__     │ 2026-11-18T06:28:01.310513Z │
    │ 28  │ node_completed         │ __tools_2__     │ 2026-11-18T06:28:03.286950Z │
    │ 29  │ node_scheduled         │ __model_3__     │ 2026-11-18T06:28:03.323343Z │
       ...
    │ 35  │ node_skipped           │ __tools_3__     │ 2026-11-18T06:28:04.838670Z │
       ...
    │ 49  │ workflow_completed     │ —               │ 2026-11-18T06:28:04.853263Z │
    └─────┴────────────────────────┴─────────────────┴─────────────────────────────┘

Row 25 is the run stopping at the gate. Row 26 is your approval. That
is where the crash happened, between the two.

## Why it paid once

When the engine started again, it replayed this log. The log said the
search and the date check had finished, and that `pay_deposit` was
waiting for a person. So it went back to waiting. It did not run the
earlier steps again. After you approved, `pay_deposit` ran once and was
written to the log, so `payments.log` has one line.

## Try it yourself

Kill it at a different moment. Run step 07 again, but run `kill -9`
at another time. Try it right after you start `python agent.py`, while
the first tools are still running. Then restart the engine and look at
`jamjet events` again. Where does the log stop, and what does the
engine do next? Does it still pay exactly once?

## The limit

The engine surviving a crash is not the same as the worker surviving
one. If you `kill -9` the worker in the middle of a tool, that run
fails. JamJet 0.13.0 does not resume a tool that was cut off in the
middle. The run waiting for approval in step 07 survived because
nothing was running at the moment of the crash.
