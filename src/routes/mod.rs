pub mod customers;
pub mod invoices;
pub mod payments;
pub mod webhooks;

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// Cursor pagination: `?limit=20&starting_after=<id>`. IDs are UUIDv7, so
/// ordering by id is ordering by creation time.
#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,
    pub starting_after: Option<Uuid>,
}

pub fn page_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(20).clamp(1, 100)
}

/// Expects up to `limit + 1` items. The extra one only signals `has_more`.
pub fn page(mut items: Vec<Value>, limit: i64) -> Value {
    let has_more = items.len() as i64 > limit;
    items.truncate(limit as usize);
    json!({ "object": "list", "data": items, "has_more": has_more })
}
