# Step 04: a budget cap

## Install

    pip install "jamjet[sidecar]==0.13.0"

Set the signing key from the series README, under "Before you start".

## Run

    python agent.py

## Expected output

    model: scripted (no API key)
    budget exceeded: tokens 2080 >= limit 2000

One new argument: `budget={"tokens": 2000}`. The script plays a model
stuck in a loop. It asks for `check_dates` again and again and never
answers. Each turn uses 520 tokens. After four turns the total is 2080,
so the fifth model call is refused with `BudgetExceededError`. The
script catches it and prints the message.

The cap is checked before each model call, so the call that crosses it
still finishes. That is why the total can end above the limit.

A real model spends the budget its own way. In our runs, Claude Sonnet
5.5 asked for 31 searches in one turn, one for each day of December.
That turn alone used about 3,000 tokens, so the cap refused the next
call:

    model: anthropic/claude-sonnet-5-5
    budget exceeded: tokens 3034 >= limit 2000

The numbers change from run to run.

`budget=` also takes a dollar cap, like `budget=0.50`. A dollar cap
needs a known price for the model. A model with no price, including the
scripted one, never adds to it, so the cap does nothing. A token cap
works for any model.
