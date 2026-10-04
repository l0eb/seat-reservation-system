//! Outcome tallies, latency percentiles and pass/fail checks.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::api::Reply;

/// Responses grouped by label ("201", "409 seat_taken", ...), with latencies.
#[derive(Default)]
pub struct Tally {
    pub counts: BTreeMap<String, u64>,
    latencies: Vec<Duration>,
}

impl Tally {
    pub fn add(&mut self, reply: &Reply) {
        *self.counts.entry(reply.label()).or_default() += 1;
        self.latencies.push(reply.latency);
    }

    pub fn total(&self) -> u64 {
        self.counts.values().sum()
    }

    pub fn count(&self, label: &str) -> u64 {
        self.counts.get(label).copied().unwrap_or(0)
    }

    /// 5xx responses plus requests that got no response at all.
    pub fn failures(&self) -> u64 {
        self.counts
            .iter()
            .filter(|(label, _)| label.starts_with('5') || label.as_str() == "no response")
            .map(|(_, n)| n)
            .sum()
    }

    pub fn print(&mut self, title: &str) {
        self.latencies.sort_unstable();
        let pct = |q: f64| match self.latencies.len() {
            0 => 0.0,
            n => self.latencies[((n - 1) as f64 * q) as usize].as_secs_f64() * 1000.0,
        };
        println!(
            "  {title}: {} requests | p50 {:.1}ms  p95 {:.1}ms  p99 {:.1}ms  max {:.1}ms",
            self.total(),
            pct(0.50),
            pct(0.95),
            pct(0.99),
            pct(1.0)
        );
        for (label, n) in &self.counts {
            println!("      {label:<28} {n:>8}");
        }
    }
}

/// What the client saw, to reconcile against the server's own counters.
#[derive(Default, Clone, Copy)]
pub struct Totals {
    /// Distinct reservations created.
    pub created: u64,
    /// 201s that returned an existing reservation.
    pub replayed: u64,
    pub seat_taken: u64,
    pub per_user_limit: u64,
    pub idempotency_mismatch: u64,
    /// Reserve requests with no response: each may or may not have booked.
    pub unknown: u64,
}

impl std::ops::AddAssign for Totals {
    fn add_assign(&mut self, other: Self) {
        self.created += other.created;
        self.replayed += other.replayed;
        self.seat_taken += other.seat_taken;
        self.per_user_limit += other.per_user_limit;
        self.idempotency_mismatch += other.idempotency_mismatch;
        self.unknown += other.unknown;
    }
}

impl Totals {
    /// Fills in the decline counts from a tally of reserve responses.
    pub fn with_declines(mut self, tally: &Tally) -> Self {
        self.seat_taken = tally.count("409 seat_taken");
        self.per_user_limit = tally.count("409 per_user_limit");
        self.idempotency_mismatch = tally.count("409 idempotency_mismatch");
        self.unknown = tally.count("no response");
        self
    }
}

pub struct Check {
    scenario: &'static str,
    name: String,
    pass: bool,
    detail: String,
}

#[derive(Default)]
pub struct Report {
    checks: Vec<Check>,
}

impl Report {
    pub fn check(
        &mut self,
        scenario: &'static str,
        name: impl Into<String>,
        pass: bool,
        detail: impl Into<String>,
    ) {
        self.checks.push(Check {
            scenario,
            name: name.into(),
            pass,
            detail: detail.into(),
        });
    }

    pub fn failed(&self) -> usize {
        self.checks.iter().filter(|c| !c.pass).count()
    }

    pub fn print(&self) {
        println!("\nCHECKS");
        for check in &self.checks {
            let mark = if check.pass { "PASS" } else { "FAIL" };
            println!(
                "  [{mark}] {:<15} {:<52} {}",
                check.scenario, check.name, check.detail
            );
        }
        let failed = self.failed();
        println!(
            "\n{} of {} checks passed{}",
            self.checks.len() - failed,
            self.checks.len(),
            if failed == 0 { "" } else { " — FAILED" }
        );
    }
}
