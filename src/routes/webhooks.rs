use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{page, page_limit};
use crate::{
    auth::{random_string, Auth},
    error::{ApiJson, ApiQuery, AppError},
    AppState,
};

#[derive(Debug, Deserialize)]
pub struct CreateEndpoint {
    pub url: String,
}

#[derive(sqlx::FromRow)]
struct EndpointRow {
    id: Uuid,
    url: String,
    enabled: bool,
    created_at: DateTime<Utc>,
}

/// The secret is returned only here, once. Businesses use it to verify signatures.
pub async fn create_endpoint(
    State(st): State<AppState>,
    auth: Auth,
    ApiJson(body): ApiJson<CreateEndpoint>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let url = body.url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) || url.len() > 2000 {
        return Err(AppError::bad_request("invalid_request", "url must be an http:// or https:// URL."));
    }
    let secret = format!("whsec_{}", random_string(32));
    let row: EndpointRow = sqlx::query_as(
        "INSERT INTO webhook_endpoints (id, business_id, url, secret) VALUES ($1, $2, $3, $4)
         RETURNING id, url, enabled, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(url)
    .bind(&secret)
    .fetch_one(&st.pool)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": row.id, "object": "webhook_endpoint", "url": row.url, "enabled": row.enabled,
            "secret": secret, "created_at": row.created_at,
        })),
    ))
}

pub async fn list_endpoints(State(st): State<AppState>, auth: Auth) -> Result<Json<Value>, AppError> {
    let rows: Vec<EndpointRow> = sqlx::query_as(
        "SELECT id, url, enabled, created_at FROM webhook_endpoints WHERE business_id = $1 ORDER BY id",
    )
    .bind(auth.business_id)
    .fetch_all(&st.pool)
    .await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|r| json!({ "id": r.id, "object": "webhook_endpoint", "url": r.url, "enabled": r.enabled, "created_at": r.created_at }))
        .collect();
    Ok(Json(json!({ "object": "list", "data": data, "has_more": false })))
}

#[derive(Debug, Deserialize)]
pub struct DeliveryParams {
    pub status: Option<String>,
    pub limit: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct DeliveryRow {
    id: Uuid,
    event_id: Uuid,
    event_type: String,
    endpoint_id: Uuid,
    url: String,
    status: String,
    attempt_count: i32,
    next_attempt_at: DateTime<Utc>,
    last_attempt_at: Option<DateTime<Utc>>,
    last_response_status: Option<i32>,
    last_error: Option<String>,
    created_at: DateTime<Utc>,
}

/// Newest first. Lets a business (and the demo) see what was sent and how it went.
pub async fn list_deliveries(
    State(st): State<AppState>,
    auth: Auth,
    ApiQuery(params): ApiQuery<DeliveryParams>,
) -> Result<Json<Value>, AppError> {
    let limit = page_limit(params.limit);
    let rows: Vec<DeliveryRow> = sqlx::query_as(
        "SELECT d.id, d.event_id, e.type AS event_type, d.endpoint_id, w.url, d.status,
                d.attempt_count, d.next_attempt_at, d.last_attempt_at, d.last_response_status,
                d.last_error, d.created_at
           FROM webhook_deliveries d
           JOIN events e ON e.id = d.event_id
           JOIN webhook_endpoints w ON w.id = d.endpoint_id
          WHERE e.business_id = $1 AND ($2::text IS NULL OR d.status = $2)
          ORDER BY d.created_at DESC, d.id
          LIMIT $3",
    )
    .bind(auth.business_id)
    .bind(&params.status)
    .bind(limit + 1)
    .fetch_all(&st.pool)
    .await?;
    let data = rows
        .iter()
        .map(|d| {
            json!({
                "id": d.id, "object": "webhook_delivery", "event_id": d.event_id, "event_type": d.event_type,
                "endpoint_id": d.endpoint_id, "url": d.url, "status": d.status,
                "attempt_count": d.attempt_count, "next_attempt_at": d.next_attempt_at,
                "last_attempt_at": d.last_attempt_at, "last_response_status": d.last_response_status,
                "last_error": d.last_error, "created_at": d.created_at,
            })
        })
        .collect();
    Ok(Json(page(data, limit)))
}

#[derive(Debug, Deserialize)]
pub struct EventParams {
    /// Return events created after this event id (oldest first).
    pub after: Option<Uuid>,
    pub limit: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct EventRow {
    id: Uuid,
    #[sqlx(rename = "type")]
    event_type: String,
    payload: Value,
    created_at: DateTime<Utc>,
}

/// How a business catches up on webhooks it missed: remember the last event
/// id you processed and page forward from it.
pub async fn list_events(
    State(st): State<AppState>,
    auth: Auth,
    ApiQuery(params): ApiQuery<EventParams>,
) -> Result<Json<Value>, AppError> {
    let limit = page_limit(params.limit);
    let rows: Vec<EventRow> = sqlx::query_as(
        "SELECT id, type, payload, created_at FROM events
          WHERE business_id = $1 AND ($2::uuid IS NULL OR id > $2)
          ORDER BY id ASC LIMIT $3",
    )
    .bind(auth.business_id)
    .bind(params.after)
    .bind(limit + 1)
    .fetch_all(&st.pool)
    .await?;
    let data = rows
        .iter()
        .map(|e| json!({ "id": e.id, "object": "event", "type": e.event_type, "created_at": e.created_at, "data": e.payload }))
        .collect();
    Ok(Json(page(data, limit)))
}
