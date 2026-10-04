use std::sync::Arc;
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
use crate::single_flight::SingleFlight;
use crate::state::AppState;

const MAX_ROWS: u32 = 26;
const MAX_SEATS_PER_ROW: u32 = 500;
const MAX_SEATS: usize = (MAX_ROWS * MAX_SEATS_PER_ROW) as usize;
const MAX_LABEL_LEN: usize = 32;
const DEFAULT_PER_USER_LIMIT: i32 = 4;
const DEFAULT_LIST_LIMIT: u32 = 20;
const MAX_LIST_LIMIT: u32 = 100;
/// Shows can't be edited after creation, so this only bounds memory use.
const SHOW_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
/// The show page is for display, so it may lag bookings by up to this long:
/// during an on-sale, deleting it on every booking meant it was rebuilt
/// (all of a show's seats, from Postgres) for nearly every read. Bookings
/// leave it to expire; cancels still delete it, so a freed seat shows up at
/// once. Counts in any copy still add up: each is one database snapshot.
const SHOW_DETAIL_CACHE_TTL: Duration = Duration::from_secs(2);
/// How long a page read waits for a rebuild slot before 503 overloaded.
const SHOW_READ_WAIT: Duration = Duration::from_secs(5);
const _: () = assert!(SHOW_DETAIL_CACHE_TTL.as_secs() < crate::cache::BREAKER_COOLDOWN.as_secs());

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/shows", post(create_show).get(list_shows))
        .route("/shows/{id}", get(get_show))
}

/// Seats are given either as explicit labels (`seats`, as in the brief) or
/// as a grid (`rows` x `seats_per_row`, labelled A1..), not both.
#[derive(Deserialize)]
struct CreateShowRequest {
    name: String,
    price_paise: i64,
    #[serde(default)]
    per_user_limit: Option<i32>,
    #[serde(default)]
    seats: Option<Vec<String>>,
    #[serde(default)]
    rows: Option<u32>,
    #[serde(default)]
    seats_per_row: Option<u32>,
}

#[derive(sqlx::FromRow)]
struct KeyedShow {
    request_hash: String,
    #[sqlx(flatten)]
    show: Show,
}

/// Returns the show with every seat. With an Idempotency-Key, a retry gets
/// the same show back (still 201, with its current seats) instead of a
/// duplicate with its own seat map; without one, every call creates a show.
async fn create_show(
    State(state): State<AppState>,
    user: AuthUser,
    idempotency_key: Result<IdempotencyKey, AppError>,
    JsonBody(req): JsonBody<CreateShowRequest>,
) -> Result<(StatusCode, Json<ShowDetail>), AppError> {
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
    let labels = requested_seats(req.seats, req.rows, req.seats_per_row)?;
    // Hashing the labels, not the request's shape, so a grid and the same
    // seats listed out are the same request.
    let hash = request_hash(&(name, req.price_paise, per_user_limit, &labels));

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
        drop(tx);
        let detail = build_show_detail(&state, existing.show.id).await?;
        return Ok((StatusCode::CREATED, Json(detail)));
    };
    sqlx::query("insert into seats (show_id, label) select $1, unnest($2::text[])")
        .bind(show.id)
        .bind(&labels)
        .execute(&mut *tx)
        .await?;
    crate::seat_map::notify_show(&mut tx, show.id).await?;
    tx.commit().await?;
    state.seat_map.add_show(show.id, &labels);

    let mut seats: Vec<Seat> = labels
        .into_iter()
        .map(|label| Seat {
            label,
            status: SeatStatus::Available,
        })
        .collect();
    seats.sort_by(|a, b| seat_sort_key(&a.label).cmp(&seat_sort_key(&b.label)));
    let counts = SeatCounts {
        available: seats.len() as i64,
        ..SeatCounts::default()
    };
    Ok((
        StatusCode::CREATED,
        Json(ShowDetail {
            show,
            counts,
            seats,
        }),
    ))
}

/// The seat labels a create asks for, validated: either `seats` or both of
/// `rows` and `seats_per_row`.
fn requested_seats(
    seats: Option<Vec<String>>,
    rows: Option<u32>,
    seats_per_row: Option<u32>,
) -> Result<Vec<String>, AppError> {
    match (seats, rows, seats_per_row) {
        (Some(seats), None, None) => seat_list(seats),
        (None, Some(rows), Some(per_row)) => {
            if !(1..=MAX_ROWS).contains(&rows) {
                return Err(AppError::Validation(format!("rows must be 1..={MAX_ROWS}")));
            }
            if !(1..=MAX_SEATS_PER_ROW).contains(&per_row) {
                return Err(AppError::Validation(format!(
                    "seats_per_row must be 1..={MAX_SEATS_PER_ROW}"
                )));
            }
            Ok(seat_labels(rows, per_row))
        }
        _ => Err(AppError::Validation(
            "give either seats (a list of labels) or rows and seats_per_row".into(),
        )),
    }
}

/// Explicit labels: trimmed, non-empty, unique, at most MAX_SEATS of them.
fn seat_list(seats: Vec<String>) -> Result<Vec<String>, AppError> {
    if !(1..=MAX_SEATS).contains(&seats.len()) {
        return Err(AppError::Validation(format!(
            "seats must have 1..={MAX_SEATS} labels"
        )));
    }
    let mut seen = std::collections::HashSet::with_capacity(seats.len());
    let mut labels = Vec::with_capacity(seats.len());
    for seat in seats {
        let label = seat.trim();
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err(AppError::Validation(format!(
                "seat labels must be 1..={MAX_LABEL_LEN} characters"
            )));
        }
        if !seen.insert(label.to_string()) {
            return Err(AppError::Validation(format!(
                "duplicate seat label: {label}"
            )));
        }
        labels.push(label.to_string());
    }
    Ok(labels)
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
pub(crate) struct ShowDetail {
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

/// GET /shows/{id} cache misses in flight on this replica, by show.
pub(crate) type ShowReads = SingleFlight<Uuid, Result<Arc<ShowDetail>, ShowReadError>>;

/// What a shared page read can fail with; every waiter gets a copy.
#[derive(Clone)]
pub(crate) enum ShowReadError {
    NotFound,
    Overloaded,
    Internal(Arc<str>),
}

impl From<AppError> for ShowReadError {
    fn from(err: AppError) -> Self {
        match err {
            AppError::NotFound(_) => ShowReadError::NotFound,
            AppError::Overloaded => ShowReadError::Overloaded,
            other => ShowReadError::Internal(format!("{other:?}").into()),
        }
    }
}

impl From<ShowReadError> for AppError {
    fn from(err: ShowReadError) -> Self {
        match err {
            ShowReadError::NotFound => AppError::NotFound("show not found"),
            ShowReadError::Overloaded => AppError::Overloaded,
            ShowReadError::Internal(msg) => AppError::Internal(anyhow::anyhow!("{msg}")),
        }
    }
}

async fn get_show(
    State(state): State<AppState>,
    PathParam(show_id): PathParam<Uuid>,
) -> Result<Json<Arc<ShowDetail>>, AppError> {
    if let Some(detail) = state.cache.get_json(&show_detail_key(show_id)).await {
        return Ok(Json(Arc::new(detail)));
    }
    // A burst of misses for one show on this replica shares one read.
    let reader = state.clone();
    let detail = state
        .show_reads
        .run(show_id, move || refill_show_detail(reader, show_id))
        .await?;
    Ok(Json(detail))
}

/// Rebuild the page from Postgres and cache it, taking one of the few
/// page-read slots so a read storm can't starve bookings of connections.
async fn refill_show_detail(
    state: AppState,
    show_id: Uuid,
) -> Result<Arc<ShowDetail>, ShowReadError> {
    let _slot =
        match tokio::time::timeout(SHOW_READ_WAIT, state.show_read_semaphore.acquire()).await {
            Ok(Ok(slot)) => slot,
            Ok(Err(closed)) => return Err(ShowReadError::Internal(closed.to_string().into())),
            Err(_) => return Err(ShowReadError::Overloaded),
        };
    let key = show_detail_key(show_id);
    // Another replica may have refilled it while this one waited.
    if let Some(detail) = state.cache.get_json(&key).await {
        return Ok(Arc::new(detail));
    }
    let detail = build_show_detail(&state, show_id).await?;
    state
        .cache
        .set_json(&key, &detail, SHOW_DETAIL_CACHE_TTL)
        .await;
    Ok(Arc::new(detail))
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

/// Row letters then seat number numerically, so A2 sorts before A10. The
/// whole label breaks ties, so free-form labels still sort the same way
/// every time.
fn seat_sort_key(label: &str) -> (&str, u32, &str) {
    let split = label
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(label.len());
    let (row, num) = label.split_at(split);
    (row, num.parse().unwrap_or(0), label)
}

/// Cache key for the GET /shows/{id} body. Cancels delete it after
/// committing; bookings let it expire (see SHOW_DETAIL_CACHE_TTL).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(v: &[&str]) -> Option<Vec<String>> {
        Some(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn explicit_seats_are_trimmed_and_kept_in_order() {
        let seats = requested_seats(labels(&["A1", " A2 ", "VIP-1"]), None, None).unwrap();
        assert_eq!(seats, ["A1", "A2", "VIP-1"]);
    }

    #[test]
    fn grid_and_list_describe_the_same_seats() {
        let grid = requested_seats(None, Some(2), Some(2)).unwrap();
        assert_eq!(grid, ["A1", "A2", "B1", "B2"]);
    }

    #[test]
    fn exactly_one_seat_layout_is_required() {
        assert!(requested_seats(None, None, None).is_err());
        assert!(requested_seats(labels(&["A1"]), Some(1), Some(1)).is_err());
        assert!(requested_seats(None, Some(1), None).is_err());
    }

    #[test]
    fn bad_seat_lists_are_rejected() {
        assert!(requested_seats(labels(&[]), None, None).is_err());
        assert!(requested_seats(labels(&["A1", " "]), None, None).is_err());
        assert!(requested_seats(labels(&["A1", "A1 "]), None, None).is_err());
        assert!(requested_seats(labels(&[&"x".repeat(MAX_LABEL_LEN + 1)]), None, None).is_err());
        let too_many: Vec<String> = (0..=MAX_SEATS).map(|n| format!("S{n}")).collect();
        assert!(requested_seats(Some(too_many), None, None).is_err());
    }

    #[test]
    fn seats_sort_naturally() {
        let mut v = vec!["A10", "B1", "A2", "VIP-2", "A1"];
        v.sort_by(|a, b| seat_sort_key(a).cmp(&seat_sort_key(b)));
        assert_eq!(v, ["A1", "A2", "A10", "B1", "VIP-2"]);
    }
}
