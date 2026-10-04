//! Just enough Prometheus text parsing to compare counters before and after.

use std::collections::HashMap;

pub struct Metrics(HashMap<String, f64>);

impl Metrics {
    pub fn parse(text: &str) -> Self {
        let series = text
            .lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| {
                let (name, value) = line.rsplit_once(' ')?;
                Some((name.to_string(), value.parse().ok()?))
            })
            .collect();
        Self(series)
    }

    /// A series by its full name, e.g. `reserve_shed_total` or
    /// `reservations_declined_total{reason="seat_taken"}`. Missing is 0.
    pub fn get(&self, series: &str) -> f64 {
        self.0.get(series).copied().unwrap_or(0.0)
    }

    /// Sum of every series whose name starts with `prefix`.
    pub fn sum(&self, prefix: &str) -> f64 {
        self.0
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .map(|(_, v)| v)
            .sum()
    }

    pub fn show(&self, name: &str, show: &str) -> Option<f64> {
        self.0.get(&format!("{name}{{show=\"{show}\"}}")).copied()
    }
}
