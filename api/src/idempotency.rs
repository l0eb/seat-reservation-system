//! Idempotency-Key handling for the endpoints that create things. A key is
//! scoped to the user who sent it, and the stored request hash catches a key
//! reused for a different request.
//!
//! Reserve requires a key, from the Idempotency-Key header or the body's
//! idempotency_key (the brief allows either). Creating a show takes an
//! optional header: without one it's a plain create.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::AppError;

const MAX_KEY_LEN: usize = 255;

/// The Idempotency-Key header, if one was sent. A blank header counts as
/// absent; one that isn't text or is too long is rejected.
pub struct IdempotencyKey(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for IdempotencyKey {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, AppError> {
        let Some(value) = parts.headers.get("idempotency-key") else {
            return Ok(Self(None));
        };
        let value = value
            .to_str()
            .map_err(|_| AppError::Validation("Idempotency-Key must be text".into()))?;
        Ok(Self(normalize("Idempotency-Key", value)?))
    }
}

/// The key for a request that must have one: the header, or the body's
/// idempotency_key. Sending both is fine as long as they agree.
pub fn required_key(header: Option<String>, body: Option<&str>) -> Result<String, AppError> {
    let body = body
        .map(|key| normalize("idempotency_key", key))
        .transpose()?
        .flatten();
    match (header, body) {
        (Some(header), Some(body)) if header != body => Err(AppError::Validation(
            "Idempotency-Key header and idempotency_key differ".into(),
        )),
        (Some(key), _) | (None, Some(key)) => Ok(key),
        (None, None) => Err(AppError::Validation(
            "an idempotency key is required: Idempotency-Key header or idempotency_key".into(),
        )),
    }
}

fn normalize(name: &str, key: &str) -> Result<Option<String>, AppError> {
    let key = key.trim();
    if key.len() > MAX_KEY_LEN {
        return Err(AppError::Validation(format!(
            "{name} must be at most {MAX_KEY_LEN} characters"
        )));
    }
    Ok((!key.is_empty()).then(|| key.to_string()))
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

    #[test]
    fn key_comes_from_header_or_body() {
        assert_eq!(required_key(Some("h".into()), None).unwrap(), "h");
        assert_eq!(required_key(None, Some(" b ")).unwrap(), "b");
        assert_eq!(required_key(Some("k".into()), Some("k")).unwrap(), "k");
    }

    #[test]
    fn key_is_required_and_must_agree() {
        assert!(required_key(None, None).is_err());
        assert!(required_key(None, Some("   ")).is_err());
        assert!(required_key(Some("a".into()), Some("b")).is_err());
        assert!(required_key(None, Some(&"x".repeat(MAX_KEY_LEN + 1))).is_err());
    }
}
