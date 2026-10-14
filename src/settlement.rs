//! Moving an invoice out of `locked`. Used by both the pay handler and the
//! reconciler. Every update is conditional (`WHERE status = 'pending'` and
//! `WHERE state = 'locked'`), so whichever of them gets there first wins and
//! the other one changes nothing.

use serde_json::json;
use uuid::Uuid;
use sqlx::PgPool;

use crate::{
    error::AppError,
    events,
    models::{attempt_json, fetch_line_items, invoice_json, AttemptRow, InvoiceRow, ATTEMPT_COLS, INVOICE_COLS},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The PSP declined the card. The customer is told via invoice.payment_failed.
    Declined,
    /// The PSP never received the charge (e.g. crash before calling it).
    /// No money moved, so no webhook is sent.
    Abandoned,
}

/// Returns `false` if the attempt was already settled by someone else.
pub async fn settle_success(pool: &PgPool, attempt_id: Uuid, psp_ref: &str) -> Result<bool, AppError> {
    let mut tx = pool.begin().await?;

    let attempt: Option<AttemptRow> = sqlx::query_as(&format!(
        "UPDATE payment_attempts SET status = 'succeeded', psp_ref = $2, updated_at = now()
          WHERE id = $1 AND status = 'pending'
          RETURNING {ATTEMPT_COLS}"
    ))
    .bind(attempt_id)
    .bind(psp_ref)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(attempt) = attempt else {
        return Ok(false);
    };

    let invoice: Option<InvoiceRow> = sqlx::query_as(&format!(
        "UPDATE invoices SET state = 'paid', prev_state = NULL, locked_at = NULL, updated_at = now()
          WHERE id = $1 AND state = 'locked'
          RETURNING {INVOICE_COLS}"
    ))
    .bind(attempt.invoice_id)
    .fetch_optional(&mut *tx)
    .await?;
    let invoice = invoice.ok_or_else(|| {
        AppError::internal(format!("invoice {} not locked while attempt {} pending", attempt.invoice_id, attempt.id))
    })?;

    let items = fetch_line_items(&mut *tx, invoice.id).await?;
    events::emit(
        &mut tx,
        invoice.business_id,
        invoice.id,
        events::INVOICE_PAID,
        json!({ "invoice": invoice_json(&invoice, &items), "payment_attempt": attempt_json(&attempt) }),
    )
    .await?;

    tx.commit().await?;
    tracing::info!(invoice_id = %invoice.id, attempt_id = %attempt.id, "invoice paid");
    Ok(true)
}

/// Returns `false` if the attempt was already settled by someone else.
pub async fn settle_failure(
    pool: &PgPool,
    attempt_id: Uuid,
    code: &str,
    kind: FailureKind,
) -> Result<bool, AppError> {
    let status = match kind {
        FailureKind::Declined => "failed",
        FailureKind::Abandoned => "abandoned",
    };
    let mut tx = pool.begin().await?;

    let attempt: Option<AttemptRow> = sqlx::query_as(&format!(
        "UPDATE payment_attempts SET status = $2, failure_code = $3, updated_at = now()
          WHERE id = $1 AND status = 'pending'
          RETURNING {ATTEMPT_COLS}"
    ))
    .bind(attempt_id)
    .bind(status)
    .bind(code)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(attempt) = attempt else {
        return Ok(false);
    };

    // Go back to wherever the invoice was before the payment started.
    // In SQL, `state = prev_state` reads the old prev_state before it is cleared.
    let invoice: Option<InvoiceRow> = sqlx::query_as(&format!(
        "UPDATE invoices SET state = prev_state, prev_state = NULL, locked_at = NULL, updated_at = now()
          WHERE id = $1 AND state = 'locked'
          RETURNING {INVOICE_COLS}"
    ))
    .bind(attempt.invoice_id)
    .fetch_optional(&mut *tx)
    .await?;
    let invoice = invoice.ok_or_else(|| {
        AppError::internal(format!("invoice {} not locked while attempt {} pending", attempt.invoice_id, attempt.id))
    })?;

    if kind == FailureKind::Declined {
        let items = fetch_line_items(&mut *tx, invoice.id).await?;
        events::emit(
            &mut tx,
            invoice.business_id,
            invoice.id,
            events::INVOICE_PAYMENT_FAILED,
            json!({ "invoice": invoice_json(&invoice, &items), "payment_attempt": attempt_json(&attempt) }),
        )
        .await?;
    }

    tx.commit().await?;
    tracing::info!(invoice_id = %invoice.id, attempt_id = %attempt.id, status, code, state = %invoice.state, "payment attempt failed");
    Ok(true)
}
