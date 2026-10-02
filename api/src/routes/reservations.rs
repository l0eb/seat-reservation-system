use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use uuid::Uuid;

use super::shows::{lock_seats, show_detail_key};
use crate::auth::AuthUser;
use crate::error::AppError;
use crate::models::{Reservation, RESERVATION_COLUMNS};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/reservations/{id}/cancel", post(cancel))
}

async fn cancel(
    State(state): State<AppState>,
    Path(reservation_id): Path<Uuid>,
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
            Some(_) => AppError::Conflict("already_cancelled"),
        });
    };

    // Same lock order as reserve: user counter, then seats by label.
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
    state.cache.delete(&show_detail_key(reservation.show_id)).await;
    Ok(Json(reservation))
}
