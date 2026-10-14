//! API keys look like `sk_live_<32 random base62 chars>`.
//!
//! We store only SHA-256(key). The first 16 characters are a non-secret
//! prefix used to find the row, then the hash is compared in constant time.
//! SHA-256 (not bcrypt) is enough because the key is long and random:
//! there is no small password space to brute-force.

use axum::{
    extract::FromRequestParts,
    http::{header::AUTHORIZATION, request::Parts},
};
use rand::{distributions::Alphanumeric, Rng};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{error::AppError, AppState};

pub const KEY_PREFIX: &str = "sk_live_";
pub const LOOKUP_PREFIX_LEN: usize = 16;
const MIN_KEY_LEN: usize = 24;

pub fn generate_api_key() -> String {
    format!("{KEY_PREFIX}{}", random_string(32))
}

pub fn random_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

pub fn hash_key(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

/// The non-secret part of a key used for lookup, or `None` if malformed.
pub fn lookup_prefix(key: &str) -> Option<&str> {
    if key.starts_with(KEY_PREFIX) && key.len() >= MIN_KEY_LEN {
        key.get(..LOOKUP_PREFIX_LEN)
    } else {
        None
    }
}

/// The authenticated business. Add this as a handler argument to require a key.
#[derive(Debug, Clone, Copy)]
pub struct Auth {
    pub business_id: Uuid,
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let key = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .ok_or_else(AppError::unauthorized)?;
        let prefix = lookup_prefix(key).ok_or_else(AppError::unauthorized)?;

        let row: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
            "SELECT business_id, key_hash FROM api_keys WHERE prefix = $1 AND revoked_at IS NULL",
        )
        .bind(prefix)
        .fetch_optional(&state.pool)
        .await?;
        let (business_id, stored_hash) = row.ok_or_else(AppError::unauthorized)?;

        if bool::from(hash_key(key).ct_eq(&stored_hash)) {
            Ok(Auth { business_id })
        } else {
            Err(AppError::unauthorized())
        }
    }
}
