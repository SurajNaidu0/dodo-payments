//! Required test: N concurrent POST /pay calls for one invoice.
//! At most one succeeds, nobody is double-charged, and the final state is consistent.

mod common;

use std::sync::Arc;

use reqwest::StatusCode;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

const N: usize = 25;

#[sqlx::test]
async fn concurrent_payments_charge_exactly_once(pool_opts: PgPoolOptions, connect_opts: PgConnectOptions) {
    // sqlx::test borrows connections from a shared parent pool of 20. Take 15
    // and leave the rest for its cleanup. 25 requests still race in Postgres.
    let pool = pool_opts.max_connections(15).connect_with(connect_opts).await.unwrap();
    let app = Arc::new(common::spawn_app(pool).await);
    let invoice = app.create_invoice().await;

    // Different idempotency keys: these are N genuinely different requests.
    let mut requests = tokio::task::JoinSet::new();
    for i in 0..N {
        let app = app.clone();
        requests.spawn(async move { app.pay(invoice, "tok_success", &format!("concurrent-{i}")).await });
    }

    let (mut succeeded, mut rejected) = (0, 0);
    while let Some(result) = requests.join_next().await {
        let (status, body) = result.unwrap();
        match status {
            StatusCode::OK => succeeded += 1,
            StatusCode::CONFLICT => {
                let code = body["error"]["code"].as_str().unwrap();
                assert!(
                    code == "payment_in_progress" || code == "invoice_already_paid",
                    "unexpected 409 body: {body}"
                );
                rejected += 1;
            }
            other => panic!("unexpected status {other}: {body}"),
        }
    }

    assert_eq!(succeeded, 1, "exactly one payment should succeed");
    assert_eq!(rejected, N - 1);
    assert_eq!(app.invoice_state(invoice).await, "paid");
    assert_eq!(app.count_attempts(invoice, "succeeded").await, 1);
    assert_eq!(app.count_attempts(invoice, "pending").await, 0, "no attempt left hanging");
    // The losers never got past the lock, so the PSP was called exactly once.
    assert_eq!(app.mock.charges_created(), 1, "customer charged more than once");
}
