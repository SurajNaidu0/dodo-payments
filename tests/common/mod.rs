//! Test harness. Each test gets:
//! - a fresh, migrated Postgres database (created by `#[sqlx::test]`),
//! - the real API router and a mock PSP, both served on random local ports,
//! - a new business and API key.
//!
//! Background workers are NOT started. Tests call `reconcile_once` and
//! `dispatch_once` themselves, so timing is deterministic.

#![allow(dead_code)]

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::Router;
use dodo_invoices::{
    config::Config,
    mock_psp::{self, MockPsp},
    router, seed, AppState,
};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

/// How long the mock PSP sleeps for tok_timeout in tests.
pub const MOCK_TIMEOUT: Duration = Duration::from_secs(3);
/// Our PSP client timeout in tests. Shorter than MOCK_TIMEOUT on purpose.
pub const PSP_CLIENT_TIMEOUT: Duration = Duration::from_secs(1);

pub struct TestApp {
    pub base: String,
    pub psp_base: String,
    pub pool: PgPool,
    pub key: String,
    pub http: reqwest::Client,
    pub mock: Arc<MockPsp>,
    pub state: AppState,
}

pub async fn spawn_app(pool: PgPool) -> TestApp {
    let mock = MockPsp::new(MOCK_TIMEOUT);
    let psp_addr = serve(mock_psp::router(mock.clone())).await;

    let mut config = Config::local(format!("http://{psp_addr}"));
    config.psp_timeout = PSP_CLIENT_TIMEOUT;
    let state = AppState::new(pool.clone(), config);
    let api_addr = serve(router(state.clone())).await;

    let (_, key) = seed::create_business_with_key(&pool, "Test Business").await.unwrap();
    TestApp {
        base: format!("http://{api_addr}"),
        psp_base: format!("http://{psp_addr}"),
        pool,
        key,
        http: reqwest::Client::new(),
        mock,
        state,
    }
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

impl TestApp {
    pub async fn request(&self, method: Method, path: &str, body: Option<Value>, idempotency_key: Option<&str>) -> (StatusCode, Value) {
        let mut req = self.http.request(method, format!("{}{}", self.base, path)).bearer_auth(&self.key);
        if let Some(key) = idempotency_key {
            req = req.header("Idempotency-Key", key);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let res = req.send().await.expect("request failed");
        let status = res.status();
        let body = res.json::<Value>().await.unwrap_or(Value::Null);
        (status, body)
    }

    /// Creates a customer and an invoice of 3 × $12.50 + 1 × $50.00 = $87.50.
    pub async fn create_invoice(&self) -> Uuid {
        // Emails are unique per business, and some tests create several invoices.
        let email = format!("ada+{}@example.com", Uuid::now_v7());
        let (status, customer) = self
            .request(Method::POST, "/customers", Some(json!({ "name": "Ada Lovelace", "email": email })), None)
            .await;
        assert_eq!(status, StatusCode::CREATED, "{customer}");

        let (status, invoice) = self
            .request(
                Method::POST,
                "/invoices",
                Some(json!({
                    "customer_id": customer["id"],
                    "due_date": "2030-01-31",
                    "line_items": [
                        { "description": "Widget", "quantity": 3, "unit_amount_cents": 1250 },
                        { "description": "Support", "quantity": 1, "unit_amount_cents": 5000 }
                    ]
                })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{invoice}");
        assert_eq!(invoice["total_cents"], 8750);
        assert_eq!(invoice["state"], "open");
        invoice["id"].as_str().unwrap().parse().unwrap()
    }

    pub async fn pay(&self, invoice: Uuid, card_token: &str, idempotency_key: &str) -> (StatusCode, Value) {
        self.request(
            Method::POST,
            &format!("/invoices/{invoice}/pay"),
            Some(json!({ "card_token": card_token })),
            Some(idempotency_key),
        )
        .await
    }

    pub async fn invoice_state(&self, invoice: Uuid) -> String {
        let (status, body) = self.request(Method::GET, &format!("/invoices/{invoice}"), None, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["state"].as_str().unwrap().to_string()
    }

    pub async fn count_attempts(&self, invoice: Uuid, status: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM payment_attempts WHERE invoice_id = $1 AND status = $2")
            .bind(invoice)
            .bind(status)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}
