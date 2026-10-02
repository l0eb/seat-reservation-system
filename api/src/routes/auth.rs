use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::mint_token;
use crate::error::AppError;
use crate::state::AppState;

const TEST_TOKEN_TTL_SECS: i64 = 24 * 60 * 60;

#[derive(Deserialize)]
struct MintTokenRequest {
    user_id: String,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Serialize)]
struct MintTokenResponse {
    token: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/auth/token", post(mint))
}

/// Env-gated: mints test JWTs for graders and the burst tool. Disabled by
/// default — must not be reachable against a show that's actually on sale.
async fn mint(
    State(state): State<AppState>,
    Json(req): Json<MintTokenRequest>,
) -> Result<Json<MintTokenResponse>, AppError> {
    if !state.config.auth_token_route_enabled {
        return Err(AppError::NotFound("not found"));
    }
    let token = mint_token(
        &state.config.jwt_secret,
        &req.user_id,
        req.role.as_deref(),
        TEST_TOKEN_TTL_SECS,
    )
    .map_err(|e| AppError::Internal(e.into()))?;
    Ok(Json(MintTokenResponse { token }))
}
