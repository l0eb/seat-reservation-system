use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::{AppError, Conflict};
use crate::extract::{JsonBody, PathParam, QueryParams};
use crate::idempotency::{request_hash, IdempotencyKey};
use crate::models::{Seat, SeatStatus, Show, SHOW_COLUMNS};
use crate::state::AppState;

const MAX_ROWS: u32 = 26;
const MAX_SEATS_PER_ROW: u32 = 500;
const DEFAULT_PER_USER_LIMIT: i32 = 4;
const DEFAULT_LIST_LIMIT: u32 = 20;
const MAX_LIST_LIMIT: u32 = 100;
/// Shows can't be edited after creation, so this only bounds memory use.
const SHOW_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
/// Writes delete the entry on commit; the TTL bounds the one race that
/// remains (a read that started before the commit re-filling it afterwards).
const SHOW_DETAIL_CACHE_TTL: Duration = Duration::from_secs(2);
const _: () = assert!(SHOW_DETAIL_CACHE_TTL.as_secs() < crate::cache::BREAKER_COOLDOWN.as_secs());

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/shows", post(create_show).get(list_shows))
        .route("/shows/{id}", get(get_show))
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

#[derive(sqlx::FromRow)]
struct KeyedShow {
    request_hash: String,
    #[sqlx(flatten)]
    show: Show,
}

/// A retry with the same Idempotency-Key gets the original show back (still
/// 201) instead of a duplicate with its own seat map.
async fn create_show(
    State(state): State<AppState>,
    user: AuthUser,
    idempotency_key: Result<IdempotencyKey, AppError>,
    JsonBody(req): JsonBody<CreateShowRequest>,
) -> Result<(StatusCode, Json<Show>), AppError> {
    if !user.is_admin {
        return Err(AppError::Forbidden("admin role required"));
    }
    let IdempotencyKey(idempotency_key) = idempotency_key?;
    let name = req.name.trim();
    let per_user_limit = req.per_user_limit.unwrap_or(DEFAULT_PER_USER_LIMIT);
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
    let hash = request_hash(&(
        name,
        req.price_paise,
        per_user_limit,
        req.rows,
        req.seats_per_row,
    ));
    let labels = seat_labels(req.rows, req.seats_per_row);

    let mut tx = state.pool.begin().await?;
    // A concurrent create with the same key blocks here until it commits,
    // then lands on the conflict path and replays.
    let inserted: Option<Show> = sqlx::query_as(&format!(
        "insert into shows
             (name, price_paise, per_user_limit, total_seats,
              created_by, idempotency_key, request_hash)
         values ($1, $2, $3, $4, $5, $6, $7)
         on conflict (created_by, idempotency_key) do nothing
         returning {SHOW_COLUMNS}"
    ))
    .bind(name)
    .bind(req.price_paise)
    .bind(per_user_limit)
    .bind(labels.len() as i32)
    .bind(&user.user_id)
    .bind(&idempotency_key)
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(show) = inserted else {
        let existing: KeyedShow = sqlx::query_as(&format!(
            "select request_hash, {SHOW_COLUMNS} from shows
             where created_by = $1 and idempotency_key = $2"
        ))
        .bind(&user.user_id)
        .bind(&idempotency_key)
        .fetch_one(&mut *tx)
        .await?;
        if existing.request_hash != hash {
            return Err(AppError::Conflict(Conflict::IdempotencyMismatch));
        }
        return Ok((StatusCode::CREATED, Json(existing.show)));
    };
    sqlx::query("insert into seats (show_id, label) select $1, unnest($2::text[])")
        .bind(show.id)
        .bind(&labels)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state.seat_map.add_show(show.id, &labels);

    Ok((StatusCode::CREATED, Json(show)))
}

/// A1..A{per_row}, B1.., one letter per row.
fn seat_labels(rows: u32, per_row: u32) -> Vec<String> {
    (0..rows)
        .flat_map(|r| {
            let row = (b'A' + r as u8) as char;
            (1..=per_row).map(move |n| format!("{row}{n}"))
        })
        .collect()
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
    seats: Vec<Seat>,
}

#[derive(Deserialize)]
struct ListShowsQuery {
    limit: Option<u32>,
    after: Option<Uuid>,
}

#[derive(Serialize)]
struct ShowPage {
    shows: Vec<Show>,
    next: Option<Uuid>,
}

async fn list_shows(
    State(state): State<AppState>,
    QueryParams(query): QueryParams<ListShowsQuery>,
) -> Result<Json<ShowPage>, AppError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if !(1..=MAX_LIST_LIMIT).contains(&limit) {
        return Err(AppError::Validation(format!(
            "limit must be 1..={MAX_LIST_LIMIT}"
        )));
    }

    // Keyset paging on (created_at, id): new shows don't shift later pages.
    let cursor: Option<(DateTime<Utc>, Uuid)> = match query.after {
        None => None,
        Some(after) => Some(
            sqlx::query_as("select created_at, id from shows where id = $1")
                .bind(after)
                .fetch_optional(&state.pool)
                .await?
                .ok_or_else(|| AppError::Validation(format!("unknown cursor: {after}")))?,
        ),
    };

    let mut shows: Vec<Show> = sqlx::query_as(&format!(
        "select {SHOW_COLUMNS} from shows
         where $1::timestamptz is null or (created_at, id) < ($1, $2)
         order by created_at desc, id desc
         limit $3"
    ))
    .bind(cursor.map(|(created_at, _)| created_at))
    .bind(cursor.map(|(_, id)| id))
    .bind(i64::from(limit) + 1)
    .fetch_all(&state.pool)
    .await?;

    let next = if shows.len() > limit as usize {
        shows.truncate(limit as usize);
        shows.last().map(|show| show.id)
    } else {
        None
    };

    Ok(Json(ShowPage { shows, next }))
}

async fn get_show(
    State(state): State<AppState>,
    PathParam(show_id): PathParam<Uuid>,
) -> Result<Json<ShowDetail>, AppError> {
    let key = show_detail_key(show_id);
    if let Some(detail) = state.cache.get_json(&key).await {
        return Ok(Json(detail));
    }
    let detail = build_show_detail(&state, show_id).await?;
    state
        .cache
        .set_json(&key, &detail, SHOW_DETAIL_CACHE_TTL)
        .await;
    Ok(Json(detail))
}

async fn build_show_detail(state: &AppState, show_id: Uuid) -> Result<ShowDetail, AppError> {
    let show = load_show(state, show_id).await?;

    // Counts come from the same single-statement read as the seat list, so
    // they always sum to total_seats.
    let mut seats: Vec<Seat> = sqlx::query_as("select label, status from seats where show_id = $1")
        .bind(show_id)
        .fetch_all(&state.pool)
        .await?;
    seats.sort_by(|a, b| seat_sort_key(&a.label).cmp(&seat_sort_key(&b.label)));

    let mut counts = SeatCounts::default();
    for seat in &seats {
        match seat.status {
            SeatStatus::Available => counts.available += 1,
            SeatStatus::Held => counts.held += 1,
            SeatStatus::Confirmed => counts.confirmed += 1,
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

/// Cache key for the GET /shows/{id} body. Anything that changes a seat
/// deletes it after committing.
pub(super) fn show_detail_key(show_id: Uuid) -> String {
    format!("show:{show_id}:detail")
}

/// The show row, from cache when possible. 404 if the show doesn't exist.
pub(super) async fn load_show(state: &AppState, show_id: Uuid) -> Result<Show, AppError> {
    let key = format!("show:{show_id}");
    if let Some(show) = state.cache.get_json(&key).await {
        return Ok(show);
    }
    let show = fetch_show(&state.pool, show_id).await?;
    state.cache.set_json(&key, &show, SHOW_CACHE_TTL).await;
    Ok(show)
}

async fn fetch_show<'e>(executor: impl PgExecutor<'e>, show_id: Uuid) -> Result<Show, AppError> {
    sqlx::query_as(&format!("select {SHOW_COLUMNS} from shows where id = $1"))
        .bind(show_id)
        .fetch_optional(executor)
        .await?
        .ok_or(AppError::NotFound("show not found"))
}
