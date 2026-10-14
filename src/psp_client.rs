//! HTTP client for the payment processor.
//!
//! Every charge carries the payment attempt id as the PSP's Idempotency-Key.
//! Sending the same attempt twice can never charge the card twice.

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

#[derive(Clone)]
pub struct PspClient {
    http: reqwest::Client,
    base_url: String,
    timeout: Duration,
}

/// Result of trying to charge.
#[derive(Debug, Clone, PartialEq)]
pub enum ChargeOutcome {
    Succeeded { psp_ref: String },
    Failed { code: String },
    /// Timeout, dropped connection or 5xx. The charge may or may not exist.
    /// The invoice stays locked until the reconciler finds out.
    Unknown { reason: String },
}

/// Result of asking the PSP about an earlier charge.
#[derive(Debug, Clone, PartialEq)]
pub enum ChargeStatus {
    Succeeded { psp_ref: String },
    Failed { code: String },
    Processing,
    NotFound,
}

#[derive(Deserialize)]
struct PspBody {
    status: String,
    psp_ref: Option<String>,
    code: Option<String>,
}

impl PspClient {
    pub fn new(base_url: impl Into<String>, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .build()
            .expect("failed to build PSP HTTP client");
        Self { http, base_url: base_url.into(), timeout }
    }

    pub async fn charge(&self, attempt_id: Uuid, amount_cents: i64, card_token: &str) -> ChargeOutcome {
        let sent = self
            .http
            .post(format!("{}/charges", self.base_url))
            .timeout(self.timeout)
            .header("Idempotency-Key", attempt_id.to_string())
            .json(&json!({ "amount_cents": amount_cents, "card_token": card_token }))
            .send()
            .await;

        let response = match sent {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return ChargeOutcome::Unknown { reason: format!("PSP did not answer within {:?}", self.timeout) }
            }
            Err(e) => return ChargeOutcome::Unknown { reason: format!("PSP request failed: {e}") },
        };

        let status = response.status();
        if status.is_server_error() {
            return ChargeOutcome::Unknown { reason: format!("PSP returned HTTP {status}") };
        }
        let body: PspBody = match response.json().await {
            Ok(b) => b,
            Err(e) => return ChargeOutcome::Unknown { reason: format!("unreadable PSP response: {e}") },
        };

        match (body.status.as_str(), body.psp_ref) {
            ("succeeded", Some(psp_ref)) => ChargeOutcome::Succeeded { psp_ref },
            ("failed", _) => ChargeOutcome::Failed { code: body.code.unwrap_or_else(|| "card_declined".into()) },
            (other, _) => ChargeOutcome::Unknown { reason: format!("PSP status '{other}'") },
        }
    }

    pub async fn lookup(&self, attempt_id: Uuid) -> Result<ChargeStatus, String> {
        let response = self
            .http
            .get(format!("{}/charges/{}", self.base_url, attempt_id))
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| format!("PSP lookup failed: {e}"))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(ChargeStatus::NotFound);
        }
        if !response.status().is_success() {
            return Err(format!("PSP lookup returned HTTP {}", response.status()));
        }
        let body: PspBody = response
            .json()
            .await
            .map_err(|e| format!("unreadable PSP lookup response: {e}"))?;

        match (body.status.as_str(), body.psp_ref) {
            ("succeeded", Some(psp_ref)) => Ok(ChargeStatus::Succeeded { psp_ref }),
            ("failed", _) => Ok(ChargeStatus::Failed { code: body.code.unwrap_or_else(|| "card_declined".into()) }),
            ("processing", _) => Ok(ChargeStatus::Processing),
            (other, _) => Err(format!("unexpected PSP status '{other}'")),
        }
    }
}
