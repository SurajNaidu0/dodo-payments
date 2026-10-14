//! The reconciler: a background loop that finds invoices stuck in `locked`
//! and asks the PSP what really happened to their payment attempt.
//!
//! An invoice gets stuck when the PSP timed out, the network failed, or our
//! process crashed between locking the invoice and saving the PSP's answer.
//! The PSP is the source of truth, so we copy its answer into our database.

use std::time::Duration;

use uuid::Uuid;
use sqlx::PgPool;

use crate::{
    error::AppError,
    psp_client::{ChargeStatus, PspClient},
    settlement::{self, FailureKind},
    AppState,
};

const BATCH_SIZE: i64 = 50;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub checked: usize,
    pub paid: usize,
    pub failed: usize,
    pub abandoned: usize,
    pub still_processing: usize,
    pub errors: usize,
}

/// One pass. `older_than` must be well above the PSP client timeout so we
/// never race a request that is still talking to the PSP. Tests pass zero.
pub async fn reconcile_once(pool: &PgPool, psp: &PspClient, older_than: Duration) -> Result<ReconcileReport, AppError> {
    let stuck: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT a.id
           FROM payment_attempts a
           JOIN invoices i ON i.id = a.invoice_id
          WHERE a.status = 'pending'
            AND i.state = 'locked'
            AND i.locked_at <= now() - ($1::bigint * interval '1 millisecond')
          ORDER BY i.locked_at
          LIMIT $2",
    )
    .bind(older_than.as_millis() as i64)
    .bind(BATCH_SIZE)
    .fetch_all(pool)
    .await?;

    let mut report = ReconcileReport { checked: stuck.len(), ..Default::default() };
    for (attempt_id,) in stuck {
        let result = match psp.lookup(attempt_id).await {
            Ok(ChargeStatus::Succeeded { psp_ref }) => {
                report.paid += 1;
                settlement::settle_success(pool, attempt_id, &psp_ref).await
            }
            Ok(ChargeStatus::Failed { code }) => {
                report.failed += 1;
                settlement::settle_failure(pool, attempt_id, &code, FailureKind::Declined).await
            }
            Ok(ChargeStatus::NotFound) => {
                // The charge never reached the PSP, so no money moved.
                // Release the invoice back to where it was.
                report.abandoned += 1;
                settlement::settle_failure(pool, attempt_id, "psp_no_record", FailureKind::Abandoned).await
            }
            Ok(ChargeStatus::Processing) => {
                report.still_processing += 1;
                Ok(false)
            }
            Err(e) => {
                tracing::warn!(%attempt_id, error = %e, "PSP lookup failed; will retry next run");
                report.errors += 1;
                Ok(false)
            }
        };
        if let Err(e) = result {
            report.errors += 1;
            tracing::error!(%attempt_id, error = %e.message, "failed to settle attempt");
        }
    }
    if report.checked > 0 {
        tracing::info!(?report, "reconciler pass finished");
    }
    Ok(report)
}

pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(state.config.reconcile_interval);
        loop {
            tick.tick().await;
            if let Err(e) = reconcile_once(&state.pool, &state.psp, state.config.reconcile_after).await {
                tracing::error!(error = %e.message, "reconciler pass failed");
            }
        }
    })
}
