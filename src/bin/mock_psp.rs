use std::{env, time::Duration};

use dodo_invoices::mock_psp::{router, MockPsp};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let addr = env::var("MOCK_PSP_ADDR").unwrap_or_else(|_| "0.0.0.0:4000".into());
    let timeout_secs: u64 = env::var("MOCK_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30);

    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind mock PSP address");
    tracing::info!(%addr, timeout_secs, "mock PSP listening");
    axum::serve(listener, router(MockPsp::new(Duration::from_secs(timeout_secs))))
        .await
        .expect("server error");
}
