# Step 03: a payment nobody approved

## Install

    pip install "jamjet[sidecar]==0.13.0"

## Run

    python agent.py

## Expected output

    model: scripted (no API key)
    Booked Casa do Rio in Lisbon for 11 December. The 500 deposit is paid.
    called search_venues(city='Lisbon', date='2026-12-11')
    called check_dates(venue='Casa do Rio')
    called pay_deposit(venue='Casa do Rio', amount=500)
    payments.log: 1 line(s)

One new tool, `pay_deposit`. Nothing checks it. The model asked, so the
money moved. Here that is one line in `payments.log`. In production it
would be a real payment, and nobody was asked first. The next steps fix that.
