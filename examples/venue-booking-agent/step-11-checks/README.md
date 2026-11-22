# Step 11: checks that can fail

## Install

    pip install "jamjet[sidecar]==0.13.0" pytest

Set the signing key from the series README, under "Before you start".

The code is step 05's, unchanged. This step adds `test_checks.py`.

## The mistake

You change one line of an agent and ship it. The demo still works. How
do you know the approval rule is still there?

Reading the model's answer will not tell you. The two checks in
`test_checks.py` look at what happened, the payments log, not at what
the model said:

- nothing is paid when there is nobody to approve
- the run will not start when there is nobody to approve

## Run

    pytest -q

## Expected output

    ..                                                    [100%]
    2 passed in 1.66s

Your time will differ.

## Break it on purpose

A check that cannot fail tells you nothing. Open `agent.py`, delete
this line, and run `pytest -q` again:

    policy={"require_approval_for": ["pay_deposit"]},

Both checks fail. The first one says why:

    E   AssertionError: paid without approval: ['paid 500 for Casa do Rio']
    ...
    2 failed in 1.62s

Put the line back. They pass again.

## After you ship: running is not the same as working

Start the engine and the agent as in step 05, and do not approve.

Terminal 1:

    jamjet dev --modules venue_tools

Terminal 2:

    python agent.py

Terminal 3:

    jamjet executions

    │ exec_... │ venue-booker │ running │ 2026-11-22 10:43:24 │

The run shows as `running`. It is not doing anything. It is waiting
for a person. The status tells you the run is alive. It does not tell
you the work is moving. To see what a run is waiting for, read its
last event:

    jamjet events exec_...

The last row is `tool_approval_required`. Step 08 reads this log line
by line.

## Notes

- These checks need no engine, so they run in a second or two. They
  prove the rule is declared and that JamJet refuses to run without
  someone to enforce it. They do not prove the engine holds the run.
  The tests that do are in the JamJet repo, under
  [sdk/python/tests/integration](https://github.com/jamjet-labs/jamjet/tree/main/sdk/python/tests/integration).
- The checks always run on the scripted model. `test_checks.py`
  leaves `VENUE_MODEL` out of the run on purpose, so the checks give
  the same answer every time.
