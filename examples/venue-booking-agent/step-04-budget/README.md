# Step 04: a budget cap

## Install

    pip install "jamjet[sidecar]==0.13.0"

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

`budget=` also takes a dollar cap, like `budget=0.50`. A dollar cap
needs a known price for the model. A model with no price, including the
scripted one, never adds to it, so the cap does nothing. A token cap
works for any model.
