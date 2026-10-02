use std::time::Duration;

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::time::timeout;

use crate::metrics::{render_seat_gauges, ShowSeatCounts};
use crate::state::AppState;

const READY_TIMEOUT: Duration = Duration::from_secs(1);
const GAUGE_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
}

async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn readyz(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
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

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let mut body = String::new();
    state.metrics.render(&mut body);

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
