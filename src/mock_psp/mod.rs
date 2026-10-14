//! Mock payment processor. Outcome is chosen by the card token:
//!
//! | token                  | behavior                                              |
//! |------------------------|-------------------------------------------------------|
//! | tok_success            | succeeded after ~100 ms                               |
//! | tok_insufficient_funds | failed (insufficient_funds) after ~100 ms             |
//! | tok_card_declined      | failed (card_declined) after ~100 ms                  |
//! | tok_timeout            | records "processing", sleeps (30 s), then succeeds    |
//! | tok_network_error      | HTTP 500, records nothing                             |
//!
//! Two additions beyond the assignment's spec, both standard on real PSPs:
//! - `Idempotency-Key` header: the same key never creates a second charge.
//! - `GET /charges/{key}`: look up what happened, used by our reconciler.
//!
//! It also hosts `/webhook-sink`, a receiver that logs webhooks for the demo.
//! State is in memory and lost on restart, which is fine for a mock.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Clone)]
struct Charge {
    status: &'static str,
    psp_ref: Option<String>,
    code: Option<&'static str>,
}

impl Charge {
    fn to_json(&self) -> Value {
        json!({ "status": self.status, "psp_ref": self.psp_ref, "code": self.code })
    }
}

pub struct MockPsp {
    charges: Mutex<HashMap<String, Charge>>,
    charges_created: AtomicU64,
    timeout: Duration,
    webhooks: Mutex<Vec<Value>>,
}

impl MockPsp {
    pub fn new(timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            charges: Mutex::new(HashMap::new()),
            charges_created: AtomicU64::new(0),
            timeout,
            webhooks: Mutex::new(Vec::new()),
        })
    }

    /// Number of distinct charges created. Tests use it to prove no double charge.
    pub fn charges_created(&self) -> u64 {
        self.charges_created.load(Ordering::SeqCst)
    }

    pub fn received_webhooks(&self) -> Vec<Value> {
        self.webhooks.lock().unwrap().clone()
    }
}

pub fn router(state: Arc<MockPsp>) -> Router {
    Router::new()
        .route("/charges", post(create_charge))
        .route("/charges/{key}", get(get_charge))
        .route("/stats", get(stats))
        .route("/webhook-sink", post(receive_webhook).get(list_webhooks))
        .with_state(state)
}

#[derive(Deserialize)]
struct ChargeRequest {
    #[allow(dead_code)]
    amount_cents: i64,
    card_token: String,
}

async fn create_charge(
    State(psp): State<Arc<MockPsp>>,
    headers: HeaderMap,
    Json(req): Json<ChargeRequest>,
) -> Response {
    let Some(key) = headers.get("Idempotency-Key").and_then(|v| v.to_str().ok()).map(str::to_string) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "status": "failed", "code": "missing_idempotency_key" })))
            .into_response();
    };

    let new_charge = {
        let mut charges = psp.charges.lock().unwrap();
        if let Some(existing) = charges.get(&key) {
            tracing::info!(%key, status = existing.status, "mock PSP: replaying existing charge");
            return Json(existing.to_json()).into_response();
        }
        let charge = match req.card_token.as_str() {
            "tok_network_error" => {
                tracing::info!(%key, "mock PSP: simulating network error");
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "simulated network error" })))
                    .into_response();
            }
            "tok_success" => Charge { status: "succeeded", psp_ref: Some(Uuid::new_v4().to_string()), code: None },
            "tok_insufficient_funds" => Charge { status: "failed", psp_ref: None, code: Some("insufficient_funds") },
            "tok_card_declined" => Charge { status: "failed", psp_ref: None, code: Some("card_declined") },
            "tok_timeout" => Charge { status: "processing", psp_ref: None, code: None },
            _ => Charge { status: "failed", psp_ref: None, code: Some("invalid_card_token") },
        };
        charges.insert(key.clone(), charge.clone());
        psp.charges_created.fetch_add(1, Ordering::SeqCst);
        charge
    };
    tracing::info!(%key, token = %req.card_token, status = new_charge.status, "mock PSP: charge created");

    if new_charge.status == "processing" {
        // Finish the charge in a separate task. Like a real PSP, the charge
        // completes even if the caller gives up and disconnects (which makes
        // the server drop this handler).
        let finisher = {
            let psp = psp.clone();
            let key = key.clone();
            tokio::spawn(async move {
                tokio::time::sleep(psp.timeout).await;
                let done = Charge { status: "succeeded", psp_ref: Some(Uuid::new_v4().to_string()), code: None };
                psp.charges.lock().unwrap().insert(key.clone(), done.clone());
                tracing::info!(%key, "mock PSP: slow charge finished");
                done
            })
        };
        let done = finisher.await.expect("finisher task panicked");
        return Json(done.to_json()).into_response();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    Json(new_charge.to_json()).into_response()
}

async fn get_charge(State(psp): State<Arc<MockPsp>>, Path(key): Path<String>) -> Response {
    match psp.charges.lock().unwrap().get(&key) {
        Some(charge) => Json(charge.to_json()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "status": "not_found" }))).into_response(),
    }
}

async fn stats(State(psp): State<Arc<MockPsp>>) -> Json<Value> {
    Json(json!({ "charges_created": psp.charges_created() }))
}

async fn receive_webhook(State(psp): State<Arc<MockPsp>>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let raw = String::from_utf8_lossy(&body).to_string();
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    tracing::info!(
        event_type = ?header("Dodo-Event-Type"),
        event_id = ?header("Dodo-Event-Id"),
        signature = ?header("Dodo-Signature"),
        "webhook-sink: received webhook"
    );
    psp.webhooks.lock().unwrap().push(json!({
        "received_at": chrono::Utc::now(),
        "signature": header("Dodo-Signature"),
        "event_id": header("Dodo-Event-Id"),
        "event_type": header("Dodo-Event-Type"),
        "raw_body": raw,
        "body": parsed,
    }));
    StatusCode::OK
}

async fn list_webhooks(State(psp): State<Arc<MockPsp>>) -> Json<Value> {
    Json(json!({ "data": psp.received_webhooks() }))
}
