//! Invoice & Payment Service.
//!
//! The binary in `src/bin/api.rs` wires this together. Tests build the same
//! router in-process, so they exercise exactly the production code.

pub mod auth;
pub mod config;
pub mod error;
pub mod events;
pub mod idempotency;
pub mod mock_psp;
pub mod models;
pub mod money;
pub mod psp_client;
pub mod routes;
pub mod seed;
pub mod settlement;
pub mod state_machine;
pub mod webhook_signing;
pub mod workers;

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use sqlx::PgPool;
use tower_http::trace::TraceLayer;

pub use workers::{reconciler, webhook_dispatcher};

use crate::{config::Config, error::AppError, psp_client::PspClient, routes::*};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Shared state handed to every request handler and background worker.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub psp: PspClient,
    /// HTTP client for webhook delivery (separate from the PSP client).
    pub http: reqwest::Client,
    pub config: Arc<Config>,
}

impl AppState {
    pub fn new(pool: PgPool, config: Config) -> Self {
        let psp = PspClient::new(config.psp_url.clone(), config.psp_timeout);
        let http = reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client");
        Self { pool, psp, http, config: Arc::new(config) }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({ "status": "ok" })) }))
        .route("/customers", post(customers::create).get(customers::list))
        .route("/customers/{id}", get(customers::get))
        .route("/invoices", post(invoices::create).get(invoices::list))
        .route("/invoices/{id}", get(invoices::get))
        .route("/invoices/{id}/pay", post(payments::pay))
        .route("/invoices/{id}/void", post(invoices::void))
        .route(
            "/invoices/{id}/mark_uncollectible",
            post(invoices::mark_uncollectible),
        )
        .route("/payment_attempts/{id}", get(payments::get_attempt))
        .route(
            "/webhook_endpoints",
            post(webhooks::create_endpoint).get(webhooks::list_endpoints),
        )
        .route("/webhook_deliveries", get(webhooks::list_deliveries))
        .route("/events", get(webhooks::list_events))
        .fallback(|| async {
            AppError::new(
                axum::http::StatusCode::NOT_FOUND,
                "route_not_found",
                "No route matches this method and path.",
            )
        })
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
