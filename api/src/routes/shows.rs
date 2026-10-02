use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::AppError;
use crate::models::{Reservation, Show, RESERVATION_COLUMNS, SHOW_COLUMNS};
use crate::state::AppState;

const MAX_ROWS: u32 = 26;
const MAX_SEATS_PER_ROW: u32 = 500;
const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;
/// Shows can't be edited after creation, so this only bounds memory use.
const SHOW_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
/// Writes delete the entry on commit; the TTL bounds the one race that
/// remains (a read that started before the commit re-filling it afterwards).
const SHOW_DETAIL_CACHE_TTL: Duration = Duration::from_secs(2);
const _: () = assert!(SHOW_DETAIL_CACHE_TTL.as_secs() < crate::cache::BREAKER_COOLDOWN.as_secs());

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/shows", post(create_show))
        .route("/shows/{id}", get(get_show))
        .route("/shows/{id}/reserve", post(reserve))
}

#[derive(Deserialize)]
struct CreateShowRequest {
    name: String,
    price_paise: i64,
    #[serde(default)]
    per_user_limit: Option<i32>,
    rows: u32,
    seats_per_row: u32,
}

async fn create_show(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<CreateShowRequest>,
) -> Result<(StatusCode, Json<Show>), AppError> {
    if !user.is_admin {
        return Err(AppError::Forbidden("admin role required"));
    }
    let name = req.name.trim();
    let per_user_limit = req.per_user_limit.unwrap_or(4);
    if name.is_empty() {
        return Err(AppError::Validation("name must not be empty".into()));
    }
    if req.price_paise < 0 {
        return Err(AppError::Validation("price_paise must be >= 0".into()));
    }
    if per_user_limit <= 0 {
        return Err(AppError::Validation("per_user_limit must be > 0".into()));
    }
    if !(1..=MAX_ROWS).contains(&req.rows) {
        return Err(AppError::Validation(format!("rows must be 1..={MAX_ROWS}")));
    }
    if !(1..=MAX_SEATS_PER_ROW).contains(&req.seats_per_row) {
        return Err(AppError::Validation(format!(
            "seats_per_row must be 1..={MAX_SEATS_PER_ROW}"
        )));
    }

    let labels: Vec<String> = (0..req.rows)
        .flat_map(|r| {
            let row = (b'A' + r as u8) as char;
            (1..=req.seats_per_row).map(move |n| format!("{row}{n}"))
        })
        .collect();

    let mut tx = state.pool.begin().await?;
    let show: Show = sqlx::query_as(&format!(
        "insert into shows (name, price_paise, per_user_limit, total_seats)
         values ($1, $2, $3, $4) returning {SHOW_COLUMNS}"
    ))
    .bind(name)
    .bind(req.price_paise)
    .bind(per_user_limit)
    .bind(labels.len() as i32)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("insert into seats (show_id, label) select $1, unnest($2::text[])")
        .bind(show.id)
        .bind(&labels)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(show)))
}

#[derive(Serialize, Deserialize, sqlx::FromRow)]
struct SeatView {
    label: String,
    status: String,
}

#[derive(Serialize, Deserialize, Default)]
struct SeatCounts {
    available: i64,
    held: i64,
    confirmed: i64,
}

#[derive(Serialize, Deserialize)]
struct ShowDetail {
    #[serde(flatten)]
    show: Show,
    counts: SeatCounts,
    seats: Vec<SeatView>,
}

fn show_key(show_id: Uuid) -> String {
    format!("show:{show_id}")
}

pub(super) fn show_detail_key(show_id: Uuid) -> String {
    format!("show:{show_id}:detail")
}

async fn load_show(state: &AppState, show_id: Uuid) -> Result<Show, AppError> {
    let key = show_key(show_id);
    if let Some(show) = state.cache.get_json(&key).await {
        return Ok(show);
    }
    let show = fetch_show(&state.pool, show_id).await?;
    state.cache.set_json(&key, &show, SHOW_CACHE_TTL).await;
    Ok(show)
}

async fn get_show(
    State(state): State<AppState>,
    Path(show_id): Path<Uuid>,
) -> Result<Json<ShowDetail>, AppError> {
    let key = show_detail_key(show_id);
    if let Some(detail) = state.cache.get_json(&key).await {
        return Ok(Json(detail));
    }
    let detail = build_show_detail(&state, show_id).await?;
    state.cache.set_json(&key, &detail, SHOW_DETAIL_CACHE_TTL).await;
    Ok(Json(detail))
}

async fn build_show_detail(state: &AppState, show_id: Uuid) -> Result<ShowDetail, AppError> {
    let show = load_show(state, show_id).await?;

    // Counts come from the same single-statement read as the seat list, so
    // they always sum to total_seats.
    let mut seats: Vec<SeatView> =
        sqlx::query_as("select label, status from seats where show_id = $1")
            .bind(show_id)
            .fetch_all(&state.pool)
            .await?;
    seats.sort_by(|a, b| seat_sort_key(&a.label).cmp(&seat_sort_key(&b.label)));

    let mut counts = SeatCounts::default();
    for seat in &seats {
        match seat.status.as_str() {
            "available" => counts.available += 1,
            "held" => counts.held += 1,
            _ => counts.confirmed += 1,
        }
    }

    Ok(ShowDetail {
        show,
        counts,
        seats,
    })
}

/// Row letters then seat number numerically, so A2 sorts before A10.
fn seat_sort_key(label: &str) -> (&str, u32) {
    let split = label
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(label.len());
    let (row, num) = label.split_at(split);
    (row, num.parse().unwrap_or(0))
}

#[derive(Deserialize)]
struct ReserveRequest {
    seats: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct KeyedReservation {
    request_hash: String,
    #[sqlx(flatten)]
    reservation: Reservation,
}

enum Reserved {
    Created(Reservation),
    Replayed(Reservation),
}

async fn reserve(
    State(state): State<AppState>,
    Path(show_id): Path<Uuid>,
    user: AuthUser,
    headers: HeaderMap,
    Json(req): Json<ReserveRequest>,
) -> Result<(StatusCode, Json<Reservation>), AppError> {
    let outcome = reserve_seats(&state, show_id, &user, &headers, req).await;
    match &outcome {
        Ok(Reserved::Created(_)) => state.metrics.record_confirmed(),
        Ok(Reserved::Replayed(_)) => state.metrics.record_declined("idempotent_replay"),
        Err(AppError::Conflict(reason)) => state.metrics.record_declined(reason),
        Err(_) => {}
    }
    let (Reserved::Created(reservation) | Reserved::Replayed(reservation)) = outcome?;
    Ok((StatusCode::CREATED, Json(reservation)))
}

async fn reserve_seats(
    state: &AppState,
    show_id: Uuid,
    user: &AuthUser,
    headers: &HeaderMap,
    req: ReserveRequest,
) -> Result<Reserved, AppError> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| AppError::Validation("Idempotency-Key header is required".into()))?;
    if key.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(AppError::Validation(format!(
            "Idempotency-Key must be at most {MAX_IDEMPOTENCY_KEY_LEN} characters"
        )));
    }

    let mut seats = req.seats;
    let requested = seats.len();
    seats.sort();
    seats.dedup();
    if seats.is_empty() {
        return Err(AppError::Validation("seats must not be empty".into()));
    }
    if seats.len() != requested {
        return Err(AppError::Validation("seats must not contain duplicates".into()));
    }
    let request_hash = request_hash(show_id, &seats);

    let show = load_show(state, show_id).await?;

    // A retry of a request that already succeeded must replay, not hit the
    // fast-path decline below (its seats are now confirmed — by this user).
    if let Some(existing) = find_by_key(&state.pool, &user.user_id, key).await? {
        return replay(existing, &request_hash);
    }

    if seats.len() > show.per_user_limit as usize {
        return Err(AppError::Conflict("per_user_limit"));
    }

    // Fast path, outside any transaction: it can only decline, so a stale
    // read is safe, and doomed requests never take a pooled write slot.
    let found: Vec<SeatView> = sqlx::query_as(
        "select label, status from seats where show_id = $1 and label = any($2)",
    )
    .bind(show_id)
    .bind(&seats)
    .fetch_all(&state.pool)
    .await?;
    if found.len() != seats.len() {
        let unknown: Vec<&str> = seats
            .iter()
            .filter(|s| !found.iter().any(|f| &f.label == *s))
            .map(String::as_str)
            .collect();
        return Err(AppError::Validation(format!(
            "unknown seats: {}",
            unknown.join(", ")
        )));
    }
    if found.iter().any(|s| s.status != "available") {
        return Err(AppError::Conflict("seat_taken"));
    }

    let n = seats.len() as i32;
    let mut tx = state.pool.begin().await?;

    // Lock order: idempotency key, then the user's counter, then seats by
    // label. Cancel follows the same order, so the two can't deadlock.
    let inserted: Option<Reservation> = sqlx::query_as(&format!(
        "insert into reservations
             (show_id, user_id, seats, amount_paise, status, idempotency_key, request_hash)
         values ($1, $2, $3, $4, 'confirmed', $5, $6)
         on conflict (user_id, idempotency_key) do nothing
         returning {RESERVATION_COLUMNS}"
    ))
    .bind(show_id)
    .bind(&user.user_id)
    .bind(&seats)
    .bind(show.price_paise * n as i64)
    .bind(key)
    .bind(&request_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(reservation) = inserted else {
        // A concurrent request with this key committed first.
        let existing = find_by_key(&mut *tx, &user.user_id, key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("idempotency conflict but no reservation row"))?;
        return replay(existing, &request_hash);
    };

    let held: Option<i32> = sqlx::query_scalar(
        "insert into user_show_holds (show_id, user_id, held) values ($1, $2, $3)
         on conflict (show_id, user_id) do update
             set held = user_show_holds.held + excluded.held
             where user_show_holds.held + excluded.held <= $4
         returning held",
    )
    .bind(show_id)
    .bind(&user.user_id)
    .bind(n)
    .bind(show.per_user_limit)
    .fetch_optional(&mut *tx)
    .await?;
    if held.is_none() {
        return Err(AppError::Conflict("per_user_limit"));
    }

    lock_seats(&mut tx, show_id, &seats).await?;
    let claimed = sqlx::query(
        "update seats set status = 'confirmed', reservation_id = $3
         where show_id = $1 and label = any($2) and status = 'available'",
    )
    .bind(show_id)
    .bind(&seats)
    .bind(reservation.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed != seats.len() as u64 {
        return Err(AppError::Conflict("seat_taken"));
    }

    tx.commit().await?;
    state.cache.delete(&show_detail_key(show_id)).await;
    Ok(Reserved::Created(reservation))
}

/// Locks the seat rows in label order before anything updates them; an
/// UPDATE alone locks in arbitrary order and can deadlock multi-seat requests.
pub(super) async fn lock_seats(
    conn: &mut PgConnection,
    show_id: Uuid,
    seats: &[String],
) -> Result<(), AppError> {
    sqlx::query(
        "select label from seats where show_id = $1 and label = any($2)
         order by label for update",
    )
    .bind(show_id)
    .bind(seats)
    .execute(conn)
    .await?;
    Ok(())
}

fn replay(existing: KeyedReservation, request_hash: &str) -> Result<Reserved, AppError> {
    if existing.request_hash == request_hash {
        Ok(Reserved::Replayed(existing.reservation))
    } else {
        Err(AppError::Conflict("idempotency_mismatch"))
    }
}

async fn find_by_key<'e>(
    executor: impl PgExecutor<'e>,
    user_id: &str,
    key: &str,
) -> Result<Option<KeyedReservation>, AppError> {
    Ok(sqlx::query_as(&format!(
        "select request_hash, {RESERVATION_COLUMNS} from reservations
         where user_id = $1 and idempotency_key = $2"
    ))
    .bind(user_id)
    .bind(key)
    .fetch_optional(executor)
    .await?)
}

async fn fetch_show<'e>(executor: impl PgExecutor<'e>, show_id: Uuid) -> Result<Show, AppError> {
    sqlx::query_as(&format!("select {SHOW_COLUMNS} from shows where id = $1"))
        .bind(show_id)
        .fetch_optional(executor)
        .await?
        .ok_or(AppError::NotFound("show not found"))
}

/// Seats arrive sorted and de-duplicated, so the same request always hashes
/// the same; JSON encoding keeps labels from running into each other.
fn request_hash(show_id: Uuid, sorted_seats: &[String]) -> String {
    let canonical = serde_json::to_vec(&(show_id, sorted_seats)).expect("serializable");
    hex::encode(Sha256::digest(&canonical))
}
