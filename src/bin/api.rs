use std::time::Duration;

use dodo_invoices::{config::Config, reconciler, router, seed, webhook_dispatcher, AppState, MIGRATOR};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::from_env();
    let pool = connect_with_retry(&config.database_url).await;
    MIGRATOR.run(&pool).await.expect("failed to run migrations");
    tracing::info!("migrations applied");

    if let Some(key) = &config.demo_api_key {
        seed::ensure_demo_business(&pool, key).await.expect("failed to seed demo business");
        tracing::info!("demo business ready; use the DEMO_API_KEY as your Bearer token");
    }

    let state = AppState::new(pool, config.clone());
    reconciler::spawn(state.clone());
    webhook_dispatcher::spawn(state.clone());

    let listener = tokio::net::TcpListener::bind(&config.api_addr).await.expect("failed to bind API address");
    tracing::info!(addr = %config.api_addr, psp = %config.psp_url, "API listening");
    axum::serve(listener, router(state)).await.expect("server error");
}

async fn connect_with_retry(url: &str) -> sqlx::PgPool {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match PgPoolOptions::new().max_connections(20).connect(url).await {
            Ok(pool) => return pool,
            Err(e) if attempt < 30 => {
                tracing::warn!(error = %e, attempt, "database not ready, retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => panic!("could not connect to database: {e}"),
        }
    }
}
