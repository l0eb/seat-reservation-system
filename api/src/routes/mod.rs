mod auth;
mod health;
mod reservations;
pub(crate) mod shows;

use axum::{middleware, Router};

use crate::metrics::track_status;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(auth::router())
        .merge(health::router())
        .merge(shows::router())
        .merge(reservations::router())
        .layer(middleware::from_fn_with_state(state.clone(), track_status))
        .with_state(state)
}
