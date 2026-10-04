//! Burst: reproduces an on-sale stampede against a running seat reservation
//! service and checks, from the outside, that it stayed correct.
//!
//!     cargo run --release --bin burst -- http://localhost:8080
//!
//! Needs the service's test-token route (AUTH_TOKEN_ROUTE_ENABLED=true) and
//! no other traffic during the run: the final step compares the service's
//! counters with what this client saw.

mod api;
mod metrics;
mod report;
mod scenarios;

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};

use api::Api;
use metrics::Metrics;
use report::{Report, Totals};
use scenarios::{hot_seat, idempotency, per_user_limit, stampede, Ctx};

#[derive(Parser)]
#[command(about = "Reproduce an on-sale stampede and check the service stayed correct")]
struct Args {
    /// Base URL of the service, e.g. http://localhost:8080
    base_url: String,

    /// Which scenarios to run.
    #[arg(long, value_enum, default_values_t = [Scenario::All])]
    scenario: Vec<Scenario>,

    /// Stampede load as rate:seconds phases, sent at a fixed rate.
    #[arg(long, default_value = "2000:10,5000:10,2000:10", value_parser = parse_phases)]
    phases: Phases,

    /// Distinct buyers in the stampede.
    #[arg(long, default_value_t = 20_000)]
    users: usize,

    /// Stampede show size (26 x 500 = 13,000 seats, the service maximum).
    #[arg(long, default_value_t = 26)]
    rows: u32,
    #[arg(long, default_value_t = 500)]
    seats_per_row: u32,

    /// Share of stampede requests that are retries of an earlier request.
    #[arg(long, default_value_t = 0.05)]
    retry_rate: f64,

    /// Share of stampede winners that cancel straight away.
    #[arg(long, default_value_t = 0.02)]
    cancel_rate: f64,

    /// Buyers in the hot-seat storm, all for one seat at the same instant.
    #[arg(long, default_value_t = 500)]
    hot_buyers: usize,

    /// Seed for the stampede's traffic; random if not given.
    #[arg(long)]
    seed: Option<u64>,

    /// Per-request timeout.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Scenario {
    All,
    HotSeat,
    Idempotency,
    PerUserLimit,
    Stampede,
}

/// (requests per second, seconds) per phase.
#[derive(Clone)]
struct Phases(Vec<(u64, u64)>);

fn parse_phases(raw: &str) -> Result<Phases, String> {
    raw.split(',')
        .map(|phase| {
            let (rate, secs) = phase.split_once(':').ok_or("expected rate:seconds")?;
            let rate: u64 = rate
                .trim()
                .parse()
                .map_err(|_| format!("bad rate {rate:?}"))?;
            let secs: u64 = secs
                .trim()
                .parse()
                .map_err(|_| format!("bad seconds {secs:?}"))?;
            if rate == 0 || secs == 0 {
                return Err("rate and seconds must be > 0".into());
            }
            Ok((rate, secs))
        })
        .collect::<Result<_, _>>()
        .map(Phases)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match run(args).await {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("burst: {err:#}");
            ExitCode::from(2)
        }
    }
}

/// Returns the number of failed checks.
async fn run(args: Args) -> Result<usize> {
    let api = Api::new(&args.base_url, Duration::from_secs(args.timeout_secs))?;
    api.ready().await?;
    let run = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let admin = api.mint(&format!("burst-{run}-admin"), true).await?;
    println!("burst run {run} against {}", api.base());

    let before = api.metrics().await?;
    let mut ctx = Ctx {
        api,
        admin,
        run,
        report: Report::default(),
        shows: Vec::new(),
    };
    let wants = |s: Scenario| args.scenario.contains(&Scenario::All) || args.scenario.contains(&s);
    let mut totals = Totals::default();
    if wants(Scenario::HotSeat) {
        totals += hot_seat::run(&mut ctx, args.hot_buyers).await?;
    }
    if wants(Scenario::Idempotency) {
        totals += idempotency::run(&mut ctx, 50, 20).await?;
    }
    if wants(Scenario::PerUserLimit) {
        totals += per_user_limit::run(&mut ctx, 20).await?;
    }
    if wants(Scenario::Stampede) {
        let opts = stampede::Options {
            users: args.users,
            phases: args.phases.0.clone(),
            rows: args.rows,
            seats_per_row: args.seats_per_row,
            retry_rate: args.retry_rate,
            cancel_rate: args.cancel_rate,
            seed: args.seed.unwrap_or_else(rand::random),
        };
        if opts.rows == 0 || opts.rows > 26 || opts.seats_per_row < 2 || opts.seats_per_row > 500 {
            bail!("--rows must be 1..=26 and --seats-per-row 2..=500");
        }
        totals += stampede::run(&mut ctx, &opts).await?;
    }

    let after = ctx.api.metrics().await?;
    reconcile(&mut ctx, &before, &after, totals);
    ctx.report.print();
    Ok(ctx.report.failed())
}

/// The service's own counters must tell the same story as the client.
fn reconcile(ctx: &mut Ctx, before: &Metrics, after: &Metrics, seen: Totals) {
    const NAME: &str = "metrics";
    println!("\n== reconciliation with /metrics (assumes no other traffic during the run)");
    let delta = |series: &str| (after.get(series) - before.get(series)) as u64;
    let r = &mut ctx.report;
    let confirmed = delta("reservations_confirmed_total");
    // A request with no response may have booked: allow up to that many more.
    r.check(
        NAME,
        "reservations_confirmed_total matches new reservations",
        confirmed >= seen.created && confirmed <= seen.created + seen.unknown,
        format!(
            "server {confirmed}, client {}{}",
            seen.created,
            if seen.unknown > 0 {
                format!(" (+{} unknown)", seen.unknown)
            } else {
                String::new()
            }
        ),
    );
    for (reason, client) in [
        ("seat_taken", seen.seat_taken),
        ("per_user_limit", seen.per_user_limit),
        ("idempotency_mismatch", seen.idempotency_mismatch),
        ("idempotent_replay", seen.replayed),
    ] {
        let server = delta(&format!(
            "reservations_declined_total{{reason=\"{reason}\"}}"
        ));
        r.check(
            NAME,
            format!("declined {{reason=\"{reason}\"}} matches"),
            server == client,
            format!("server {server}, client {client}"),
        );
    }
    let server_errors = (after.sum("http_requests_total{status=\"5")
        - before.sum("http_requests_total{status=\"5")) as u64;
    r.check(
        NAME,
        "no 5xx counted by the service",
        server_errors == 0,
        format!("{server_errors}"),
    );
    let shed = delta("reserve_shed_total");
    r.check(
        NAME,
        "no requests shed for overload",
        shed == 0,
        format!("{shed}"),
    );
    let memory = delta("reserve_requests_total{path=\"memory\"}");
    let database = delta("reserve_requests_total{path=\"database\"}");
    println!("  reserve requests answered from memory: {memory}, by the database: {database}");

    for (scenario, show) in &ctx.shows {
        let confirmed = after.show("seats_confirmed", show);
        let map = after.show("seat_map_taken", show);
        r.check(
            NAME,
            format!("seat map agrees with database ({scenario})"),
            confirmed.is_some() && confirmed == map,
            format!(
                "map {}, db {}",
                map.unwrap_or(-1.0),
                confirmed.unwrap_or(-1.0)
            ),
        );
    }
}
