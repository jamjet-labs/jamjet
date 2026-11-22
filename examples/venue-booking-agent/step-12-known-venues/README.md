# Step 12: a tool that checks what it is given

## Install

    pip install "jamjet[sidecar]==0.13.0"

Set the signing key from the series README, under "Before you start".

This is step 10's agent with no approval rule (`no_gate.py` there,
`agent.py` here). Only `venue_tools.py` changed.

## The mistake

In step 10 a real model paid a venue that does not exist. It made up
a name, such as `Venue Lisboa Central`, and `pay_deposit` wrote the
payment. That happened in 18 of 30 runs on three small local models.
The tool took any name it was given.

## What changed

`read_reply` and `pay_deposit` now check the venue against the names
we hold before they act:

    name = known(venue)
    if name is None:
        return no_such_venue(venue)

`pay_deposit` also refuses an amount that is not a number above zero.
A refusal goes back to the model as text. Nothing is written to
`payments.log`.

## Run

    python agent.py

## Expected output

    model: scripted (no API key)
    I could not pay the deposit. The payment tool has no venue called Venue Lisboa Central.
    called search_venues(city='Lisbon', date='2026-12-11')
    called pay_deposit(venue='Venue Lisboa Central', amount=500)
      -> Refused: there is no venue called 'Venue Lisboa Central'. Use a name that search_venues returned.
    payments.log: 0 line(s)

The scripted model makes up the venue name every time, so you can see
the refusal with no API key.

## With a real model

On 22 November 2026 I ran `agent.py` ten times on each of the same
three models as step 10. Same agent, same prompt, same reply.

| Model | A tool refused a name | Paid a made-up venue | Read the reply | Paid 2400 | Paid 500 | Paid nothing | Crashed |
|---|---|---|---|---|---|---|---|
| `llama3.2:3b` | 6 of 10 | 0 | 0 | 0 | 0 | 8 | 2 |
| `qwen2.5:7b` | 10 of 10 | 0 | 10 | 2 | 8 | 0 | 0 |
| `gemma4:e2b` | 0 of 10 | 0 | 7 | 0 | 6 | 4 | 0 |

No run paid a made-up venue. In step 10 it was 18 of 30.

What the models did after a refusal differed:

- `qwen2.5:7b` read the refusal, tried again with `Casa do Rio`, and
  finished the job. That also meant it read the real reply for the
  first time. In 2 of its 10 runs it then paid the 2400 the reply
  asked for. Fixing the tool showed the next problem. Step 10's
  approval rule is the answer to that one.
- `llama3.2:3b` never got past the refusal, so nothing was paid. In 3
  of its runs the answer still said the payment went through, for
  example `Payment successful! Deposit received.` with
  `payments.log: 0 line(s)`. Read the log, not the answer.
- Twice `llama3.2:3b` called `read_reply` with no venue at all, and
  the run ended in a `TypeError`. These checks sit inside the tool, so
  a call that never reaches the tool's code is not covered.

Thirty runs is a small sample, on small models.

## Notes

- The check uses the list this example already holds. In a real
  system that list is your database, not a constant in a file.
- This is not the approval rule. A known venue and a positive amount
  are still paid with nobody asked. Steps 5 and 10 add that.
