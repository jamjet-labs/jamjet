# Step 09: the audit

## Install

    pip install "jamjet[sidecar]==0.13.0"

Set the signing key from the series README, under "Before you start".
Set it in every terminal you open below.

## What changed

Nothing. The code is the step 05 code again. This step only reads, with
the run from step 07 and the restarted engine still running in
terminal 1.

## Who approved this?

In another terminal, in this step folder, use the execution id the
agent printed:

    jamjet audit exec_...

You see this (a captured sample, so your ids and times will differ):

    Audit trail for exec_a66351f02255436598ab4986fb4db6d6
    Agent: venue-booker   Status: completed   Started: 2026-11-18 06:27:43
    Source: the engine's event log and approval records. Times are UTC.

    06:27:44  model asked for search_venues  call_0_0 {"city":"Lisbon","date":"2026-12-11"}
    06:27:47  ran search_venues              call_0_0 -> [{"venue": "Casa do Rio", "capacity": 120, "price": 2400}]
    06:27:47  model asked for check_dates    call_1_0 {"venue":"Casa do Rio"}
    06:27:49  ran check_dates                call_1_0 -> ["2026-12-04", "2026-12-11"]
    06:27:49  model asked for pay_deposit    call_2_0 {"amount":500,"venue":"Casa do Rio"}
    06:27:51  pay_deposit needs approval     approver: human
    06:28:01  approved by tester             dev mode, name not verified
    06:28:03  ran pay_deposit                call_2_0 -> Deposit of 500 paid for Casa do Rio.
    06:28:04  final answer                   Booked Casa do Rio in Lisbon for 11 December. The 500 deposit is paid.

    search_venues ran 1 time
    check_dates ran 1 time
    pay_deposit ran 1 time, approved by tester at 06:28:01

The last line answers the question: `pay_deposit` ran 1 time, approved
by `tester`, at 06:28:01. The times are UTC.

The line `dev mode, name not verified` is honest. In dev mode the
engine takes the name you typed after `--as` and does not check it.

## The same thing as data

    jamjet audit exec_... --json

This prints the run and one row per step. The rows are the same as the
lines above:

    {
      "execution": { "status": "completed", ... },
      "rows": [
        { "time": "...", "step": "approved by tester",
          "detail": "dev mode, name not verified" },
        ...
      ],
      "tool_runs": {
        "search_venues": 1,
        "check_dates": 1,
        "pay_deposit": 1
      }
    }

Use it when you want to check a run from a script instead of by eye.

## Notes

- The audit comes from the engine's event log and approval records,
  not from anything the model said.
- Step 06 used the same command to show a blocked tool.
