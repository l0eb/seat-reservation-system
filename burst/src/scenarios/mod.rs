pub mod hot_seat;
pub mod idempotency;
pub mod per_user_limit;
pub mod stampede;

use crate::api::Api;
use crate::report::Report;

pub struct Ctx {
    pub api: Api,
    pub admin: String,
    /// Short id that keeps this run's users and shows apart from other runs.
    pub run: String,
    pub report: Report,
    /// Every show this run created, for the end-of-run reconciliation.
    pub shows: Vec<(&'static str, String)>,
}

impl Ctx {
    pub fn users(&self, scenario: &str) -> String {
        format!("burst-{}-{scenario}", self.run)
    }

    pub fn show_name(&self, scenario: &str) -> String {
        format!("burst {} {scenario}", self.run)
    }
}

/// Seat labels A1..A{n} in one row.
pub fn row_a(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("A{i}")).collect()
}
