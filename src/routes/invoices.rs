use std::collections::HashMap;

use axum::{extract::State, http::StatusCode, Json};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{page, page_limit};
use crate::{
    auth::Auth,
    error::{ApiJson, ApiPath, ApiQuery, AppError},
    events,
    models::{
        fetch_invoice, fetch_line_items, invoice_json, load_invoice_json, InvoiceRow, LineItemRow,
        INVOICE_COLS, LINE_ITEM_COLS,
    },
    money::{price_line_items, LineItemInput},
    state_machine::{allowed_from_db, rejection, Action, InvoiceState},
    AppState,
};

#[derive(Debug, Deserialize)]
pub struct CreateInvoice {
    pub customer_id: Uuid,
    pub due_date: NaiveDate,
    pub line_items: Vec<LineItemInput>,
    /// Optional. Only "USD" is accepted.
    pub currency: Option<String>,
    // Any client-sent "total" is ignored: serde drops unknown fields.
}

pub async fn create(
    State(st): State<AppState>,
    auth: Auth,
    ApiJson(body): ApiJson<CreateInvoice>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    if let Some(currency) = &body.currency {
        if !currency.eq_ignore_ascii_case("USD") {
            return Err(AppError::bad_request("unsupported_currency", "Only USD is supported."));
        }
    }
    let (items, total_cents) = price_line_items(&body.line_items)?;

    let customer_exists: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM customers WHERE id = $1 AND business_id = $2")
            .bind(body.customer_id)
            .bind(auth.business_id)
            .fetch_optional(&st.pool)
            .await?;
    if customer_exists.is_none() {
        return Err(AppError::not_found("Customer"));
    }

    let mut tx = st.pool.begin().await?;
    let invoice: InvoiceRow = sqlx::query_as(&format!(
        "INSERT INTO invoices (id, business_id, customer_id, state, total_cents, currency, due_date)
         VALUES ($1, $2, $3, 'open', $4, 'USD', $5)
         RETURNING {INVOICE_COLS}"
    ))
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(body.customer_id)
    .bind(total_cents)
    .bind(body.due_date)
    .fetch_one(&mut *tx)
    .await?;

    let mut rows = Vec::with_capacity(items.len());
    for (position, item) in items.iter().enumerate() {
        let row: LineItemRow = sqlx::query_as(&format!(
            "INSERT INTO invoice_line_items
                 (id, invoice_id, position, description, quantity, unit_amount_cents, amount_cents)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING {LINE_ITEM_COLS}"
        ))
        .bind(Uuid::now_v7())
        .bind(invoice.id)
        .bind(position as i32)
        .bind(&item.description)
        .bind(item.quantity)
        .bind(item.unit_amount_cents)
        .bind(item.amount_cents)
        .fetch_one(&mut *tx)
        .await?;
        rows.push(row);
    }

    let body = invoice_json(&invoice, &rows);
    events::emit(&mut tx, auth.business_id, invoice.id, events::INVOICE_CREATED, json!({ "invoice": body }))
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(body)))
}

pub async fn get(
    State(st): State<AppState>,
    auth: Auth,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, AppError> {
    load_invoice_json(&st.pool, auth.business_id, id)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::not_found("Invoice"))
}

#[derive(Debug, Deserialize)]
pub struct InvoiceListParams {
    pub state: Option<String>,
    pub limit: Option<i64>,
    pub starting_after: Option<Uuid>,
}

pub async fn list(
    State(st): State<AppState>,
    auth: Auth,
    ApiQuery(params): ApiQuery<InvoiceListParams>,
) -> Result<Json<Value>, AppError> {
    if let Some(state) = &params.state {
        if InvoiceState::parse(state).is_none() {
            return Err(AppError::bad_request(
                "invalid_query",
                "state must be one of: open, locked, uncollectible, paid, void.",
            ));
        }
    }
    let limit = page_limit(params.limit);
    let invoices: Vec<InvoiceRow> = sqlx::query_as(&format!(
        "SELECT {INVOICE_COLS} FROM invoices
          WHERE business_id = $1
            AND ($2::text IS NULL OR state = $2)
            AND ($3::uuid IS NULL OR id < $3)
          ORDER BY id DESC LIMIT $4"
    ))
    .bind(auth.business_id)
    .bind(&params.state)
    .bind(params.starting_after)
    .bind(limit + 1)
    .fetch_all(&st.pool)
    .await?;

    // Load all line items for this page in one query.
    let ids: Vec<Uuid> = invoices.iter().map(|i| i.id).collect();
    let items: Vec<LineItemRow> = sqlx::query_as(&format!(
        "SELECT {LINE_ITEM_COLS} FROM invoice_line_items WHERE invoice_id = ANY($1) ORDER BY position"
    ))
    .bind(&ids)
    .fetch_all(&st.pool)
    .await?;
    let mut by_invoice: HashMap<Uuid, Vec<LineItemRow>> = HashMap::new();
    for item in items {
        by_invoice.entry(item.invoice_id).or_default().push(item);
    }

    let data = invoices
        .iter()
        .map(|inv| invoice_json(inv, by_invoice.get(&inv.id).map(Vec::as_slice).unwrap_or(&[])))
        .collect();
    Ok(Json(page(data, limit)))
}

pub async fn void(
    State(st): State<AppState>,
    auth: Auth,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, AppError> {
    transition(&st, auth, id, Action::Void, InvoiceState::Void, events::INVOICE_VOIDED).await
}

pub async fn mark_uncollectible(
    State(st): State<AppState>,
    auth: Auth,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, AppError> {
    transition(
        &st,
        auth,
        id,
        Action::MarkUncollectible,
        InvoiceState::Uncollectible,
        events::INVOICE_MARKED_UNCOLLECTIBLE,
    )
    .await
}

/// A business-triggered transition. One conditional UPDATE does the check and
/// the write together, so a concurrent payment can never slip in between.
async fn transition(
    st: &AppState,
    auth: Auth,
    id: Uuid,
    action: Action,
    target: InvoiceState,
    event_type: &str,
) -> Result<Json<Value>, AppError> {
    let mut tx = st.pool.begin().await?;
    let updated: Option<InvoiceRow> = sqlx::query_as(&format!(
        "UPDATE invoices SET state = $3, updated_at = now()
          WHERE id = $1 AND business_id = $2 AND state = ANY($4)
          RETURNING {INVOICE_COLS}"
    ))
    .bind(id)
    .bind(auth.business_id)
    .bind(target.as_str())
    .bind(allowed_from_db(action))
    .fetch_optional(&mut *tx)
    .await?;

    let Some(invoice) = updated else {
        drop(tx);
        // Nothing matched: either it doesn't exist or its state forbids this.
        let current = fetch_invoice(&st.pool, auth.business_id, id)
            .await?
            .ok_or_else(|| AppError::not_found("Invoice"))?;
        return Err(rejection(current.state(), action));
    };

    let items = fetch_line_items(&mut *tx, invoice.id).await?;
    let body = invoice_json(&invoice, &items);
    events::emit(&mut tx, auth.business_id, invoice.id, event_type, json!({ "invoice": body })).await?;
    tx.commit().await?;
    Ok(Json(body))
}
