mod auth;
mod reservations;
mod shows;

use axum::Router;

use crate::state::AppState;

/// Assembled incrementally as each route group lands.
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(auth::router())
        .merge(shows::router())
        .merge(reservations::router())
        .with_state(state)
}
