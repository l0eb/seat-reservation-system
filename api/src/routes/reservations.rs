//! Booking and cancelling seats. Every flow here takes row locks in the same
//! order — idempotency key, then the user's counter, then seats by label —
//! so concurrent reserves and cancels can't deadlock each other.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;

use super::shows::{load_show, show_detail_key};
use crate::auth::AuthUser;
use crate::error::{AppError, Conflict};
use crate::extract::{JsonBody, PathParam};
use crate::idempotency::{request_hash, IdempotencyKey};
use crate::metrics::Decline;
use crate::models::{Reservation, Seat, SeatStatus, Show, RESERVATION_COLUMNS};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/shows/{id}/reserve", post(reserve))
        .route("/reservations/{id}/cancel", post(cancel))
}

// ---------------------------------------------------------------- reserve

#[derive(Deserialize)]
struct ReserveRequest {
    seats: Vec<String>,
}

/// A reserve request that passed its input checks.
struct ReserveInput {
    idempotency_key: String,
    /// Sorted and free of duplicates.
    seats: Vec<String>,
    request_hash: String,
}

enum Reserved {
    Created(Reservation),
    Replayed(Reservation),
}

#[derive(sqlx::FromRow)]
struct KeyedReservation {
    request_hash: String,
    #[sqlx(flatten)]
    reservation: Reservation,
}

async fn reserve(
    State(state): State<AppState>,
    PathParam(show_id): PathParam<Uuid>,
    user: AuthUser,
    IdempotencyKey(idempotency_key): IdempotencyKey,
    JsonBody(req): JsonBody<ReserveRequest>,
) -> Result<(StatusCode, Json<Reservation>), AppError> {
    let input = parse_reserve_input(show_id, idempotency_key, req)?;
    let outcome = reserve_seats(&state, show_id, &user, &input).await;
    record_outcome(&state, &outcome);
    let (Reserved::Created(reservation) | Reserved::Replayed(reservation)) = outcome?;
    Ok((StatusCode::CREATED, Json(reservation)))
}

async fn reserve_seats(
    state: &AppState,
    show_id: Uuid,
    user: &AuthUser,
    input: &ReserveInput,
) -> Result<Reserved, AppError> {
    let show = load_show(state, show_id).await?;

    // A retry of a request that already succeeded must replay, not hit the
    // pre-check below (its seats are now confirmed — by this user).
    if let Some(existing) = find_by_key(&state.pool, &user.user_id, &input.idempotency_key).await? {
        return replay(existing, &input.request_hash);
    }
    if input.seats.len() > show.per_user_limit as usize {
        return Err(AppError::Conflict(Conflict::PerUserLimit));
    }
    if let Err(err) = precheck_seats(state, show_id, &input.seats).await {
        // A concurrent request with this key can commit between the lookup
        // above and the pre-check, so the seats may be taken by this very
        // request. Look again before declining.
        if matches!(err, AppError::Conflict(Conflict::SeatTaken)) {
            if let Some(existing) =
                find_by_key(&state.pool, &user.user_id, &input.idempotency_key).await?
            {
                return replay(existing, &input.request_hash);
            }
        }
        return Err(err);
    }
    claim_seats(state, &show, user, input).await
}

fn parse_reserve_input(
    show_id: Uuid,
    idempotency_key: String,
    req: ReserveRequest,
) -> Result<ReserveInput, AppError> {
    let mut seats = req.seats;
    let requested = seats.len();
    seats.sort();
    seats.dedup();
    if seats.is_empty() {
        return Err(AppError::Validation("seats must not be empty".into()));
    }
    if seats.len() != requested {
        return Err(AppError::Validation(
            "seats must not contain duplicates".into(),
        ));
    }

    Ok(ReserveInput {
        idempotency_key,
        request_hash: request_hash(&(show_id, &seats)),
        seats,
    })
}

/// Fast path, outside any transaction: it can only decline, so a stale read
/// is safe, and doomed requests never take a pooled write slot.
async fn precheck_seats(state: &AppState, show_id: Uuid, seats: &[String]) -> Result<(), AppError> {
    let found: Vec<Seat> =
        sqlx::query_as("select label, status from seats where show_id = $1 and label = any($2)")
            .bind(show_id)
            .bind(seats)
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
    if found.iter().any(|s| s.status != SeatStatus::Available) {
        return Err(AppError::Conflict(Conflict::SeatTaken));
    }
    Ok(())
}

/// The decision itself: one transaction that either books every seat or
/// none. Losers of a race on a seat get `SeatTaken`, never an error.
async fn claim_seats(
    state: &AppState,
    show: &Show,
    user: &AuthUser,
    input: &ReserveInput,
) -> Result<Reserved, AppError> {
    let n = input.seats.len() as i32;
    let mut tx = state.pool.begin().await?;

    let inserted: Option<Reservation> = sqlx::query_as(&format!(
        "insert into reservations
             (show_id, user_id, seats, amount_paise, status, idempotency_key, request_hash)
         values ($1, $2, $3, $4, 'confirmed', $5, $6)
         on conflict (user_id, idempotency_key) do nothing
         returning {RESERVATION_COLUMNS}"
    ))
    .bind(show.id)
    .bind(&user.user_id)
    .bind(&input.seats)
    .bind(show.price_paise * n as i64)
    .bind(&input.idempotency_key)
    .bind(&input.request_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(reservation) = inserted else {
        // A concurrent request with this key committed first.
        let existing = find_by_key(&mut *tx, &user.user_id, &input.idempotency_key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("idempotency conflict but no reservation row"))?;
        return replay(existing, &input.request_hash);
    };

    let held: Option<i32> = sqlx::query_scalar(
        "insert into user_show_holds (show_id, user_id, held) values ($1, $2, $3)
         on conflict (show_id, user_id) do update
             set held = user_show_holds.held + excluded.held
             where user_show_holds.held + excluded.held <= $4
         returning held",
    )
    .bind(show.id)
    .bind(&user.user_id)
    .bind(n)
    .bind(show.per_user_limit)
    .fetch_optional(&mut *tx)
    .await?;
    if held.is_none() {
        return Err(AppError::Conflict(Conflict::PerUserLimit));
    }

    lock_seats(&mut tx, show.id, &input.seats).await?;
    let claimed = sqlx::query(
        "update seats set status = 'confirmed', reservation_id = $3
         where show_id = $1 and label = any($2) and status = 'available'",
    )
    .bind(show.id)
    .bind(&input.seats)
    .bind(reservation.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed != input.seats.len() as u64 {
        return Err(AppError::Conflict(Conflict::SeatTaken));
    }

    tx.commit().await?;
    state.cache.delete(&show_detail_key(show.id)).await;
    Ok(Reserved::Created(reservation))
}

fn record_outcome(state: &AppState, outcome: &Result<Reserved, AppError>) {
    let decline = match outcome {
        Ok(Reserved::Created(_)) => {
            state.metrics.record_confirmed();
            return;
        }
        Ok(Reserved::Replayed(_)) => Decline::IdempotentReplay,
        Err(AppError::Conflict(Conflict::SeatTaken)) => Decline::SeatTaken,
        Err(AppError::Conflict(Conflict::PerUserLimit)) => Decline::PerUserLimit,
        Err(AppError::Conflict(Conflict::IdempotencyMismatch)) => Decline::IdempotencyMismatch,
        Err(_) => return,
    };
    state.metrics.record_declined(decline);
}

fn replay(existing: KeyedReservation, request_hash: &str) -> Result<Reserved, AppError> {
    if existing.request_hash == request_hash {
        Ok(Reserved::Replayed(existing.reservation))
    } else {
        Err(AppError::Conflict(Conflict::IdempotencyMismatch))
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

// ----------------------------------------------------------------- cancel

async fn cancel(
    State(state): State<AppState>,
    PathParam(reservation_id): PathParam<Uuid>,
    user: AuthUser,
) -> Result<Json<Reservation>, AppError> {
    let mut tx = state.pool.begin().await?;

    let cancelled: Option<Reservation> = sqlx::query_as(&format!(
        "update reservations set status = 'cancelled'
         where id = $1 and user_id = $2 and status = 'confirmed'
         returning {RESERVATION_COLUMNS}"
    ))
    .bind(reservation_id)
    .bind(&user.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(reservation) = cancelled else {
        let owner: Option<String> =
            sqlx::query_scalar("select user_id from reservations where id = $1")
                .bind(reservation_id)
                .fetch_optional(&mut *tx)
                .await?;
        return Err(match owner {
            None => AppError::NotFound("reservation not found"),
            Some(owner) if owner != user.user_id => {
                AppError::Forbidden("reservation belongs to another user")
            }
            Some(_) => AppError::Conflict(Conflict::AlreadyCancelled),
        });
    };

    sqlx::query(
        "update user_show_holds set held = held - $3
         where show_id = $1 and user_id = $2",
    )
    .bind(reservation.show_id)
    .bind(&user.user_id)
    .bind(reservation.seats.len() as i32)
    .execute(&mut *tx)
    .await?;

    lock_seats(&mut tx, reservation.show_id, &reservation.seats).await?;
    // Matching on reservation_id means a cancel can never free a seat that
    // now belongs to someone else.
    sqlx::query(
        "update seats set status = 'available', reservation_id = null
         where show_id = $1 and label = any($2) and reservation_id = $3",
    )
    .bind(reservation.show_id)
    .bind(&reservation.seats)
    .bind(reservation.id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    state
        .cache
        .delete(&show_detail_key(reservation.show_id))
        .await;
    Ok(Json(reservation))
}

/// Locks the seat rows in label order before anything updates them; an
/// UPDATE alone locks in arbitrary order and can deadlock multi-seat requests.
async fn lock_seats(
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
