mod common;

use reqwest::{Method, StatusCode};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test]
async fn duplicate_email_is_rejected_case_insensitively(pool: PgPool) {
    let app = common::spawn_app(pool).await;

    let (status, first) = app
        .request(Method::POST, "/customers", Some(json!({ "name": "Ada", "email": "ada@example.com" })), None)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");

    let (status, body) = app
        .request(Method::POST, "/customers", Some(json!({ "name": "Ada 2", "email": " ADA@Example.com " })), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "customer_already_exists");
    assert_eq!(body["error"]["details"]["customer_id"], first["id"]);
}
