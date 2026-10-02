# Seat Reservation at Scale

A JSON HTTP API that sells assigned seats for an event correctly under
concurrent load: no seat is ever double-sold, no user exceeds their
per-show booking limit, and no retried request is double-charged.

Built for the Paytm Money "Deploy & Observe" take-home.

Setup, run, and load-test instructions land here as the service takes shape
(see `WRITEUP.md` for the design rationale once it exists).
