//! The on-sale rush: bookings at a fixed rate, phase by phase, whether or
//! not earlier ones have answered (that is how real traffic arrives). Some
//! requests are client retries of earlier ones, and some winners cancel,
//! which puts seats back for others.
//!
//! Afterwards the run is checked from the outside: from the client's own
//! record of who got what, no seat may belong to two live reservations,
//! and the show's seat list must match that record exactly.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use uuid::Uuid;

use super::Ctx;
use crate::api::{Reply, ShowSpec};
use crate::report::{Tally, Totals};

const NAME: &str = "stampede";
const PER_USER_LIMIT: usize = 4;
/// Behind schedule by more than this, the run didn't deliver its rate.
const MAX_SEND_LAG: Duration = Duration::from_millis(250);

pub struct Options {
    pub users: usize,
    /// (requests per second, seconds)
    pub phases: Vec<(u64, u64)>,
    pub rows: u32,
    pub seats_per_row: u32,
    pub retry_rate: f64,
    pub cancel_rate: f64,
    pub seed: u64,
}

#[derive(Clone)]
struct Planned {
    at: Duration,
    phase: usize,
    user: usize,
    key: String,
    seats: Vec<String>,
    cancel: bool,
}

struct Outcome {
    plan: Planned,
    lag: Duration,
    reply: Reply,
    cancel: Option<Reply>,
}

pub async fn run(ctx: &mut Ctx, opts: &Options) -> Result<Totals> {
    let total: u64 = opts.phases.iter().map(|(rate, secs)| rate * secs).sum();
    let profile: Vec<String> = opts
        .phases
        .iter()
        .map(|(r, s)| format!("{r}/s for {s}s"))
        .collect();
    println!(
        "\n== stampede: {total} bookings ({}) from {} users on {} seats; {:.0}% retries, {:.0}% of winners cancel; seed {}",
        profile.join(", then "),
        opts.users,
        opts.rows * opts.seats_per_row,
        opts.retry_rate * 100.0,
        opts.cancel_rate * 100.0,
        opts.seed
    );
    let api = ctx.api.clone();
    let spec = ShowSpec {
        name: ctx.show_name(NAME),
        rows: opts.rows,
        seats_per_row: opts.seats_per_row,
        per_user_limit: PER_USER_LIMIT as u32,
    };
    let show = Arc::new(api.new_show(&ctx.admin, &spec).await?);
    ctx.shows.push((NAME, show.to_string()));
    let tokens = Arc::new(api.mint_users(&ctx.users(NAME), opts.users).await?);

    let plan = make_plan(opts);
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(plan.len());
    for planned in plan {
        let due = started + planned.at;
        tokio::time::sleep_until(due.into()).await;
        let lag = Instant::now().saturating_duration_since(due);
        let (api, show, tokens) = (api.clone(), show.clone(), tokens.clone());
        tasks.push(tokio::spawn(async move {
            let token = &tokens[planned.user];
            let reply = api
                .reserve(token, &show, &planned.seats, &planned.key)
                .await;
            let cancel = match reply.reservation_id() {
                Some(id) if planned.cancel && reply.is(201) => Some(api.cancel(token, id).await),
                _ => None,
            };
            Outcome {
                plan: planned,
                lag,
                reply,
                cancel,
            }
        }));
    }
    let sending_took = started.elapsed();
    let mut outcomes = Vec::with_capacity(tasks.len());
    for task in tasks {
        outcomes.push(task.await?);
    }
    println!(
        "  sent on schedule in {:.1}s (planned {}s); last answer at {:.1}s",
        sending_took.as_secs_f64(),
        opts.phases.iter().map(|p| p.1).sum::<u64>(),
        started.elapsed().as_secs_f64()
    );

    // ---- what each phase saw
    let mut phases: Vec<Tally> = opts.phases.iter().map(|_| Tally::default()).collect();
    let mut all = Tally::default();
    let mut cancels = Tally::default();
    for o in &outcomes {
        phases[o.plan.phase].add(&o.reply);
        all.add(&o.reply);
        if let Some(c) = &o.cancel {
            cancels.add(c);
        }
    }
    for (i, tally) in phases.iter_mut().enumerate() {
        let (rate, secs) = opts.phases[i];
        tally.print(&format!("phase {} ({rate}/s for {secs}s)", i + 1));
    }
    all.print("all bookings");
    cancels.print("cancels by winners");

    // ---- the client's own record of who holds what
    let mut by_key: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut reservations: HashMap<&str, (usize, &[String])> = HashMap::new();
    let mut cancelled: HashSet<&str> = HashSet::new();
    // Cancels with no answer: the reservation may or may not still hold
    // its seats, so it can't count as live (or as gone) below.
    let mut maybe_cancelled: HashSet<&str> = HashSet::new();
    let mut unsure: Vec<&[String]> = Vec::new(); // may or may not hold these seats
    for o in &outcomes {
        match (o.reply.status, o.reply.reservation_id()) {
            (Some(201), Some(id)) => {
                by_key.entry(&o.plan.key).or_default().insert(id);
                reservations.insert(id, (o.plan.user, &o.plan.seats));
            }
            _ if o.reply.outcome_unknown() => unsure.push(&o.plan.seats),
            _ => {}
        }
        if let (Some(c), Some(id)) = (&o.cancel, o.reply.reservation_id()) {
            match c.status {
                Some(200) => {
                    cancelled.insert(id);
                }
                _ if c.outcome_unknown() => {
                    maybe_cancelled.insert(id);
                    unsure.push(&o.plan.seats);
                }
                _ => {}
            }
        }
    }
    let mut holders: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut per_user: HashMap<usize, usize> = HashMap::new();
    for (id, (user, seats)) in &reservations {
        if cancelled.contains(id) || maybe_cancelled.contains(id) {
            continue;
        }
        *per_user.entry(*user).or_default() += seats.len();
        for seat in seats.iter() {
            holders.entry(seat).or_default().push(id);
        }
    }
    let double: Vec<&&str> = holders
        .iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(s, _)| s)
        .collect();
    let keys_split = by_key.values().filter(|ids| ids.len() > 1).count();
    let over_limit = per_user.values().filter(|&&n| n > PER_USER_LIMIT).count();
    let client_seats: BTreeSet<String> = holders.keys().map(|s| s.to_string()).collect();
    let unsure_seats: BTreeSet<&String> = unsure.iter().flat_map(|s| s.iter()).collect();

    let state = api.show_settled(&show).await?;
    let missing: Vec<&String> = client_seats
        .difference(&state.confirmed_seats)
        .filter(|s| !unsure_seats.contains(s))
        .collect();
    let extra: Vec<&String> = state
        .confirmed_seats
        .difference(&client_seats)
        .filter(|s| !unsure_seats.contains(s))
        .collect();
    let max_lag = outcomes.iter().map(|o| o.lag).max().unwrap_or_default();

    let r = &mut ctx.report;
    r.check(
        NAME,
        "no seat held by two live reservations",
        double.is_empty(),
        if double.is_empty() {
            format!("{} seats held", holders.len())
        } else {
            format!("double-booked: {:?}", &double[..double.len().min(5)])
        },
    );
    r.check(
        NAME,
        "each idempotency key maps to one reservation",
        keys_split == 0,
        format!("{keys_split} keys split"),
    );
    r.check(
        NAME,
        "no user over the per-show limit",
        over_limit == 0,
        format!("{over_limit} users over"),
    );
    r.check(
        NAME,
        "seat list matches what buyers were told",
        missing.is_empty() && extra.is_empty(),
        format!(
            "{} confirmed; {} missing, {} unexplained{}",
            state.confirmed,
            missing.len(),
            extra.len(),
            if unsure_seats.is_empty() {
                String::new()
            } else {
                format!(" ({} seats unknowable: no answer)", unsure_seats.len())
            }
        ),
    );
    r.check(
        NAME,
        "counts add up to total seats",
        state.counts_add_up(),
        format!(
            "{}+{}+{} of {}",
            state.available, state.held, state.confirmed, state.total_seats
        ),
    );
    r.check(
        NAME,
        "no 5xx or lost responses (bookings)",
        all.failures() == 0,
        format!("{}", all.failures()),
    );
    r.check(
        NAME,
        "no 5xx or lost responses (cancels)",
        cancels.failures() == 0,
        format!("{}", cancels.failures()),
    );
    r.check(
        NAME,
        "load generator kept to its schedule",
        max_lag <= MAX_SEND_LAG,
        format!("max lag {}ms", max_lag.as_millis()),
    );

    let ok = all.count("201");
    Ok(Totals {
        created: reservations.len() as u64,
        replayed: ok - reservations.len() as u64,
        ..Totals::default()
    }
    .with_declines(&all))
}

/// Users, seats, retries and cancels are decided up front from the seed,
/// so the same seed replays the same traffic shape.
fn make_plan(opts: &Options) -> Vec<Planned> {
    let mut rng = StdRng::seed_from_u64(opts.seed);
    let mut plan: Vec<Planned> = Vec::new();
    let mut offset = Duration::ZERO;
    for (phase, &(rate, secs)) in opts.phases.iter().enumerate() {
        for i in 0..rate * secs {
            let at = offset + Duration::from_secs_f64(i as f64 / rate as f64);
            let planned = if !plan.is_empty() && rng.random_bool(opts.retry_rate) {
                // A retry: same user, key and seats as an earlier request.
                let earlier = &plan[rng.random_range(0..plan.len())];
                Planned {
                    at,
                    phase,
                    cancel: false,
                    ..earlier.clone()
                }
            } else {
                let row = (b'A' + rng.random_range(0..opts.rows) as u8) as char;
                let n = rng.random_range(1..=opts.seats_per_row);
                let pair = n < opts.seats_per_row && rng.random_bool(1.0 / 3.0);
                let seats = if pair {
                    vec![format!("{row}{n}"), format!("{row}{}", n + 1)]
                } else {
                    vec![format!("{row}{n}")]
                };
                Planned {
                    at,
                    phase,
                    user: rng.random_range(0..opts.users),
                    key: Uuid::new_v4().to_string(),
                    seats,
                    cancel: rng.random_bool(opts.cancel_rate),
                }
            };
            plan.push(planned);
        }
        offset += Duration::from_secs(secs);
    }
    plan
}
