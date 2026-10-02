use axum::Router;

use crate::state::AppState;

/// Assembled incrementally as each route group lands; empty for now.
pub fn router(state: AppState) -> Router {
    Router::new().with_state(state)
}
