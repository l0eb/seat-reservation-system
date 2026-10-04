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

    /// A per-replica gauge for one show, e.g. `seat_map_taken`: each
    /// replica's value, sorted by replica.
    pub fn show_per_replica(&self, name: &str, show: &str) -> Vec<(String, f64)> {
        let prefix = format!("{name}{{show=\"{show}\",replica=\"");
        let mut values: Vec<(String, f64)> = self
            .0
            .iter()
            .filter_map(|(series, v)| {
                let replica = series.strip_prefix(&prefix)?.strip_suffix("\"}")?;
                Some((replica.to_string(), *v))
            })
            .collect();
        values.sort_by(|a, b| a.0.cmp(&b.0));
        values
    }

    /// Each replica's start time (Unix seconds), from
    /// `process_start_time_seconds{replica=..}`.
    pub fn start_times(&self) -> std::collections::BTreeMap<String, f64> {
        self.0
            .iter()
            .filter_map(|(series, v)| {
                let replica = series.strip_prefix("process_start_time_seconds{replica=\"")?;
                Some((replica.strip_suffix("\"}")?.to_string(), *v))
            })
            .collect()
    }

    /// Replicas whose counters are missing from the totals.
    pub fn replicas_down(&self) -> Vec<String> {
        self.0
            .iter()
            .filter(|(_, v)| **v == 0.0)
            .filter_map(|(series, _)| {
                let replica = series.strip_prefix("replica_up{replica=\"")?;
                Some(replica.strip_suffix("\"}")?.to_string())
            })
            .collect()
    }
}
