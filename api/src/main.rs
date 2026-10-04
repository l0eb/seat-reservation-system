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
mod single_flight;
mod state;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderName, Request};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::{DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tower_http::LatencyUnit;
use tracing::Level;
use tracing_subscriber::EnvFilter;

use cache::Cache;
use config::Config;
use metrics::Metrics;
use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    // The runtime image has no shell or curl, so the container healthcheck
    // runs the binary itself: `api healthcheck` exits 0 iff /readyz is 200.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
        std::process::exit(match healthcheck(&port).await {
            Ok(true) => 0,
            _ => 1,
        });
    }

    // Log lines go to a background thread, so a request never waits on
    // stdout; if it ever falls 128k lines behind, lines are dropped rather
    // than slowing bookings. The guard flushes what's queued on exit.
    let (log_writer, _log_guard) = tracing_appender::non_blocking(std::io::stdout());
    tracing_subscriber::fmt()
        .with_writer(log_writer)
        .json()
        // The current span (request id, method, path, replica) is enough;
        // the full span list would repeat it on every line.
        .with_span_list(false)
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
    let pool_options = PgPoolOptions::new()
        .max_connections(config.db_pool_max_connections)
        .acquire_timeout(Duration::from_secs(config.db_acquire_timeout_secs));
    let pool = connect_with_retry(pool_options, connect_options).await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    // Loads the seat map and keeps it in step with the other replicas.
    let seat_map = seat_map::listen(pool.clone()).await?;
    let metrics = Arc::new(Metrics::new());
    let cache = Cache::connect(config.cache_url.as_deref(), metrics.clone()).await?;
    tracing::info!(enabled = cache.enabled(), "cache");

    let state = AppState {
        pool,
        cache,
        reserve_semaphore: Arc::new(Semaphore::new(config.reserve_semaphore_permits)),
        show_read_semaphore: Arc::new(Semaphore::new(config.show_read_permits)),
        show_reads: Arc::new(single_flight::SingleFlight::new()),
        config: Arc::new(config.clone()),
        metrics,
        seat_map,
        http: reqwest::Client::new(),
        draining: Arc::new(AtomicBool::new(false)),
    };
    let draining = state.draining.clone();
    let drain_for = Duration::from_secs(state.config.shutdown_drain_secs);

    let request_id_header = HeaderName::from_static("x-request-id");
    let replica = state.config.replica_id.clone();

    // One JSON line per request ("finished processing request", with status
    // and latency), inside a span that carries the request id (the same one
    // returned in the x-request-id header), method, path and replica, so a
    // response can be traced to its log line and every line inside it.
    // Quieten with RUST_LOG=info,tower_http=warn.
    let trace = TraceLayer::new_for_http()
        .make_span_with(move |req: &Request<Body>| {
            let request_id = req
                .headers()
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-");
            tracing::info_span!(
                "request",
                request_id,
                method = %req.method(),
                path = %req.uri().path(),
                replica = %replica,
            )
        })
        .on_response(
            DefaultOnResponse::new()
                .level(Level::INFO)
                .latency_unit(LatencyUnit::Millis),
        )
        // 5xx here are mostly deliberate 503s when shedding load; real
        // internal errors log their own ERROR line in AppError.
        .on_failure(DefaultOnFailure::new().level(Level::WARN));

    let app = routes::router(state)
        .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
        .layer(trace)
        .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid));

    let listener = TcpListener::bind(("0.0.0.0", config.port)).await?;
    tracing::info!(port = config.port, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(draining, drain_for))
        .await?;
    tracing::info!("shut down");

    Ok(())
}

/// On a cold start the database may come up after the service (a fresh
/// compose stack, a managed Postgres waking up). Try 15 times, 2s apart
/// (each attempt can also wait out the acquire timeout), before exiting
/// and leaving it to the platform's restart policy.
async fn connect_with_retry(
    pool_options: PgPoolOptions,
    connect_options: PgConnectOptions,
) -> anyhow::Result<sqlx::PgPool> {
    let mut attempt = 1;
    loop {
        match pool_options
            .clone()
            .connect_with(connect_options.clone())
            .await
        {
            Ok(pool) => return Ok(pool),
            Err(e) if attempt < 15 => {
                tracing::warn!(attempt, error = %e, "database not reachable yet, retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
                attempt += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// On SIGTERM (what a platform sends on deploy) or Ctrl-C: fail /readyz
/// for `drain_for` while still serving, so a load balancer's health check
/// stops sending traffic here; then stop accepting and let in-flight
/// requests finish. Without the first step, a request can reach a
/// connection that is closing and come back as a 502.
async fn shutdown_signal(draining: Arc<AtomicBool>, drain_for: Duration) {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    draining.store(true, Ordering::Relaxed);
    tracing::info!(
        drain_secs = drain_for.as_secs(),
        "shutdown signal received, draining"
    );
    tokio::time::sleep(drain_for).await;
}

async fn healthcheck(port: &str) -> std::io::Result<bool> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let check = async {
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await?;
        stream
            .write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut head = [0u8; 12];
        stream.read_exact(&mut head).await?;
        Ok(head.ends_with(b" 200"))
    };
    tokio::time::timeout(Duration::from_secs(3), check)
        .await
        .unwrap_or(Ok(false))
}
