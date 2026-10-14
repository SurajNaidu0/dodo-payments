//! Sends webhook deliveries from the outbox, off the request path.
//!
//! Claiming uses `FOR UPDATE SKIP LOCKED` plus a 60 second lease (pushing
//! `next_attempt_at` forward), so several API instances can run this safely
//! and a crashed sender's deliveries are retried after the lease expires.
//!
//! Retry schedule after a failed attempt (±10% jitter):
//! 30s, 2m, 10m, 30m, 1h, 3h, 6h, 12h. That is 9 attempts over about 23 hours.
//! After the 9th failure the delivery is marked `dead`; the business can
//! still fetch the event from GET /events.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rand::Rng;
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::{error::AppError, webhook_signing, AppState};

pub const RETRY_DELAYS_SECS: [i64; 8] = [30, 120, 600, 1_800, 3_600, 10_800, 21_600, 43_200];
pub const MAX_ATTEMPTS: i32 = RETRY_DELAYS_SECS.len() as i32 + 1;
const BATCH_SIZE: i64 = 20;
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(sqlx::FromRow)]
struct Claimed {
    delivery_id: Uuid,
    attempt_count: i32,
    event_id: Uuid,
    event_type: String,
    payload: Value,
    event_created_at: DateTime<Utc>,
    url: String,
    secret: String,
}

/// One pass. Returns how many deliveries were attempted.
pub async fn dispatch_once(pool: &PgPool, http: &reqwest::Client) -> Result<usize, AppError> {
    // One statement, so claiming is atomic.
    let claimed: Vec<Claimed> = sqlx::query_as(
        "WITH due AS (
             SELECT id FROM webhook_deliveries
              WHERE status = 'pending' AND next_attempt_at <= now()
              ORDER BY next_attempt_at
              LIMIT $1
              FOR UPDATE SKIP LOCKED
         )
         UPDATE webhook_deliveries d
            SET next_attempt_at = now() + interval '60 seconds'
           FROM due, events e, webhook_endpoints w
          WHERE d.id = due.id AND e.id = d.event_id AND w.id = d.endpoint_id
         RETURNING d.id AS delivery_id, d.attempt_count, e.id AS event_id, e.type AS event_type,
                   e.payload, e.created_at AS event_created_at, w.url, w.secret",
    )
    .bind(BATCH_SIZE)
    .fetch_all(pool)
    .await?;

    let count = claimed.len();
    let mut sends = JoinSet::new();
    for delivery in claimed {
        let pool = pool.clone();
        let http = http.clone();
        sends.spawn(async move { deliver(&pool, &http, delivery).await });
    }
    while let Some(result) = sends.join_next().await {
        match result {
            Ok(Err(e)) => tracing::error!(error = %e, "failed to record webhook delivery result"),
            Err(e) => tracing::error!(error = %e, "webhook delivery task panicked"),
            Ok(Ok(())) => {}
        }
    }
    Ok(count)
}

async fn deliver(pool: &PgPool, http: &reqwest::Client, d: Claimed) -> Result<(), sqlx::Error> {
    let body = serde_json::to_vec(&json!({
        "id": d.event_id,
        "type": d.event_type,
        "created_at": d.event_created_at,
        "data": d.payload,
    }))
    .expect("event JSON always serializes");
    let timestamp = Utc::now().timestamp();

    let sent = http
        .post(&d.url)
        .timeout(SEND_TIMEOUT)
        .header("Content-Type", "application/json")
        .header(webhook_signing::HEADER, webhook_signing::header_value(&d.secret, timestamp, &body))
        .header("Dodo-Event-Id", d.event_id.to_string())
        .header("Dodo-Event-Type", &d.event_type)
        .body(body)
        .send()
        .await;

    let attempt_no = d.attempt_count + 1;
    let (ok, response_status, error) = match sent {
        Ok(r) if r.status().is_success() => (true, Some(r.status().as_u16() as i32), None),
        Ok(r) => (false, Some(r.status().as_u16() as i32), Some(format!("HTTP {}", r.status()))),
        Err(e) => (false, None, Some(e.to_string())),
    };

    if ok {
        sqlx::query(
            "UPDATE webhook_deliveries
                SET status = 'succeeded', attempt_count = $2, last_attempt_at = now(),
                    last_response_status = $3, last_error = NULL
              WHERE id = $1",
        )
        .bind(d.delivery_id)
        .bind(attempt_no)
        .bind(response_status)
        .execute(pool)
        .await?;
        tracing::info!(delivery_id = %d.delivery_id, event_type = %d.event_type, url = %d.url, "webhook delivered");
    } else if attempt_no >= MAX_ATTEMPTS {
        sqlx::query(
            "UPDATE webhook_deliveries
                SET status = 'dead', attempt_count = $2, last_attempt_at = now(),
                    last_response_status = $3, last_error = $4
              WHERE id = $1",
        )
        .bind(d.delivery_id)
        .bind(attempt_no)
        .bind(response_status)
        .bind(&error)
        .execute(pool)
        .await?;
        tracing::warn!(delivery_id = %d.delivery_id, url = %d.url, "webhook dead after max attempts");
    } else {
        let next = Utc::now() + retry_delay(attempt_no);
        sqlx::query(
            "UPDATE webhook_deliveries
                SET attempt_count = $2, next_attempt_at = $3, last_attempt_at = now(),
                    last_response_status = $4, last_error = $5
              WHERE id = $1",
        )
        .bind(d.delivery_id)
        .bind(attempt_no)
        .bind(next)
        .bind(response_status)
        .bind(&error)
        .execute(pool)
        .await?;
        tracing::warn!(delivery_id = %d.delivery_id, url = %d.url, attempt_no, error = ?error, next_attempt_at = %next, "webhook failed; will retry");
    }
    Ok(())
}

/// Delay before the next try, after `attempt_no` attempts have failed.
pub fn retry_delay(attempt_no: i32) -> chrono::Duration {
    let index = (attempt_no.max(1) as usize - 1).min(RETRY_DELAYS_SECS.len() - 1);
    let base = RETRY_DELAYS_SECS[index];
    let jitter = rand::thread_rng().gen_range(-(base / 10)..=(base / 10));
    chrono::Duration::seconds(base + jitter)
}

pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(state.config.dispatch_interval);
        loop {
            tick.tick().await;
            if let Err(e) = dispatch_once(&state.pool, &state.http).await {
                tracing::error!(error = %e.message, "webhook dispatch pass failed");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_is_documented_numbers_within_jitter() {
        for (i, base) in RETRY_DELAYS_SECS.iter().enumerate() {
            let d = retry_delay(i as i32 + 1).num_seconds();
            assert!(d >= base - base / 10 && d <= base + base / 10, "attempt {} gave {d}s", i + 1);
        }
        assert_eq!(MAX_ATTEMPTS, 9);
    }
}
