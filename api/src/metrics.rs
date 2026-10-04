use std::collections::HashMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

use crate::state::AppState;

/// Reserve requests that confirmed nothing new. A replay returns 201 but
/// still counts here, since it books no seats.
#[derive(Debug, Clone, Copy)]
pub enum Decline {
    SeatTaken,
    PerUserLimit,
    IdempotentReplay,
    IdempotencyMismatch,
}

impl Decline {
    const ALL: [Decline; 4] = [
        Decline::SeatTaken,
        Decline::PerUserLimit,
        Decline::IdempotentReplay,
        Decline::IdempotencyMismatch,
    ];

    fn label(self) -> &'static str {
        match self {
            Decline::SeatTaken => "seat_taken",
            Decline::PerUserLimit => "per_user_limit",
            Decline::IdempotentReplay => "idempotent_replay",
            Decline::IdempotencyMismatch => "idempotency_mismatch",
        }
    }
}

pub const CACHE_RESULTS: [&str; 4] = ["hit", "miss", "error", "bypassed"];

/// Where a reserve request was answered: the in-memory seat map declined it,
/// or it went to Postgres.
#[derive(Debug, Clone, Copy)]
pub enum ReservePath {
    Memory,
    Database,
}

const RESERVE_PATHS: [&str; 2] = ["memory", "database"];
const RETRY_RESULTS: [&str; 2] = ["recovered", "failed"];

/// In-process counters, one set per replica: /metrics adds up every
/// replica's (see `merge`). Seat gauges are read from the database at
/// scrape time instead.
pub struct Metrics {
    reservations_confirmed: AtomicU64,
    reservations_declined: [AtomicU64; Decline::ALL.len()],
    cache_lookups: [AtomicU64; CACHE_RESULTS.len()],
    reserve_paths: [AtomicU64; RESERVE_PATHS.len()],
    reserve_shed: AtomicU64,
    reserve_retries: [AtomicU64; RETRY_RESULTS.len()],
    /// Indexed by HTTP status code.
    http_requests: Vec<AtomicU64>,
    /// Unix seconds; counters count from here.
    started_at: u64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            reservations_confirmed: AtomicU64::new(0),
            reservations_declined: Default::default(),
            cache_lookups: Default::default(),
            reserve_paths: Default::default(),
            reserve_shed: AtomicU64::new(0),
            reserve_retries: Default::default(),
            http_requests: (0..600).map(|_| AtomicU64::new(0)).collect(),
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    pub fn started_at(&self) -> u64 {
        self.started_at
    }

    pub fn record_confirmed(&self) {
        self.reservations_confirmed.fetch_add(1, Relaxed);
    }

    pub fn record_declined(&self, reason: Decline) {
        self.reservations_declined[reason as usize].fetch_add(1, Relaxed);
    }

    pub fn record_cache(&self, result: &str) {
        if let Some(i) = CACHE_RESULTS.iter().position(|r| *r == result) {
            self.cache_lookups[i].fetch_add(1, Relaxed);
        }
    }

    pub fn record_reserve_path(&self, path: ReservePath) {
        self.reserve_paths[path as usize].fetch_add(1, Relaxed);
    }

    pub fn record_shed(&self) {
        self.reserve_shed.fetch_add(1, Relaxed);
    }

    pub fn record_retry(&self, recovered: bool) {
        self.reserve_retries[usize::from(!recovered)].fetch_add(1, Relaxed);
    }

    fn record_http_status(&self, status: u16) {
        if let Some(counter) = self.http_requests.get(status as usize) {
            counter.fetch_add(1, Relaxed);
        }
    }

    pub fn render(&self, out: &mut String) {
        out.push_str("# HELP reservations_confirmed_total Reservations confirmed.\n");
        out.push_str("# TYPE reservations_confirmed_total counter\n");
        let confirmed = self.reservations_confirmed.load(Relaxed);
        let _ = writeln!(out, "reservations_confirmed_total {confirmed}");

        out.push_str("# HELP reservations_declined_total Reserve requests that confirmed nothing new, by reason.\n");
        out.push_str("# TYPE reservations_declined_total counter\n");
        for reason in Decline::ALL {
            let n = self.reservations_declined[reason as usize].load(Relaxed);
            let label = reason.label();
            let _ = writeln!(out, "reservations_declined_total{{reason=\"{label}\"}} {n}");
        }

        out.push_str("# HELP cache_lookups_total Cache reads, by result.\n");
        out.push_str("# TYPE cache_lookups_total counter\n");
        for (result, counter) in CACHE_RESULTS.iter().zip(&self.cache_lookups) {
            let n = counter.load(Relaxed);
            let _ = writeln!(out, "cache_lookups_total{{result=\"{result}\"}} {n}");
        }

        out.push_str("# HELP reserve_requests_total Reserve requests that got past input checks, by where they were answered.\n");
        out.push_str("# TYPE reserve_requests_total counter\n");
        for (path, counter) in RESERVE_PATHS.iter().zip(&self.reserve_paths) {
            let n = counter.load(Relaxed);
            let _ = writeln!(out, "reserve_requests_total{{path=\"{path}\"}} {n}");
        }

        out.push_str("# HELP reserve_shed_total Reserve requests turned away with 503 after waiting too long for a database slot.\n");
        out.push_str("# TYPE reserve_shed_total counter\n");
        let shed = self.reserve_shed.load(Relaxed);
        let _ = writeln!(out, "reserve_shed_total {shed}");

        out.push_str("# HELP reserve_retries_total Booking transactions retried after a serialization failure or deadlock, by outcome.\n");
        out.push_str("# TYPE reserve_retries_total counter\n");
        for (result, counter) in RETRY_RESULTS.iter().zip(&self.reserve_retries) {
            let n = counter.load(Relaxed);
            let _ = writeln!(out, "reserve_retries_total{{result=\"{result}\"}} {n}");
        }

        out.push_str("# HELP http_requests_total HTTP responses, by status code.\n");
        out.push_str("# TYPE http_requests_total counter\n");
        for (status, counter) in self.http_requests.iter().enumerate() {
            let n = counter.load(Relaxed);
            if n > 0 {
                let _ = writeln!(out, "http_requests_total{{status=\"{status}\"}} {n}");
            }
        }
    }
}

#[derive(sqlx::FromRow)]
pub struct ShowSeatCounts {
    pub show_id: Uuid,
    pub available: i64,
    pub held: i64,
    pub confirmed: i64,
}

/// A per-show gauge: metric name, help text, and how to read it.
type SeatGauge = (&'static str, &'static str, fn(&ShowSeatCounts) -> i64);

pub fn render_seat_gauges(out: &mut String, shows: &[ShowSeatCounts]) {
    let gauges: [SeatGauge; 3] = [
        ("seats_available", "Seats available, per show.", |s| {
            s.available
        }),
        ("seats_held", "Seats held, per show.", |s| s.held),
        ("seats_confirmed", "Seats confirmed, per show.", |s| {
            s.confirmed
        }),
    ];
    for (name, help, value) in gauges {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        for show in shows {
            let _ = writeln!(out, "{name}{{show=\"{}\"}} {}", show.show_id, value(show));
        }
    }
}

pub fn render_reserve_inflight(out: &mut String, inflight: usize) {
    out.push_str("# HELP reserve_inflight Reserve requests using the database right now.\n");
    out.push_str("# TYPE reserve_inflight gauge\n");
    let _ = writeln!(out, "reserve_inflight {inflight}");
}

/// Must equal seats_confirmed for each show whenever no booking is mid-flight.
pub fn render_seat_map(out: &mut String, taken: &[(Uuid, i64)]) {
    out.push_str("# HELP seat_map_taken Seats the in-memory seat map holds as taken, per show.\n");
    out.push_str("# TYPE seat_map_taken gauge\n");
    for (show, n) in taken {
        let _ = writeln!(out, "seat_map_taken{{show=\"{show}\"}} {n}");
    }
}

/// Gauges that describe one process and mean nothing summed: these keep a
/// `replica` label in the merged output instead.
const PER_REPLICA: [&str; 3] = [
    "reserve_inflight",
    "seat_map_taken",
    "process_start_time_seconds",
];

/// Every replica's /metrics/local in one exposition: counters summed across
/// replicas, per-replica gauges labelled with their replica, and
/// `replica_up` saying whose numbers are in the totals. A replica that
/// didn't answer is `None`, named by its URL.
pub fn merge(replicas: &[(String, Option<String>)]) -> String {
    struct Family {
        header: Vec<String>,
        samples: Vec<String>,
    }
    let mut order: Vec<String> = Vec::new();
    let mut families: HashMap<String, Family> = HashMap::new();
    let mut totals: HashMap<String, u64> = HashMap::new();

    for (replica, text) in replicas {
        let Some(text) = text else { continue };
        let mut current = String::new();
        for line in text.lines() {
            if let Some(rest) = line
                .strip_prefix("# HELP ")
                .or(line.strip_prefix("# TYPE "))
            {
                current = rest.split(' ').next().unwrap_or_default().to_string();
                let family = families.entry(current.clone()).or_insert_with(|| {
                    order.push(current.clone());
                    Family {
                        header: Vec::new(),
                        samples: Vec::new(),
                    }
                });
                if family.header.len() < 2 && !family.header.iter().any(|h| h == line) {
                    family.header.push(line.to_string());
                }
                continue;
            }
            let Some((series, value)) = line.rsplit_once(' ') else {
                continue;
            };
            let Ok(value) = value.parse::<u64>() else {
                continue;
            };
            let Some(family) = families.get_mut(&current) else {
                continue;
            };
            let series = if PER_REPLICA.contains(&current.as_str()) {
                with_label(series, "replica", replica)
            } else {
                series.to_string()
            };
            match totals.get_mut(&series) {
                Some(total) if !PER_REPLICA.contains(&current.as_str()) => *total += value,
                Some(_) => {}
                None => {
                    family.samples.push(series.clone());
                    totals.insert(series, value);
                }
            }
        }
    }

    let mut out = String::new();
    out.push_str("# HELP replica_up Replicas whose counters are in the totals below (1), or that did not answer (0).\n");
    out.push_str("# TYPE replica_up gauge\n");
    for (replica, text) in replicas {
        let up = u8::from(text.is_some());
        let _ = writeln!(out, "replica_up{{replica=\"{replica}\"}} {up}");
    }
    for name in order {
        let family = &families[&name];
        for line in &family.header {
            let _ = writeln!(out, "{line}");
        }
        for series in &family.samples {
            let _ = writeln!(out, "{series} {}", totals[series]);
        }
    }
    out
}

/// `name{a="1"}` + b="2" -> `name{a="1",b="2"}`.
fn with_label(series: &str, key: &str, value: &str) -> String {
    match series.strip_suffix('}') {
        Some(open) => format!("{open},{key}=\"{value}\"}}"),
        None => format!("{series}{{{key}=\"{value}\"}}"),
    }
}

pub async fn track_status(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let response = next.run(req).await;
    state.metrics.record_http_status(response.status().as_u16());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counters_with_zeroed_reasons() {
        let m = Metrics::new();
        m.record_confirmed();
        m.record_declined(Decline::SeatTaken);
        m.record_declined(Decline::SeatTaken);
        m.record_http_status(201);
        let mut out = String::new();
        m.render(&mut out);
        assert!(out.contains("reservations_confirmed_total 1\n"));
        assert!(out.contains("reservations_declined_total{reason=\"seat_taken\"} 2\n"));
        assert!(out.contains("reservations_declined_total{reason=\"per_user_limit\"} 0\n"));
        assert!(out.contains("http_requests_total{status=\"201\"} 1\n"));
        assert!(!out.contains("status=\"200\""));
    }

    #[test]
    fn renders_seat_gauges_per_show() {
        let id = Uuid::nil();
        let mut out = String::new();
        render_seat_gauges(
            &mut out,
            &[ShowSeatCounts {
                show_id: id,
                available: 7,
                held: 0,
                confirmed: 3,
            }],
        );
        assert!(out.contains(&format!("seats_available{{show=\"{id}\"}} 7\n")));
        assert!(out.contains(&format!("seats_confirmed{{show=\"{id}\"}} 3\n")));
    }

    #[test]
    fn merge_sums_counters_and_labels_per_replica_gauges() {
        let a = "# HELP c_total C.\n# TYPE c_total counter\nc_total{reason=\"x\"} 2\n\
                 # HELP seat_map_taken M.\n# TYPE seat_map_taken gauge\nseat_map_taken{show=\"s\"} 5\n";
        let b = "# HELP c_total C.\n# TYPE c_total counter\nc_total{reason=\"x\"} 3\nc_total{reason=\"y\"} 1\n\
                 # HELP seat_map_taken M.\n# TYPE seat_map_taken gauge\nseat_map_taken{show=\"s\"} 5\n";
        let out = merge(&[
            ("api-1".into(), Some(a.into())),
            ("api-2".into(), Some(b.into())),
            ("http://api-3:8080".into(), None),
        ]);
        assert!(out.contains("c_total{reason=\"x\"} 5\n"));
        assert!(out.contains("c_total{reason=\"y\"} 1\n"));
        assert!(out.contains("seat_map_taken{show=\"s\",replica=\"api-1\"} 5\n"));
        assert!(out.contains("seat_map_taken{show=\"s\",replica=\"api-2\"} 5\n"));
        assert!(out.contains("replica_up{replica=\"http://api-3:8080\"} 0\n"));
        assert_eq!(out.matches("# TYPE c_total counter").count(), 1);
    }

    #[test]
    fn unlabelled_series_get_a_replica_label() {
        assert_eq!(
            with_label("reserve_inflight", "replica", "a"),
            "reserve_inflight{replica=\"a\"}"
        );
    }
}
