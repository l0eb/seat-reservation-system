mod auth;
mod cache;
mod config;
mod error;
mod extract;
mod idempotency;
mod metrics;
mod models;
mod routes;
mod seat_map;
mod state;

use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderName;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::{DefaultOnFailure, TraceLayer};
use tracing::Level;
use tracing_subscriber::EnvFilter;

use cache::Cache;
use config::Config;
use metrics::Metrics;
use seat_map::SeatMap;
use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;

    // A statement stuck on a lock can't hold a connection (and a reserve
    // permit) forever.
    let statement_timeout = format!("{}s", config.db_statement_timeout_secs);
    let connect_options = config
        .database_url
        .parse::<PgConnectOptions>()?
        .options([("statement_timeout", statement_timeout.as_str())]);
    let pool = PgPoolOptions::new()
        .max_connections(config.db_pool_max_connections)
        .acquire_timeout(Duration::from_secs(config.db_acquire_timeout_secs))
        .connect_with(connect_options)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    let seat_map = Arc::new(SeatMap::load(&pool).await?);
    let metrics = Arc::new(Metrics::new());
    let cache = Cache::connect(config.cache_url.as_deref(), metrics.clone()).await?;
    tracing::info!(enabled = cache.enabled(), "cache");

    let state = AppState {
        pool,
        cache,
        reserve_semaphore: Arc::new(Semaphore::new(config.reserve_semaphore_permits)),
        config: Arc::new(config.clone()),
        metrics,
        seat_map,
    };

    let request_id_header = HeaderName::from_static("x-request-id");

    let app = routes::router(state)
        .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
        // 5xx here are mostly deliberate 503s when shedding load; real
        // internal errors log their own ERROR line in AppError.
        .layer(TraceLayer::new_for_http().on_failure(DefaultOnFailure::new().level(Level::WARN)))
        .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid));

    let listener = TcpListener::bind(("0.0.0.0", config.port)).await?;
    tracing::info!(port = config.port, "listening");
    axum::serve(listener, app).await?;

    Ok(())
}
