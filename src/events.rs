//! The transactional outbox. Call `emit` inside the same transaction as the
//! state change. If the transaction rolls back, the event and its webhook
//! deliveries disappear with it. If it commits, the dispatcher will send them.

use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

pub const INVOICE_CREATED: &str = "invoice.created";
pub const INVOICE_PAID: &str = "invoice.paid";
pub const INVOICE_PAYMENT_FAILED: &str = "invoice.payment_failed";
pub const INVOICE_VOIDED: &str = "invoice.voided";
pub const INVOICE_MARKED_UNCOLLECTIBLE: &str = "invoice.marked_uncollectible";

pub async fn emit(
    conn: &mut PgConnection,
    business_id: Uuid,
    invoice_id: Uuid,
    event_type: &str,
    data: Value,
) -> Result<Uuid, sqlx::Error> {
    let event_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, business_id, type, invoice_id, payload) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(event_id)
    .bind(business_id)
    .bind(event_type)
    .bind(invoice_id)
    .bind(data)
    .execute(&mut *conn)
    .await?;

    // One delivery per enabled endpoint of this business, due immediately.
    sqlx::query(
        "INSERT INTO webhook_deliveries (id, event_id, endpoint_id, status, next_attempt_at)
         SELECT gen_random_uuid(), $1, id, 'pending', now()
           FROM webhook_endpoints
          WHERE business_id = $2 AND enabled",
    )
    .bind(event_id)
    .bind(business_id)
    .execute(&mut *conn)
    .await?;

    Ok(event_id)
}
