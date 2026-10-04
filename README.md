# Seat Reservation at Scale

A JSON HTTP API that sells assigned seats for a show and stays correct when
thousands of buyers stampede it at once: no seat is ever sold twice, no
user exceeds their per-show limit, and a retried request never books
twice. Built for the Paytm Money "Deploy & Observe" take-home.

**Live:** https://13-126-244-127.sslip.io (AWS Mumbai: 3 API replicas behind
Caddy, RDS Postgres). Design and trade-offs: [WRITEUP.md](WRITEUP.md).

```
client ─HTTPS─▶ edge: Caddy (TLS, least_conn, /readyz health checks) + Dragonfly cache
                  ├──▶ api-1 ┐
                  ├──▶ api-2 ├──▶ Postgres 16 — decides every booking
                  └──▶ api-3 ┘      (replicas share seat-map updates via LISTEN/NOTIFY)
```

Rust (axum, sqlx, tokio). Money is integer paise throughout.

## Try it against the live URL

```bash
BASE=https://13-126-244-127.sslip.io
token() { curl -s -X POST $BASE/auth/token -H 'content-type: application/json' -d "$1" | jq -r .token; }
ADMIN=$(token '{"user_id":"ops","role":"admin"}')
ALICE=$(token '{"user_id":"alice"}')

SHOW=$(curl -s -X POST $BASE/shows -H "authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  -d '{"name":"friday-night","seats":["A1","A2","A3"],"price_paise":25000}' | jq -r .id)

curl -s -X POST $BASE/shows/$SHOW/reserve -H "authorization: Bearer $ALICE" -H 'content-type: application/json' \
  -d '{"seats":["A2"],"idempotency_key":"k-1"}'
# {"reservation_id":"…","show_id":"…","user_id":"alice","seats":["A2"],"amount_paise":25000,"status":"confirmed",…}

curl -s $BASE/shows/$SHOW | jq .counts       # {"available":2,"held":0,"confirmed":1}
```

`POST /auth/token` mints test JWTs (24h) for any `user_id`; add `"role":"admin"`
to create shows. It is enabled on the live deployment so you can test, and off
by default (`AUTH_TOKEN_ROUTE_ENABLED`).

## Run it locally

Needs Docker. From a clean checkout:

```bash
docker compose up --build        # api-1..3 + Caddy on :8080, Postgres, Dragonfly
curl localhost:8080/readyz       # {"status":"ready"}
```

This is the same image and the same Caddyfile as the deployment, configured
only through the environment. Migrations run on start. pgAdmin is opt-in:
`docker compose --profile tools up` (http://localhost:5050).

To run the service on the host instead (needs Rust 1.93+):
`docker compose up -d postgres dragonfly && cp .env.example .env && cargo run --release --bin api`.

Tests: `cargo test --workspace` (29 unit tests; the concurrency behaviour is
tested end to end by the burst tool below).

## The burst: one command

```bash
./burst.sh <BASE_URL>                          # e.g. ./burst.sh http://localhost:8080
./burst.sh https://13-126-244-127.sslip.io --phases 300:20
./burst.sh <BASE_URL> --help                   # all options
```

Builds the `burst` binary (needs Rust) and replays an on-sale stampede against
any deployment through its public API, then checks the result. Exits 0 if every
check passes, 1 if any fails. Scenarios (`--scenario`, default all):

| Scenario | What it does | What must hold |
|---|---|---|
| hot-seat | 500 buyers send for seat A1 at the same instant, then all retry | exactly one 201; 499 × `409 seat_taken`, both waves; the winner's retry returns the same reservation |
| idempotency | 50 users × 20 parallel copies of one booking; then the same key with another seat; then the original again; plus parallel show creates with one key | one reservation per user; `409 idempotency_mismatch`; the replay returns the original; one show |
| per-user-limit | 20 users × 10 parallel single-seat bookings on a limit-4 show | every user ends with exactly 4; the rest `409 per_user_limit` |
| stampede | fixed-rate open-loop load (default 2,000/s → 5,000/s → 2,000/s for 10 s each: 90,000 bookings from 20,000 users on 13,000 seats), 5% retries, 2% of winners cancel | below |

It prints the outcome distribution (status and decline reason, with
p50/p95/p99 latency) per phase, then the checks: no seat held by two live
reservations; each idempotency key maps to one reservation; nobody over the
limit; the show's seat list matches exactly what buyers were told; counts add
up to the total seats; no 5xx; and a reconciliation against `/metrics`
(confirmed, each decline reason, replays, shed, 5xx) and of every replica's
in-memory seat map against the database.

Responses that never arrive (timeouts, a load balancer 502/504) are
"unknown outcome": the tool doesn't count them as declined, and leaves those
seats out of the seat-list check.

Results: all 40 checks pass locally at the full default rate (p99 ≈ 12 ms
through the 3 replicas) and against the live URL at 300/s from a home
connection (p99 ≈ 430 ms). At the full rate over the internet, a single home
connection can't open TLS connections fast enough to keep up; the servers sat
at 14–56% CPU. Run it from a machine near the deployment for full-rate
numbers.

## API

All bodies are JSON. Errors are `{"error": "<reason>"}`.

| Method & path | Auth | What it does |
|---|---|---|
| `POST /shows` | admin | Create a show: `{"name","seats":["A1",…],"price_paise","per_user_limit"?}` (or `"rows"` + `"seats_per_row"` instead of `seats`). Returns the show with every seat `available`. Optional `Idempotency-Key` header: a retry returns the same show. |
| `GET /shows` | — | Newest first, keyset-paged: `?limit=1..100&after=<id>` |
| `GET /shows/{id}` | — | Per-seat status and `counts` (`available + held + confirmed == total_seats`) |
| `POST /shows/{id}/reserve` | user | `{"seats":["A12"],"idempotency_key":"…"}` (or the `Idempotency-Key` header). **All or nothing**: either every seat is booked or none is. 201 with the reservation. |
| `POST /reservations/{id}/cancel` | owner | Frees the seats. Not the owner: 403. Already cancelled: 409. |
| `GET /healthz` | — | Liveness: 200 while the process runs |
| `GET /readyz` | — | Readiness: 200 only if Postgres answers `SELECT 1` within 1 s, else 503 |
| `GET /metrics` | — | Prometheus text for the whole service (see below) |
| `POST /auth/token` | — | Test JWTs, when enabled |

Identity comes only from the `Authorization: Bearer` token (HS256, `sub` =
user id); any `user_id` in a body is ignored.

| Status | `error` | Meaning |
|---|---|---|
| 409 | `seat_taken` | a requested seat is held by someone else |
| 409 | `per_user_limit` | the booking would take the user over the show's limit (default 4) |
| 409 | `idempotency_mismatch` | the key was used before for a different request |
| 409 | `already_cancelled` | cancelling twice |
| 422 | message | bad input: unknown seats, duplicates, missing key, … |
| 401 / 403 / 404 | message | no or bad token / not allowed / not found |
| 503 | `overloaded` | waited 15 s for a database slot; `Retry-After: 1`. Retrying with the same key is safe. |

## Metrics and logs

`GET /metrics` is the whole service whichever replica answers: counters summed
across replicas, per-replica gauges labelled `replica`, and `replica_up` showing
whose numbers are included. `GET /metrics/local` is one replica's own.

| Metric | |
|---|---|
| `reservations_confirmed_total` | bookings made |
| `reservations_declined_total{reason}` | `seat_taken`, `per_user_limit`, `idempotent_replay`, `idempotency_mismatch` |
| `seats_available` / `seats_held` / `seats_confirmed{show}` | read from the database in one statement, so they add up to the show's total |
| `http_requests_total{status}` | responses by status code |
| `reserve_requests_total{path}` | answered from the in-memory seat map (`memory`) or by Postgres (`database`) |
| `reserve_shed_total`, `reserve_retries_total{result}`, `reserve_inflight{replica}` | load handling |
| `seat_map_taken{show,replica}` | each replica's in-memory view; equals `seats_confirmed` at rest |
| `cache_lookups_total{result}`, `replica_up`, `process_start_time_seconds` | |

Logs are JSON, one line per event, with the `x-request-id` (also returned as a
response header) on every request span. Normal requests log nothing at `info`;
5xx responses and anything unusual log a line.

- Live: CloudWatch Logs group `/seat-reservation/api`, one stream per replica:
  `aws logs tail /seat-reservation/api --follow --region ap-south-1`
- Local: `docker compose logs -f api-1 api-2 api-3`

## Deploy (AWS)

```bash
deploy/up.sh      # creates everything in your default region (~10-15 min the first time)
deploy/push.sh    # build the arm64 image, push to ECR, roll out one replica at a time
deploy/down.sh    # delete everything (asks first) and list anything left
```

`up.sh` creates, all tagged `Project=seat-reservation`: an edge `c7g.large` on an
Elastic IP (Caddy with a Let's Encrypt certificate for `<ip>.sslip.io`, plus
Dragonfly), three `c7g.large` API replicas reachable only from the edge and
each other, RDS Postgres 16 (`db.t4g.small`) reachable only from the replicas,
four security groups, an IAM role, SSM parameters for secrets and a CloudWatch
log group. There's no SSH: instances are configured and redeployed through SSM.
About $6/day; `down.sh` stops all of it.

## Configuration

| Variable | Default | |
|---|---|---|
| `DATABASE_URL` | — | required |
| `JWT_SECRET` | — | required |
| `CACHE_URL` | unset (no cache) | Dragonfly/Redis; optional, failures bypass it |
| `AUTH_TOKEN_ROUTE_ENABLED` | `false` | enables `POST /auth/token` |
| `PORT` | 8080 | |
| `DB_POOL_MAX_CONNECTIONS` | 30 | per replica |
| `RESERVE_SEMAPHORE_PERMITS` | 24 | bookings using the database at once, per replica |
| `RESERVE_QUEUE_TIMEOUT_SECS` | 15 | wait for a permit before `503 overloaded` |
| `DB_ACQUIRE_TIMEOUT_SECS` / `DB_STATEMENT_TIMEOUT_SECS` | 5 / 5 | |
| `REPLICA_ID`, `PEERS` | `api`, none | name in metrics; other replicas' URLs for `/metrics` totals |
| `SHUTDOWN_DRAIN_SECS` | 0 | on SIGTERM, fail `/readyz` this long before closing |

## Layout

```
api/            the service (src/routes: shows, reservations, health, auth; seat_map.rs; migrations/)
burst/          the burst tool
burst.sh        runs it
caddy/          the load balancer config, shared by compose and the deployment
deploy/         AWS scripts (up, push, down) and the per-host scripts they run
Dockerfile      multi-stage, cross-compiles for amd64 or arm64; distroless runtime
```
