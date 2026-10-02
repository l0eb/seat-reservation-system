use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

use crate::state::AppState;

pub const DECLINE_REASONS: [&str; 4] = [
    "seat_taken",
    "per_user_limit",
    "idempotent_replay",
    "idempotency_mismatch",
];

/// In-process counters. Exact only because the service runs as a single
/// instance; seat gauges are read from the database at scrape time instead.
pub struct Metrics {
    reservations_confirmed: AtomicU64,
    reservations_declined: [AtomicU64; DECLINE_REASONS.len()],
    /// Indexed by HTTP status code.
    http_requests: Vec<AtomicU64>,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            reservations_confirmed: AtomicU64::new(0),
            reservations_declined: Default::default(),
            http_requests: (0..600).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    pub fn record_confirmed(&self) {
        self.reservations_confirmed.fetch_add(1, Relaxed);
    }

    pub fn record_declined(&self, reason: &str) {
        if let Some(i) = DECLINE_REASONS.iter().position(|r| *r == reason) {
            self.reservations_declined[i].fetch_add(1, Relaxed);
        }
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
        for (reason, counter) in DECLINE_REASONS.iter().zip(&self.reservations_declined) {
            let n = counter.load(Relaxed);
            let _ = writeln!(out, "reservations_declined_total{{reason=\"{reason}\"}} {n}");
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

pub fn render_seat_gauges(out: &mut String, shows: &[ShowSeatCounts]) {
    let gauges: [(&str, &str, fn(&ShowSeatCounts) -> i64); 3] = [
        ("seats_available", "Seats available, per show.", |s| s.available),
        ("seats_held", "Seats held, per show.", |s| s.held),
        ("seats_confirmed", "Seats confirmed, per show.", |s| s.confirmed),
    ];
    for (name, help, value) in gauges {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        for show in shows {
            let _ = writeln!(out, "{name}{{show=\"{}\"}} {}", show.show_id, value(show));
        }
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
        m.record_declined("seat_taken");
        m.record_declined("seat_taken");
        m.record_declined("not_a_reason");
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
            &[ShowSeatCounts { show_id: id, available: 7, held: 0, confirmed: 3 }],
        );
        assert!(out.contains(&format!("seats_available{{show=\"{id}\"}} 7\n")));
        assert!(out.contains(&format!("seats_confirmed{{show=\"{id}\"}} 3\n")));
    }
}
