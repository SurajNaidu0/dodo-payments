use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{page, page_limit, ListParams};
use crate::{
    auth::Auth,
    error::{ApiJson, ApiPath, ApiQuery, AppError},
    AppState,
};

#[derive(Debug, Deserialize)]
pub struct CreateCustomer {
    pub name: String,
    pub email: String,
}

#[derive(sqlx::FromRow)]
struct CustomerRow {
    id: Uuid,
    name: String,
    email: String,
    created_at: DateTime<Utc>,
}

fn customer_json(c: &CustomerRow) -> Value {
    json!({ "id": c.id, "object": "customer", "name": c.name, "email": c.email, "created_at": c.created_at })
}

pub async fn create(
    State(st): State<AppState>,
    auth: Auth,
    ApiJson(body): ApiJson<CreateCustomer>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let name = body.name.trim();
    let email = body.email.trim();
    if name.is_empty() || name.len() > 200 {
        return Err(AppError::bad_request("invalid_request", "name must be 1 to 200 characters."));
    }
    if !email.contains('@') || email.len() > 320 {
        return Err(AppError::bad_request("invalid_request", "email is not a valid address."));
    }

    // The unique index on (business_id, lower(email)) makes this race-safe.
    let row: Option<CustomerRow> = sqlx::query_as(
        "INSERT INTO customers (id, business_id, name, email) VALUES ($1, $2, $3, $4)
         ON CONFLICT (business_id, lower(email)) DO NOTHING
         RETURNING id, name, email, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(name)
    .bind(email)
    .fetch_optional(&st.pool)
    .await?;
    if let Some(row) = row {
        return Ok((StatusCode::CREATED, Json(customer_json(&row))));
    }

    let existing: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM customers WHERE business_id = $1 AND lower(email) = lower($2)",
    )
    .bind(auth.business_id)
    .bind(email)
    .fetch_optional(&st.pool)
    .await?;
    let mut err = AppError::new(
        StatusCode::CONFLICT,
        "customer_already_exists",
        "A customer with this email already exists.",
    );
    if let Some((id,)) = existing {
        err = err.with_details(json!({ "customer_id": id }));
    }
    Err(err)
}

pub async fn get(
    State(st): State<AppState>,
    auth: Auth,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, AppError> {
    let row: Option<CustomerRow> = sqlx::query_as(
        "SELECT id, name, email, created_at FROM customers WHERE id = $1 AND business_id = $2",
    )
    .bind(id)
    .bind(auth.business_id)
    .fetch_optional(&st.pool)
    .await?;
    row.map(|c| Json(customer_json(&c))).ok_or_else(|| AppError::not_found("Customer"))
}

pub async fn list(
    State(st): State<AppState>,
    auth: Auth,
    ApiQuery(params): ApiQuery<ListParams>,
) -> Result<Json<Value>, AppError> {
    let limit = page_limit(params.limit);
    let rows: Vec<CustomerRow> = sqlx::query_as(
        "SELECT id, name, email, created_at FROM customers
          WHERE business_id = $1 AND ($2::uuid IS NULL OR id < $2)
          ORDER BY id DESC LIMIT $3",
    )
    .bind(auth.business_id)
    .bind(params.starting_after)
    .bind(limit + 1)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(page(rows.iter().map(customer_json).collect(), limit)))
}
