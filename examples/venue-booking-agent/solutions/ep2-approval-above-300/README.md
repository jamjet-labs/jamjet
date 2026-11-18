# Episode 2 challenge: approval only above 300

The challenge at the end of episode 2: let small deposits through, and
ask a person only above 300. Start from `step-05-ask-first`. Try it
yourself before you read on.

## Install

    pip install "jamjet[sidecar]==0.13.0"

Set the signing key from the series README, under "Before you start".
Set it in every terminal you open below.

## What changed from step 05

`require_approval_for` matches tool names. It does not look at the
arguments, so on its own it cannot say "only above 300". The solution
splits the payment into two tools:

- `pay_small_deposit` pays with no approval while the venue's total
  stays at 300 or less. Its own code refuses more, whatever the model
  asks for.
- `pay_deposit` pays more, and the policy still holds it for approval.

In `venue_tools.py`:

    @tool
    def pay_small_deposit(venue: str, amount: float) -> str:
        """Pay a venue deposit of 300 or less. It needs no approval."""
        total = paid_so_far(venue) + amount
        # Two payments of 250 add up to 500, so the limit is on the total.
        if 0 < amount and total <= SMALL_LIMIT:
            return record_payment(venue, amount)
        return (...)  # refused: use pay_deposit

The limit is in code, not in the prompt. If the model sends 500 to
`pay_small_deposit`, the tool refuses and nothing is written to
`payments.log`. Splitting it into two payments of 250 does not help:
`paid_so_far` adds up what `payments.log` already holds for the venue,
so the second 250 is refused too.

In `agent.py`, `pay_small_deposit` joins the tools list, and the prompt
asks for a 200 deposit. The policy line is the same as in step 05:

    policy={"require_approval_for": ["pay_deposit"]},

## Run it

Terminal 1, the engine, in this folder:

    jamjet dev --modules venue_tools

Terminal 2, the agent:

    python agent.py

This time it does not wait. 200 is under the limit, so
`pay_small_deposit` pays it with no approval. (A real model could pick
`pay_deposit` for 200 instead. Then the run waits for approval, as in
step 05.)

## Expected output

    model: scripted (no API key)
    Booked Casa do Rio in Lisbon for 11 December. The 200 deposit is paid.
    execution: exec_...
    payments.log: 1 line(s)

`jamjet audit exec_...` shows `pay_small_deposit` ran once, with no
approval.

## Now try 500

With a real model, change 200 to 500 in `PROMPT` and run it again. The
model picks `pay_deposit`, and the run waits for you, as in step 05.
On Claude Sonnet 5.5 the 200 run paid at once, and the 500 run waited
until I approved it. The scripted model answers from its file, so it
stays at 200.

## Notes

- With a real model, set the provider key in terminal 1 as well. The
  engine's worker makes the model call, not `agent.py`.
