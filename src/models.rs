//! Database rows and their JSON shapes.

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::state_machine::InvoiceState;

pub const INVOICE_COLS: &str = "id, business_id, customer_id, state, prev_state, locked_at, \
     total_cents, currency, due_date, created_at, updated_at";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InvoiceRow {
    pub id: Uuid,
    pub business_id: Uuid,
    pub customer_id: Uuid,
    pub state: String,
    pub prev_state: Option<String>,
    pub locked_at: Option<DateTime<Utc>>,
    pub total_cents: i64,
    pub currency: String,
    pub due_date: NaiveDate,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl InvoiceRow {
    pub fn state(&self) -> InvoiceState {
        // The CHECK constraint on invoices.state guarantees this parses.
        InvoiceState::parse(&self.state)
            .unwrap_or_else(|| panic!("invoice {} has unknown state {}", self.id, self.state))
    }
}

pub const LINE_ITEM_COLS: &str = "id, invoice_id, description, quantity, unit_amount_cents, amount_cents";

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct LineItemRow {
    pub id: Uuid,
    #[serde(skip)]
    pub invoice_id: Uuid,
    pub description: String,
    pub quantity: i32,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

pub fn invoice_json(inv: &InvoiceRow, items: &[LineItemRow]) -> Value {
    json!({
        "id": inv.id,
        "object": "invoice",
        "customer_id": inv.customer_id,
        "state": inv.state,
        "total_cents": inv.total_cents,
        "currency": inv.currency,
        "due_date": inv.due_date,
        "line_items": items,
        "created_at": inv.created_at,
        "updated_at": inv.updated_at,
    })
}

pub async fn fetch_invoice<'e, E: PgExecutor<'e>>(
    ex: E,
    business_id: Uuid,
    id: Uuid,
) -> Result<Option<InvoiceRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {INVOICE_COLS} FROM invoices WHERE id = $1 AND business_id = $2"
    ))
    .bind(id)
    .bind(business_id)
    .fetch_optional(ex)
    .await
}

pub async fn fetch_line_items<'e, E: PgExecutor<'e>>(
    ex: E,
    invoice_id: Uuid,
) -> Result<Vec<LineItemRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {LINE_ITEM_COLS} FROM invoice_line_items WHERE invoice_id = $1 ORDER BY position"
    ))
    .bind(invoice_id)
    .fetch_all(ex)
    .await
}

pub async fn load_invoice_json(
    pool: &PgPool,
    business_id: Uuid,
    id: Uuid,
) -> Result<Option<Value>, sqlx::Error> {
    let Some(inv) = fetch_invoice(pool, business_id, id).await? else {
        return Ok(None);
    };
    let items = fetch_line_items(pool, id).await?;
    Ok(Some(invoice_json(&inv, &items)))
}

pub const ATTEMPT_COLS: &str = "id, invoice_id, business_id, amount_cents, status, failure_code, \
     psp_ref, created_at, updated_at";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AttemptRow {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub business_id: Uuid,
    pub amount_cents: i64,
    pub status: String,
    pub failure_code: Option<String>,
    pub psp_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub fn attempt_json(a: &AttemptRow) -> Value {
    json!({
        "id": a.id,
        "object": "payment_attempt",
        "invoice_id": a.invoice_id,
        "amount_cents": a.amount_cents,
        "status": a.status,
        "failure_code": a.failure_code,
        "psp_ref": a.psp_ref,
        "created_at": a.created_at,
        "updated_at": a.updated_at,
    })
}

pub async fn fetch_attempt<'e, E: PgExecutor<'e>>(
    ex: E,
    business_id: Uuid,
    id: Uuid,
) -> Result<Option<AttemptRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {ATTEMPT_COLS} FROM payment_attempts WHERE id = $1 AND business_id = $2"
    ))
    .bind(id)
    .bind(business_id)
    .fetch_optional(ex)
    .await
}
