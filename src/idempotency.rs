//! Idempotency-Key handling for POST /invoices/{id}/pay.
//!
//! A key row is in one of three conditions:
//! - in progress: no attempt and no stored response yet,
//! - linked to a payment attempt: replays render the attempt's current result,
//! - holding a stored response: replays return it byte for byte
//!   (used for errors that happened before any attempt was created).

use axum::http::{HeaderMap, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::AppError;

/// An in-progress key with no attempt is taken over after this long. It
/// covers a crash between claiming the key and creating the attempt.
const TAKEOVER_AFTER_SECS: i64 = 60;

pub enum Begin {
    /// This request owns the key and should do the work.
    Owner,
    /// The key was used before with the same request. Return its result.
    Replay(Stored),
    /// The key was used before with a different request body or path.
    Mismatch,
    /// Another request with this key is still running.
    InProgress,
}

pub struct Stored {
    pub attempt_id: Option<Uuid>,
    pub response_status: Option<i16>,
    pub response_body: Option<Value>,
}

pub fn key_from_headers(headers: &HeaderMap) -> Result<String, AppError> {
    let key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    if key.is_empty() || key.len() > 255 {
        return Err(AppError::bad_request(
            "idempotency_key_required",
            "Send an 'Idempotency-Key' header of 1 to 255 characters.",
        ));
    }
    Ok(key.to_string())
}

pub fn request_hash(parts: &[&[u8]]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().to_vec()
}

pub async fn begin(pool: &PgPool, business_id: Uuid, key: &str, hash: &[u8]) -> Result<Begin, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO idempotency_keys (business_id, key, request_hash) VALUES ($1, $2, $3)
         ON CONFLICT (business_id, key) DO NOTHING",
    )
    .bind(business_id)
    .bind(key)
    .bind(hash)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    if inserted {
        return Ok(Begin::Owner);
    }

    let (stored_hash, attempt_id, response_status, response_body): (Vec<u8>, Option<Uuid>, Option<i16>, Option<Value>) =
        sqlx::query_as(
            "SELECT request_hash, attempt_id, response_status, response_body
               FROM idempotency_keys WHERE business_id = $1 AND key = $2",
        )
        .bind(business_id)
        .bind(key)
        .fetch_one(pool)
        .await?;

    if stored_hash != hash {
        return Ok(Begin::Mismatch);
    }
    if attempt_id.is_some() || response_status.is_some() {
        return Ok(Begin::Replay(Stored { attempt_id, response_status, response_body }));
    }

    // In progress. If it has been stuck long enough, the original request
    // died before creating an attempt, so this request may take it over.
    let took_over = sqlx::query(
        "UPDATE idempotency_keys SET claimed_at = now()
          WHERE business_id = $1 AND key = $2
            AND attempt_id IS NULL AND response_status IS NULL
            AND claimed_at < now() - make_interval(secs => $3)",
    )
    .bind(business_id)
    .bind(key)
    .bind(TAKEOVER_AFTER_SECS as f64)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    Ok(if took_over { Begin::Owner } else { Begin::InProgress })
}

pub async fn attach_attempt(
    conn: &mut PgConnection,
    business_id: Uuid,
    key: &str,
    attempt_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE idempotency_keys SET attempt_id = $3 WHERE business_id = $1 AND key = $2")
        .bind(business_id)
        .bind(key)
        .bind(attempt_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Saves a final response (a 4xx that happened before any attempt existed).
pub async fn store_response(
    pool: &PgPool,
    business_id: Uuid,
    key: &str,
    status: StatusCode,
    body: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE idempotency_keys SET response_status = $3, response_body = $4
          WHERE business_id = $1 AND key = $2",
    )
    .bind(business_id)
    .bind(key)
    .bind(status.as_u16() as i16)
    .bind(body)
    .execute(pool)
    .await?;
    Ok(())
}

/// Forgets a key after a 5xx so the client can retry with the same key.
pub async fn release(pool: &PgPool, business_id: Uuid, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM idempotency_keys
          WHERE business_id = $1 AND key = $2 AND attempt_id IS NULL AND response_status IS NULL",
    )
    .bind(business_id)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(())
}
