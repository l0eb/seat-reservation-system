//! Idempotency-Key handling for the endpoints that create things. A key is
//! scoped to the user who sent it, and the stored request hash catches a key
//! reused for a different request.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::AppError;

const MAX_KEY_LEN: usize = 255;

pub struct IdempotencyKey(pub String);

impl<S: Send + Sync> FromRequestParts<S> for IdempotencyKey {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, AppError> {
        let key = parts
            .headers
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| AppError::Validation("Idempotency-Key header is required".into()))?;
        if key.len() > MAX_KEY_LEN {
            return Err(AppError::Validation(format!(
                "Idempotency-Key must be at most {MAX_KEY_LEN} characters"
            )));
        }
        Ok(Self(key.to_string()))
    }
}

/// Hash of a request's canonical form. Callers normalise first (sort, trim,
/// fill in defaults) so equivalent requests hash the same; JSON encoding
/// keeps fields from running into each other.
pub fn request_hash(canonical: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(canonical).expect("serializable");
    hex::encode(Sha256::digest(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_field_sensitive() {
        assert_eq!(request_hash(&("a", 1)), request_hash(&("a", 1)));
        assert_ne!(request_hash(&("a", 1)), request_hash(&("a", 2)));
        // ["ab","c"] and ["a","bc"] must not collide.
        assert_ne!(request_hash(&["ab", "c"]), request_hash(&["a", "bc"]));
    }
}
