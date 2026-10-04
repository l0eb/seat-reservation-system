use std::fmt::Write;
use std::time::Duration;

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::time::timeout;

use crate::metrics::{
    merge, render_reserve_inflight, render_seat_gauges, render_seat_map, ShowSeatCounts,
};
use crate::state::AppState;

const READY_TIMEOUT: Duration = Duration::from_secs(1);
const GAUGE_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// A replica slower than this is left out of the totals (replica_up 0).
const PEER_TIMEOUT: Duration = Duration::from_millis(500);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/metrics/local", get(metrics_local))
}

async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn readyz(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    if state.draining.load(std::sync::atomic::Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "draining" })),
        );
    }
    let ready = matches!(
        timeout(READY_TIMEOUT, sqlx::query("select 1").execute(&state.pool)).await,
        Ok(Ok(_))
    );
    if ready {
        (StatusCode::OK, Json(json!({ "status": "ready" })))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "database unavailable" })),
        )
    }
}

/// This replica's own counters and gauges: what a per-replica Prometheus
/// scrape should read, and what /metrics on another replica adds up.
async fn metrics_local(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4")],
        local_metrics(&state),
    )
}

fn local_metrics(state: &AppState) -> String {
    let mut body = String::new();
    state.metrics.render(&mut body);
    let permits = state.config.reserve_semaphore_permits;
    render_reserve_inflight(
        &mut body,
        permits - state.reserve_semaphore.available_permits(),
    );
    render_seat_map(&mut body, &state.seat_map.taken_counts());
    // Counters reset when a replica restarts; this tells a reader whether
    // two readings of /metrics can be subtracted.
    body.push_str(
        "# HELP process_start_time_seconds When this replica started, in Unix seconds.\n",
    );
    body.push_str("# TYPE process_start_time_seconds gauge\n");
    let _ = writeln!(
        body,
        "process_start_time_seconds {}",
        state.metrics.started_at()
    );
    body.push_str("# HELP replica_info The replica that produced these numbers.\n");
    body.push_str("# TYPE replica_info gauge\n");
    let _ = writeln!(
        body,
        "replica_info{{replica=\"{}\"}} 1",
        state.config.replica_id
    );
    body
}

/// The whole service, whichever replica answers: every replica's counters
/// added up (see `merge`), plus seat gauges read from the database.
async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let fetches = state.config.peers.iter().map(|peer| {
        let request = state
            .http
            .get(format!("{peer}/metrics/local"))
            .timeout(PEER_TIMEOUT)
            .send();
        async move {
            let text = match request.await {
                Ok(response) if response.status().is_success() => response.text().await.ok(),
                _ => None,
            };
            (peer.clone(), text)
        }
    });
    let peers = futures::future::join_all(fetches).await;
    let mut replicas = vec![(state.config.replica_id.clone(), Some(local_metrics(&state)))];
    // Name each answering peer by its own REPLICA_ID, from its local text.
    replicas.extend(peers.into_iter().map(|(url, text)| {
        let id = text.as_deref().and_then(replica_id_of).unwrap_or(url);
        (id, text)
    }));
    let mut body = merge(&replicas);

    // One statement reads one snapshot, so each show's gauges sum to its
    // total_seats. If the database is slow or down, still serve the counters.
    let query = sqlx::query_as::<_, ShowSeatCounts>(
        "select show_id,
                count(*) filter (where status = 'available') as available,
                count(*) filter (where status = 'held') as held,
                count(*) filter (where status = 'confirmed') as confirmed
         from seats group by show_id",
    )
    .fetch_all(&state.pool);
    match timeout(GAUGE_QUERY_TIMEOUT, query).await {
        Ok(Ok(rows)) => render_seat_gauges(&mut body, &rows),
        Ok(Err(err)) => tracing::warn!(error = %err, "seat gauges unavailable"),
        Err(_) => tracing::warn!("seat gauge query timed out"),
    }

    ([(CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}

fn replica_id_of(local: &str) -> Option<String> {
    let line = local.lines().find(|l| l.starts_with("replica_info{"))?;
    let start = line.find("replica=\"")? + "replica=\"".len();
    let end = start + line[start..].find('"')?;
    Some(line[start..end].to_string())
}
