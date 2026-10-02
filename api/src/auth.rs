use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub role: Option<String>,
    pub iat: usize,
    pub exp: usize,
}

pub fn mint_token(
    secret: &str,
    user_id: &str,
    role: Option<&str>,
    ttl_secs: i64,
) -> Result<String, jsonwebtoken::errors::Error> {
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        sub: user_id.to_string(),
        role: role.map(str::to_string),
        iat: now as usize,
        exp: (now + ttl_secs) as usize,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
}

fn verify_token(secret: &str, token: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::new(Algorithm::HS256),
    )?;
    Ok(data.claims)
}

/// Identity derived strictly from the bearer token's `sub` — handlers never
/// take a user id from the request body, so a spoofed body field can't act
/// as another user.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: String,
    pub is_admin: bool,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(AppError::Unauthorized("missing bearer token"))?;
        let token = header
            .strip_prefix("Bearer ")
            .ok_or(AppError::Unauthorized("expected a Bearer token"))?;
        let claims = verify_token(&state.config.jwt_secret, token)
            .map_err(|_| AppError::Unauthorized("invalid or expired token"))?;
        Ok(AuthUser {
            user_id: claims.sub,
            is_admin: claims.role.as_deref() == Some("admin"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_and_verifies_a_user_token() {
        let token = mint_token("test-secret", "user-123", None, 3600).unwrap();
        let claims = verify_token("test-secret", &token).unwrap();
        assert_eq!(claims.sub, "user-123");
        assert_eq!(claims.role, None);
    }

    #[test]
    fn admin_role_claim_round_trips() {
        let token = mint_token("test-secret", "admin-1", Some("admin"), 3600).unwrap();
        let claims = verify_token("test-secret", &token).unwrap();
        assert_eq!(claims.role.as_deref(), Some("admin"));
    }

    #[test]
    fn rejects_token_signed_with_a_different_secret() {
        let token = mint_token("secret-a", "user-123", None, 3600).unwrap();
        assert!(verify_token("secret-b", &token).is_err());
    }

    #[test]
    fn rejects_expired_token() {
        // jsonwebtoken's default Validation allows 60s of clock-skew leeway
        // on exp, so go well past that rather than just into the past.
        let token = mint_token("test-secret", "user-123", None, -120).unwrap();
        assert!(verify_token("test-secret", &token).is_err());
    }
}
