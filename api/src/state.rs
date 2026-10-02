use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::metrics::Metrics;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub metrics: Arc<Metrics>,
    /// Bounds how many requests enter the reserve handler at once, ahead of
    /// the DB pool itself, so a hot-seat storm degrades to queued latency
    /// instead of exhausting connections for unrelated requests.
    pub reserve_semaphore: Arc<Semaphore>,
}
