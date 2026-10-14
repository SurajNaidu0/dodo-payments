//! Webhooks: written to the outbox with the state change, delivered by the
//! dispatcher, signed so the receiver can verify them.

mod common;

use chrono::Utc;
use dodo_invoices::{webhook_dispatcher::dispatch_once, webhook_signing};
use reqwest::{Method, StatusCode};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test]
async fn events_are_delivered_signed(pool: PgPool) {
    let app = common::spawn_app(pool).await;
    let (status, endpoint) = app
        .request(Method::POST, "/webhook_endpoints", Some(json!({ "url": format!("{}/webhook-sink", app.psp_base) })), None)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{endpoint}");
    let secret = endpoint["secret"].as_str().unwrap().to_string();

    let invoice = app.create_invoice().await;
    app.pay(invoice, "tok_card_declined", "k1").await;
    app.pay(invoice, "tok_success", "k2").await;

    // Nothing is sent on the request path. The dispatcher sends it.
    assert!(app.mock.received_webhooks().is_empty());
    let sent = dispatch_once(&app.state.pool, &app.state.http).await.unwrap();
    assert_eq!(sent, 3);

    let received = app.mock.received_webhooks();
    let mut types: Vec<_> = received.iter().map(|w| w["event_type"].as_str().unwrap().to_string()).collect();
    types.sort();
    assert_eq!(types, ["invoice.created", "invoice.paid", "invoice.payment_failed"]);

    for webhook in &received {
        let header = webhook["signature"].as_str().unwrap();
        let raw = webhook["raw_body"].as_str().unwrap();
        assert!(webhook_signing::verify(&secret, header, raw.as_bytes(), Utc::now().timestamp(), 300));
        assert!(!webhook_signing::verify("whsec_wrong", header, raw.as_bytes(), Utc::now().timestamp(), 300));
    }

    let (_, deliveries) = app.request(Method::GET, "/webhook_deliveries", None, None).await;
    assert!(deliveries["data"].as_array().unwrap().iter().all(|d| d["status"] == "succeeded"));
}

#[sqlx::test]
async fn failing_endpoint_is_scheduled_for_retry(pool: PgPool) {
    let app = common::spawn_app(pool).await;
    // Nothing listens on port 9, so every delivery fails.
    app.request(Method::POST, "/webhook_endpoints", Some(json!({ "url": "http://127.0.0.1:9/hook" })), None).await;
    app.create_invoice().await;

    dispatch_once(&app.state.pool, &app.state.http).await.unwrap();
    let (_, deliveries) = app.request(Method::GET, "/webhook_deliveries", None, None).await;
    let d = &deliveries["data"][0];
    assert_eq!(d["status"], "pending");
    assert_eq!(d["attempt_count"], 1);
    assert!(d["last_error"].is_string());

    // Not due yet (first retry is ~30 s away), so a second pass sends nothing.
    assert_eq!(dispatch_once(&app.state.pool, &app.state.http).await.unwrap(), 0);
}
