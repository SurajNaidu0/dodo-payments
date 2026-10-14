//! POST /invoices/{id}/pay
//!
//! 1. Claim the Idempotency-Key (or replay an earlier result).
//! 2. One transaction: lock the invoice with a conditional UPDATE and insert a
//!    pending payment attempt. Commit BEFORE calling the PSP, so a crash
//!    always leaves a record the reconciler can find.
//! 3. Call the PSP with the attempt id as its idempotency key (5 s timeout).
//! 4. Success or decline: settle. Timeout or network error: leave it locked,
//!    return 202, and let the reconciler find out what happened.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::Auth,
    error::{ApiJson, ApiPath, AppError},
    idempotency::{self, Begin, Stored},
    models::{attempt_json, fetch_attempt, fetch_invoice, load_invoice_json, AttemptRow, ATTEMPT_COLS},
    psp_client::ChargeOutcome,
    settlement::{self, FailureKind},
    state_machine::{allowed_from_db, rejection, Action},
    AppState,
};

#[derive(Debug, Deserialize, Serialize)]
pub struct PayRequest {
    pub card_token: String,
}

pub async fn pay(
    State(st): State<AppState>,
    auth: Auth,
    ApiPath(invoice_id): ApiPath<Uuid>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<PayRequest>,
) -> Result<Response, AppError> {
    let key = idempotency::key_from_headers(&headers)?;
    if req.card_token.trim().is_empty() || req.card_token.len() > 100 {
        return Err(AppError::bad_request("invalid_request", "card_token must be 1 to 100 characters."));
    }

    // Step 1: idempotency. The hash covers the path and the body, so the same
    // key on a different invoice or with a different card is a mismatch.
    let canonical_body = serde_json::to_vec(&req).expect("PayRequest always serializes");
    let hash = idempotency::request_hash(&[b"POST /invoices/", invoice_id.as_bytes(), b"/pay\n", &canonical_body]);
    match idempotency::begin(&st.pool, auth.business_id, &key, &hash).await? {
        Begin::Owner => {}
        Begin::Replay(stored) => return replay(&st.pool, auth.business_id, stored).await,
        Begin::Mismatch => {
            return Err(AppError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key_reused",
                "This Idempotency-Key was already used with a different request. Use a new key for a new request.",
            ))
        }
        Begin::InProgress => {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "request_in_progress",
                "A request with this Idempotency-Key is still being processed. Retry shortly.",
            ))
        }
    }

    // Step 2: lock + attempt, in one transaction.
    let attempt = match start_attempt(&st.pool, auth.business_id, invoice_id, &key, &req.card_token).await {
        Ok(attempt) => attempt,
        Err(err) => {
            if err.status.is_client_error() {
                // e.g. invoice already paid: replays must return the same 409.
                idempotency::store_response(&st.pool, auth.business_id, &key, err.status, &err.body()).await?;
            } else if let Err(e) = idempotency::release(&st.pool, auth.business_id, &key).await {
                tracing::error!(error = %e, "failed to release idempotency key");
            }
            return Err(err);
        }
    };

    // Step 3: call the PSP.
    let outcome = st.psp.charge(attempt.id, attempt.amount_cents, &req.card_token).await;

    // Step 4: settle. If the DB write fails here, the invoice stays locked and
    // the reconciler settles it later from the PSP's record.
    match &outcome {
        ChargeOutcome::Succeeded { psp_ref } => {
            settlement::settle_success(&st.pool, attempt.id, psp_ref).await?;
        }
        ChargeOutcome::Failed { code } => {
            settlement::settle_failure(&st.pool, attempt.id, code, FailureKind::Declined).await?;
        }
        ChargeOutcome::Unknown { reason } => {
            tracing::warn!(attempt_id = %attempt.id, %reason, "PSP outcome unknown; leaving invoice locked for the reconciler");
        }
    }

    render_attempt(&st.pool, auth.business_id, attempt.id).await
}

async fn start_attempt(
    pool: &PgPool,
    business_id: Uuid,
    invoice_id: Uuid,
    key: &str,
    card_token: &str,
) -> Result<AttemptRow, AppError> {
    let mut tx = pool.begin().await?;

    // The concurrency guarantee: only one request can move the invoice from
    // open/uncollectible to locked. Everyone else matches 0 rows.
    let locked: Option<(i64,)> = sqlx::query_as(
        "UPDATE invoices
            SET prev_state = state, state = 'locked', locked_at = now(), updated_at = now()
          WHERE id = $1 AND business_id = $2 AND state = ANY($3)
          RETURNING total_cents",
    )
    .bind(invoice_id)
    .bind(business_id)
    .bind(allowed_from_db(Action::Pay))
    .fetch_optional(&mut *tx)
    .await?;

    let Some((amount_cents,)) = locked else {
        drop(tx);
        let current = fetch_invoice(pool, business_id, invoice_id)
            .await?
            .ok_or_else(|| AppError::not_found("Invoice"))?;
        return Err(rejection(current.state(), Action::Pay));
    };

    let attempt: AttemptRow = sqlx::query_as(&format!(
        "INSERT INTO payment_attempts (id, invoice_id, business_id, amount_cents, card_token, status)
         VALUES ($1, $2, $3, $4, $5, 'pending')
         RETURNING {ATTEMPT_COLS}"
    ))
    .bind(Uuid::now_v7())
    .bind(invoice_id)
    .bind(business_id)
    .bind(amount_cents)
    .bind(card_token)
    .fetch_one(&mut *tx)
    .await?;

    idempotency::attach_attempt(&mut tx, business_id, key, attempt.id).await?;
    tx.commit().await?;
    tracing::info!(%invoice_id, attempt_id = %attempt.id, amount_cents, "invoice locked, calling PSP");
    Ok(attempt)
}

async fn replay(pool: &PgPool, business_id: Uuid, stored: Stored) -> Result<Response, AppError> {
    if let Some(attempt_id) = stored.attempt_id {
        return render_attempt(pool, business_id, attempt_id).await;
    }
    match (stored.response_status, stored.response_body) {
        (Some(status), Some(body)) => {
            let status = StatusCode::from_u16(status as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            Ok((status, Json(body)).into_response())
        }
        _ => Err(AppError::internal("idempotency key has neither an attempt nor a response")),
    }
}

/// The response for a payment attempt, built from its current status.
/// Final statuses never change, so replays of them are identical.
async fn render_attempt(pool: &PgPool, business_id: Uuid, attempt_id: Uuid) -> Result<Response, AppError> {
    let attempt = fetch_attempt(pool, business_id, attempt_id)
        .await?
        .ok_or_else(|| AppError::internal(format!("attempt {attempt_id} missing")))?;
    let attempt_body = attempt_json(&attempt);

    let response = match attempt.status.as_str() {
        "succeeded" | "pending" => {
            let invoice = load_invoice_json(pool, business_id, attempt.invoice_id)
                .await?
                .ok_or_else(|| AppError::internal("invoice missing for attempt"))?;
            let status = if attempt.status == "succeeded" { StatusCode::OK } else { StatusCode::ACCEPTED };
            (status, Json(json!({ "payment_attempt": attempt_body, "invoice": invoice }))).into_response()
        }
        "failed" => AppError::new(
            StatusCode::PAYMENT_REQUIRED,
            "payment_failed",
            format!(
                "The payment was declined ({}).",
                attempt.failure_code.as_deref().unwrap_or("unknown")
            ),
        )
        .with_details(json!({ "decline_code": attempt.failure_code, "payment_attempt": attempt_body }))
        .into_response(),
        "abandoned" => AppError::new(
            StatusCode::BAD_GATEWAY,
            "psp_unavailable",
            "The payment processor could not be reached and no charge was made. Retry with a new Idempotency-Key.",
        )
        .with_details(json!({ "payment_attempt": attempt_body }))
        .into_response(),
        other => return Err(AppError::internal(format!("unknown attempt status {other}"))),
    };
    Ok(response)
}

pub async fn get_attempt(
    State(st): State<AppState>,
    auth: Auth,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    fetch_attempt(&st.pool, auth.business_id, id)
        .await?
        .map(|a| Json(attempt_json(&a)))
        .ok_or_else(|| AppError::not_found("Payment attempt"))
}
