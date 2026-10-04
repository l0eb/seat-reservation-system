use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::cache::Cache;
use crate::config::Config;
use crate::metrics::Metrics;
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
}
