//! Required test: tok_timeout / tok_network_error must not leave the invoice
//! stuck. The endpoint answers quickly, and the reconciler resolves the rest.

mod common;

use std::time::{Duration, Instant};

use dodo_invoices::reconciler::reconcile_once;
use reqwest::{Method, StatusCode};
use sqlx::PgPool;

#[sqlx::test]
async fn timeout_returns_quickly_then_reconciler_marks_paid(pool: PgPool) {
    let app = common::spawn_app(pool).await;
    let invoice = app.create_invoice().await;

    // The mock PSP takes 3 s. Our client gives up after 1 s.
    let started = Instant::now();
    let (status, body) = app.pay(invoice, "tok_timeout", "slow-1").await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["payment_attempt"]["status"], "pending");
    assert!(elapsed < common::MOCK_TIMEOUT, "endpoint hung for {elapsed:?}");

    // While locked, nothing else can touch the invoice.
    assert_eq!(app.invoice_state(invoice).await, "locked");
    let (status, body) = app.request(Method::POST, &format!("/invoices/{invoice}/void"), None, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "payment_in_progress");
    let (status, body) = app.pay(invoice, "tok_success", "slow-2").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "payment_in_progress");

    // The PSP is still working: the reconciler must wait, not guess.
    let report = reconcile_once(&app.state.pool, &app.state.psp, Duration::ZERO).await.unwrap();
    assert_eq!(report.still_processing, 1);
    assert_eq!(app.invoice_state(invoice).await, "locked");

    // Once the PSP finishes, the reconciler copies its answer.
    tokio::time::sleep(common::MOCK_TIMEOUT).await;
    let report = reconcile_once(&app.state.pool, &app.state.psp, Duration::ZERO).await.unwrap();
    assert_eq!(report.paid, 1);
    assert_eq!(app.invoice_state(invoice).await, "paid");
    assert_eq!(app.mock.charges_created(), 1);

    // The caller can learn the result by retrying with the same key.
    let (status, body) = app.pay(invoice, "tok_timeout", "slow-1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["payment_attempt"]["status"], "succeeded");
}

#[sqlx::test]
async fn network_error_is_released_by_reconciler_and_invoice_is_payable_again(pool: PgPool) {
    let app = common::spawn_app(pool).await;
    let invoice = app.create_invoice().await;

    let (status, body) = app.pay(invoice, "tok_network_error", "net-1").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(app.invoice_state(invoice).await, "locked");

    // The PSP has no record of the charge, so no money moved: release the lock.
    let report = reconcile_once(&app.state.pool, &app.state.psp, Duration::ZERO).await.unwrap();
    assert_eq!(report.abandoned, 1);
    assert_eq!(app.invoice_state(invoice).await, "open");
    assert_eq!(app.count_attempts(invoice, "abandoned").await, 1);

    // Retrying the same key explains what happened.
    let (status, body) = app.pay(invoice, "tok_network_error", "net-1").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"]["code"], "psp_unavailable");

    // Not stuck: a new attempt goes through.
    let (status, body) = app.pay(invoice, "tok_success", "net-2").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(app.invoice_state(invoice).await, "paid");
}

#[sqlx::test]
async fn reconciler_respects_the_safety_window(pool: PgPool) {
    let app = common::spawn_app(pool).await;
    let invoice = app.create_invoice().await;
    app.pay(invoice, "tok_network_error", "net-1").await;

    // Locked just now, window is 60 s: too recent to touch.
    let report = reconcile_once(&app.state.pool, &app.state.psp, Duration::from_secs(60)).await.unwrap();
    assert_eq!(report.checked, 0);
    assert_eq!(app.invoice_state(invoice).await, "locked");
}

#[sqlx::test]
async fn failed_late_payment_on_uncollectible_returns_to_uncollectible(pool: PgPool) {
    let app = common::spawn_app(pool).await;
    let invoice = app.create_invoice().await;
    let (status, _) = app.request(Method::POST, &format!("/invoices/{invoice}/mark_uncollectible"), None, None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = app.pay(invoice, "tok_card_declined", "late-1").await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(app.invoice_state(invoice).await, "uncollectible", "a decline must not reopen a written-off invoice");

    let (status, _) = app.pay(invoice, "tok_success", "late-2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(app.invoice_state(invoice).await, "paid");
}
