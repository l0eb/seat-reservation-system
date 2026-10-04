# Write-up

Live: https://13-126-244-127.sslip.io · how to run and test: [README.md](README.md)

## 1. The atomic decision

**Postgres decides every booking, in one transaction, with a conditional
update under row locks.** Each seat is one row (`seats`, primary key
`(show_id, label)`) holding its `status` and `reservation_id`; a check
constraint ties them together (`available` ⇔ no reservation). Booking:

```sql
-- inside one READ COMMITTED transaction, after the idempotency and limit steps below
SELECT label FROM seats WHERE show_id = $1 AND label = ANY($2) ORDER BY label FOR UPDATE;
UPDATE seats SET status = 'confirmed', reservation_id = $3, version = version + 1
 WHERE show_id = $1 AND label = ANY($2) AND status = 'available'
 RETURNING label, version;
-- fewer rows than requested → roll back the whole thing → 409 seat_taken
```

Why it can't double-sell: two buyers of A12 both reach the `FOR UPDATE`; one
gets the row lock, the other waits. The winner's `UPDATE` matches
(`status = 'available'`) and commits. When the waiter gets the lock, Postgres
re-checks the `WHERE` against the committed row (READ COMMITTED's re-evaluation
of an updated row), sees `confirmed`, and updates nothing. The decision
"is it free?" and the write "take it" are the same statement on a locked row;
there is no read-then-write gap. A 500-buyer storm on one seat gives exactly
one 201 and 499 × `409 seat_taken`, both locally and live.

**Multi-seat requests are all-or-nothing**, the documented answer to the brief's
partial-request question: if `["A12","A13"]` claims only one row, the count
doesn't match and the transaction rolls back, so nobody is ever left holding
half a request. That holds under concurrency because it is the same
transaction.

**No deadlocks, by a single global lock order.** Every transaction that
touches seats locks, in this order: the reservation row (its idempotency key),
then the user's `user_show_holds` row, then the seats sorted by label. Two
multi-seat bookings that overlap (`[A1,A2]` and `[A2,A1]`) both lock A1 first,
so one simply waits for the other; cancels follow the same order. Lock waits
are bounded anyway (`statement_timeout` 5 s). A serialization failure or
deadlock (`40001`/`40P01`) would retry the transaction once and then return
`503`, never a 500; across every load test it never happened
(`reserve_retries_total` stayed 0).

**Per-user limit, atomically:** `user_show_holds` keeps a seat count per user
per show, and the booking raises it with one conditional upsert:
`... ON CONFLICT DO UPDATE SET held = held + n WHERE held + n <= per_user_limit`.
No row back means over the limit → roll back → `409 per_user_limit`. Ten
parallel bookings from one user on a limit-4 show end with exactly 4.

**What sits in front of Postgres can only say no.** Most of an on-sale burst
asks for seats that are already gone (a 13,000-seat show can have at most
13,000 winners; 5,000/s for 30 s is 150,000 requests). Each replica keeps an
in-memory seat map and declines those requests without touching the database:
about half the requests in the 90k-request burst. It never grants anything.
It can only be wrong in the safe direction (a seat looks free → Postgres
decides) by construction: every seat write bumps `seats.version` and the map
only applies newer versions; bookings update it after commit and cancels
before. Users who already hold a reservation always go to Postgres, so their
retries replay rather than being declined. Behind the map, a semaphore
(24 permits per process, 10 per replica in the 3-replica setup) caps bookings
using the database at once; waiters queue in order for up to 15 s, then get
`503 overloaded` with `Retry-After`.

## 2. Idempotency

- **Where the key lives:** in the `reservations` row itself, with
  `UNIQUE (user_id, idempotency_key)`, plus `request_hash` = SHA-256 of the
  canonical request (show id and the sorted, de-duplicated seat list). Keys are
  scoped per user, so two users can't collide or replay each other.
- **Exactly once:** the booking transaction starts by inserting the reservation
  row with `ON CONFLICT (user_id, idempotency_key) DO NOTHING`. The unique index
  is the arbiter: of N concurrent requests with one key, exactly one inserts;
  the others block on the index until it commits (or rolls back), then see the
  row. Seats are claimed only by the inserter, inside the same transaction, so
  the key and the booking commit or vanish together.
- **Replay:** a retry finds the row and, if the hash matches, returns the
  original reservation with `201` (counted as `declined{reason="idempotent_replay"}`,
  since it books nothing). This holds after the fact too: a lookup by key runs
  before any seat check, so a retry of a successful booking replays even though
  its seats are now "taken".
- **Same key, different body:** hash differs → `409 idempotency_mismatch`.
- **A race I found and fixed:** a retry could look up its key (not there yet),
  then see its own seats taken because the original committed in between, and
  get `409 seat_taken`, about 1 in 10 under 20 parallel same-key requests. On
  `seat_taken`, the key is looked up once more and the request replays if it now
  exists. Tested at 100 runs × 20 parallel copies.
- The key can come in the `Idempotency-Key` header or the body's
  `idempotency_key` (both, if equal). `POST /shows` takes an optional key the
  same way, scoped to the admin, so a retried create can't make a second show.

## 3. Holds and expiry

I chose **explicit release**: a reservation is `confirmed` straight away (the
brief's success response says `"status": "confirmed"`), and
`POST /reservations/{id}/cancel` releases it, owner only (anyone else gets 403;
a second cancel 409).

A cancel frees seats with
`UPDATE seats SET status='available', reservation_id=NULL WHERE ... AND reservation_id = <this reservation>`,
under the same lock order. Matching on `reservation_id` is what makes a release
unable to resurrect anything: it can only ever free seats that still belong to
the reservation being cancelled, never a seat someone else has since bought.
The user's hold count drops in the same transaction. Freed seats are
re-bookable immediately, including through other replicas (section 4).

I didn't build time-boxed holds because nothing in this flow needs a seat held
without being sold: there's no payment step. The schema leaves room for it
(`held` is a valid seat status and is counted in `GET /shows/{id}`). With a
payment step I'd add `held_until`, have reserve create `held` seats, a confirm
endpoint, and a sweeper releasing expired holds with
`UPDATE ... WHERE status='held' AND held_until < now() ... FOR UPDATE SKIP LOCKED`
using the same `reservation_id` guard.

## 4. Consistency vs availability under a partition

**The system chooses consistency for bookings.** Postgres (one primary) is the
only place a booking is decided; nothing else can grant a seat. If the
database is unreachable, bookings stop rather than guess:

- **Replica ↔ Postgres partition:** `/readyz` runs `SELECT 1` with a 1 s
  timeout and fails closed with 503; Caddy's health check (every 2 s) stops
  sending that replica traffic. If every replica loses the database, bookings
  get `503 {"error":"no_replica_ready"}` from the edge, while `/healthz` and
  `/metrics` keep answering (they deliberately bypass the readiness check so
  you can still see what's happening). Verified live by firewalling port 5432
  on all three replicas: 503 within 1 s, recovery within a second of
  unblocking. Requests in flight at the moment of a partition can fail; a
  pooled connection that times out returns 503, and one whose commit is cut off
  has an unknown outcome. The answer is the same as for any lost response:
  retry with the same idempotency key, which replays if it booked.
- **Database failover:** RDS is single-AZ for this demo, so a primary failure is
  downtime until it's restored. Multi-AZ would make that a ~1–2 minute
  failover; the service already waits for the database on start and readiness
  tracks it.
- **Replica ↔ replica:** replicas never talk to each other to decide anything.
  They share seat-map updates through Postgres `LISTEN/NOTIFY`, so a replica that
  can reach Postgres also hears the others. The cost of staleness is bounded by
  the map's one-way rule: a stale map can only *decline* wrongly, for the few
  milliseconds between a cancel committing on one replica and its notice
  reaching another, and can never cause a double sale. Cancels notify inside
  their transaction (delivered exactly on commit, never lost); bookings are
  announced right after commit, because notifying inside the booking
  transaction serialized every commit on Postgres's global notify lock (p99
  went from 80 ms to 730 ms; moving it fixed that, back to 20 ms). A lost
  booking notice (a replica crashing between commit and announce) only leaves
  a seat looking free to the others, and a 15 s audit reloads any show whose
  map disagrees with the database once the show is quiet. If a replica's
  listening connection drops, it reloads the whole map.
- **Cache (Dragonfly) partition:** availability over freshness, safely. The
  cache holds only reads (the show row; the `GET /shows/{id}` body for 2 s,
  deleted on every booking/cancel commit). Calls time out after 100 ms and a
  breaker skips the cache for 5 s after a failure. Writes never go through it,
  so losing it costs latency, not correctness.
- **Metrics:** `/metrics` on any replica adds up its peers' counters; a peer
  that doesn't answer in 500 ms is shown as `replica_up{replica} 0` rather than
  silently left out of the totals.

## 5. Observability: what pages me at 2am

The thing that must never happen is a double sale or an inconsistent seat
count. Everything else is about whether buyers are being served.

**Page:**
1. **Any invariant breach.** A scheduled SQL check (the same queries the burst
   tool and I used: a seat in two confirmed reservations, a reservation not
   owning its seats, hold counters ≠ confirmed seats, a user over the limit)
   returning anything but zero. Also `seats_available + seats_held +
   seats_confirmed != total_seats` for any show. This shouldn't be possible by
   construction; if it fires, stop sales.
2. **Bookings failing:** a rate of `http_requests_total{status="500"}` > 0
   (a 500 is always a bug here); or sustained `reserve_shed_total` /
   `503 overloaded` (the database is the bottleneck and buyers are being turned
   away).
3. **Nothing ready:** an external probe of `/readyz` failing (the edge returns
   `no_replica_ready`), i.e. the database is unreachable or every replica is down.
4. **`reserve_retries_total{result="failed"}` > 0:** a deadlock or
   serialization failure survived its retry, meaning the lock-order guarantee
   was broken by a change.

**Ticket for the morning, not a page:** one replica down
(`replica_up == 0`; two others still serve); `seat_map_taken !=
seats_confirmed` persisting past the audit interval (correctness is unaffected,
the audit or notify path is broken); cache errors (it's bypassed); database
CPU or connections trending toward the limit.

**Not alerts:** 409s. Thousands of `seat_taken` during an on-sale are the
system working.

Logs are structured JSON, one line per request (status, latency) inside a span
carrying the `request_id` that is also returned in the `x-request-id` header,
plus method, path and replica; so a buyer's complaint with a request id leads
straight to the line, the replica, and anything else it logged. They go to
CloudWatch, one stream per replica. A background writer keeps logging off the
request path: making it synchronous had raised p99 at 5,000/s from 12 ms to
62 ms; with the writer it is back to 14 ms.

Gap I'd close first: there are no latency histograms, only counters. A p99
booking-latency SLO is the alert I'm missing.

## 6. How it was tested

| What | Result |
|---|---|
| Burst, all scenarios, locally through 3 replicas + Caddy (90k bookings at 2k/5k/2k per second, 500-buyer hot seat) | 40/40 checks; p99 ≈ 12 ms; 0 × 5xx; every replica's seat map equal to the database |
| Same, from a fresh `git clone` with an empty database | 40/40; Postman 75/75; 29 unit tests |
| Book on replica B, cancel on A, immediately re-book on B (300 times) | 300/300 re-books succeed (the cross-replica notice always arrived first) |
| Replica stopped, replica server rebooted, rolling redeploy of all three, each during a live burst | 0 errors, seat lists exact; a rebooted server rejoins on its own |
| Replica killed (SIGKILL) mid-burst, locally | 2–5 × 502 for requests in flight on it (unavoidable; the tool treats them as unknown outcome); no double booking; maps healed |
| Edge server rebooted | serving again in 19 s, same certificate |
| `/readyz` with Postgres firewalled on all replicas, live | 503 in 1 s; `/healthz` and `/metrics` still 200; recovers in < 1 s |
| Burst against the live URL from a home connection | 300/s: 40/40, p99 ≈ 430 ms. Full rate: the client couldn't open TLS connections fast enough (714 timeouts) while the servers peaked at 14–56% CPU and shed nothing; SQL invariants 0 afterwards |
| A deliberately broken build (availability check removed) | the burst tool fails: 24 winners on the hot seat, double-booked seats named |

## 7. AI usage

I built this with Claude Code (Claude) as the main pair: it wrote nearly all of
the code, scripts and tests, ran the load tests and the deploy checks, and
kept a tracker and a Postman collection up to date. I directed the work phase
by phase and made the calls on scope, architecture and trade-offs.

**What I directed or decided:**
- The bar: "no double booking ever" at 2,000–5,000 requests per second, and
  I didn't accept that buyers should have to wait in a queue instead. That
  pushed the design from "a semaphore in front of Postgres" to declining
  doomed requests from memory.
- Idempotency on show creation: I noticed some POST endpoints had no key and
  asked how they handled retries.
- Horizontal scaling: I rejected the single-instance deployment and asked how
  one box would take 5,000 requests per second. That surfaced that the local
  numbers came from a 16-thread desktop, not the 2-vCPU server, and led to 3
  replicas behind Caddy and the cross-replica design.
- Platform: AWS (Mumbai) over the free-tier PaaS options. I looked at Hetzner
  and DigitalOcean as cheaper alternatives, dropped Hetzner because it has no
  India region, and went back to AWS. I chose on-demand over spot instances
  for the grading week.
- I asked for the deploy scripts to be reviewed for anything that could run
  up a bill, for every claim to be tested live, and for commits to be small and
  separate.
- I asked whether Kubernetes would be better and decided against it for this
  scope (section 8).

**What the AI decided or proposed, and I accepted:** the lock order and the
conditional-update claim; the request-hash idempotency scheme; the in-memory
seat map and its version rule; LISTEN/NOTIFY for cross-replica sync and the
audit; aggregated `/metrics`; the burst tool's design and its checks; SSM
instead of SSH; cross-compiling for Graviton.

**Where the AI was wrong and it got caught:**
- It first notified inside the booking transaction; measurement showed p99
  jumping from 80 ms to 730 ms, and Postgres wait-event sampling pinned it on
  the notify lock.
- Its first deploy script corrupted security-group IDs by logging to stdout
  inside a captured command; RDS rejected the request on my first real run.
- The burst tool reported four "double-booked" seats against the live URL;
  checking the database showed none: cancels whose responses were lost had
  been counted as still live. The tool was fixed, not the service.
- It initially presented local load numbers as if they'd hold on the deployed
  machine.

The burst tool exists partly as a check on the AI: it was run against a
deliberately broken build to prove it catches double booking.

## 8. What I'd do next

1. **Self-healing:** today a dead server stays dead until someone acts (Caddy
   routes around it). Put the replicas in an Auto Scaling group with upstream
   discovery, or move to ECS/Kubernetes. I chose not to adopt Kubernetes here:
   the booking decision lives in one Postgres, so more pods don't scale the hard
   part, and a cluster adds cost and moving parts to a one-day service.
2. **Latency histograms** and an SLO alert on booking p99; a dashboard.
3. **Holds with expiry** if a payment step is added (section 3).
4. **Database resilience:** Multi-AZ RDS; a larger instance or a partitioned
   seats table for shows far beyond 13,000 seats.
5. **A full-rate live run** from a load generator in the same region, to replace
   the "client couldn't keep up" caveat with real server-side numbers.
6. **Smaller show reads:** `GET /shows/{id}` returns all 13,000 seats (~490 KB,
   ~38 KB gzipped); add paging or a compact availability bitmap.
7. **Real authentication** (an identity provider) instead of the test-token route.
