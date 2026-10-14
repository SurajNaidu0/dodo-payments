//! Required test: retrying with the same Idempotency-Key returns the same
//! response without a second PSP call. Plus the related edge cases.

mod common;

use reqwest::StatusCode;
use sqlx::PgPool;

#[sqlx::test]
async fn same_key_returns_same_response_without_second_psp_call(pool: PgPool) {
    let app = common::spawn_app(pool).await;
    let invoice = app.create_invoice().await;

    let (status1, body1) = app.pay(invoice, "tok_success", "key-1").await;
    assert_eq!(status1, StatusCode::OK, "{body1}");
    assert_eq!(body1["invoice"]["state"], "paid");
    assert_eq!(app.mock.charges_created(), 1);

    let (status2, body2) = app.pay(invoice, "tok_success", "key-1").await;
    assert_eq!(status2, status1);
    assert_eq!(body2, body1, "replay must return the identical body");
    assert_eq!(app.mock.charges_created(), 1, "replay must not call the PSP again");
    assert_eq!(app.count_attempts(invoice, "succeeded").await, 1);
}

#[sqlx::test]
async fn declined_payment_replays_identically_and_invoice_stays_payable(pool: PgPool) {
    let app = common::spawn_app(pool).await;
    let invoice = app.create_invoice().await;

    let (status1, body1) = app.pay(invoice, "tok_card_declined", "decline-1").await;
    assert_eq!(status1, StatusCode::PAYMENT_REQUIRED, "{body1}");
    assert_eq!(body1["error"]["code"], "payment_failed");
    assert_eq!(body1["error"]["details"]["decline_code"], "card_declined");
    assert_eq!(app.invoice_state(invoice).await, "open", "a decline returns the invoice to open");

    let (status2, body2) = app.pay(invoice, "tok_card_declined", "decline-1").await;
    assert_eq!((status2, &body2), (status1, &body1));
    assert_eq!(app.mock.charges_created(), 1);

    // The customer can retry with a different card and a new key.
    let (status3, body3) = app.pay(invoice, "tok_success", "decline-2").await;
    assert_eq!(status3, StatusCode::OK, "{body3}");
    assert_eq!(app.invoice_state(invoice).await, "paid");
}

#[sqlx::test]
async fn reusing_a_key_with_a_different_body_is_rejected(pool: PgPool) {
    let app = common::spawn_app(pool).await;
    let invoice = app.create_invoice().await;

    let (status, _) = app.pay(invoice, "tok_card_declined", "reused").await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);

    let (status, body) = app.pay(invoice, "tok_success", "reused").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "idempotency_key_reused");
    assert_eq!(app.mock.charges_created(), 1);
}

#[sqlx::test]
async fn paying_a_paid_invoice_with_a_new_key_is_rejected(pool: PgPool) {
    let app = common::spawn_app(pool).await;
    let invoice = app.create_invoice().await;
    assert_eq!(app.pay(invoice, "tok_success", "first").await.0, StatusCode::OK);

    let (status, body) = app.pay(invoice, "tok_success", "second").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "invoice_already_paid");
    assert_eq!(app.mock.charges_created(), 1);

    // The 409 is stored too, so a retry of that request gets the same answer.
    let (status2, body2) = app.pay(invoice, "tok_success", "second").await;
    assert_eq!((status2, body2), (status, body));
}
