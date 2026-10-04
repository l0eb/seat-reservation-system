use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::cache::Cache;
use crate::config::Config;
use crate::metrics::Metrics;
use crate::routes::shows::ShowReads;
use crate::seat_map::SeatMap;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cache: Cache,
    pub config: Arc<Config>,
    pub metrics: Arc<Metrics>,
    pub seat_map: Arc<SeatMap>,
    /// Bounds how many reserve requests use the database at once. Sized
    /// below the pool so reads, /readyz and /metrics always find a
    /// connection; waiters are served in arrival order.
    pub reserve_semaphore: Arc<Semaphore>,
    /// Bounds GET /shows/{id} rebuilds the same way, so page reads never
    /// take the connections bookings need.
    pub show_read_semaphore: Arc<Semaphore>,
    /// Concurrent GET /shows/{id} cache misses for one show share one read.
    pub show_reads: Arc<ShowReads>,
    /// For fetching the other replicas' counters.
    pub http: reqwest::Client,
    /// Set on SIGTERM: /readyz fails so the load balancer moves traffic
    /// away before this replica stops.
    pub draining: Arc<AtomicBool>,
}
