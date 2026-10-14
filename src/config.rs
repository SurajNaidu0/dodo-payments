use std::{env, time::Duration};

#[derive(Clone, Debug)]
pub struct Config {
    pub api_addr: String,
    pub database_url: String,
    pub psp_url: String,
    /// How long one PSP call may take before we treat the outcome as unknown.
    /// Deliberately far below tok_timeout's 30 seconds.
    pub psp_timeout: Duration,
    /// How often the reconciler wakes up.
    pub reconcile_interval: Duration,
    /// How long an invoice must be locked before the reconciler touches it.
    /// Must be well above `psp_timeout`, so no live request can still be running.
    pub reconcile_after: Duration,
    /// How often the webhook dispatcher polls for due deliveries.
    pub dispatch_interval: Duration,
    /// If set, a demo business with this API key is created at startup.
    pub demo_api_key: Option<String>,
}

impl Config {
    /// Defaults suitable for local runs and tests.
    pub fn local(psp_url: impl Into<String>) -> Self {
        Self {
            api_addr: "0.0.0.0:8080".into(),
            database_url: "postgres://postgres:postgres@localhost:5432/dodo".into(),
            psp_url: psp_url.into(),
            psp_timeout: Duration::from_millis(1_000),
            reconcile_interval: Duration::from_secs(15),
            reconcile_after: Duration::from_secs(60),
            dispatch_interval: Duration::from_millis(1_000),
            demo_api_key: None,
        }
    }

    pub fn from_env() -> Self {
        let mut c = Self::local(var("PSP_URL", "http://localhost:4000"));
        c.api_addr = var("API_ADDR", &c.api_addr);
        c.database_url = var("DATABASE_URL", &c.database_url);
        c.psp_timeout = Duration::from_millis(num("PSP_TIMEOUT_MS", 1_000));
        c.reconcile_interval = Duration::from_secs(num("RECONCILE_INTERVAL_SECS", 15));
        c.reconcile_after = Duration::from_secs(num("RECONCILE_AFTER_SECS", 60));
        c.dispatch_interval = Duration::from_millis(num("DISPATCH_INTERVAL_MS", 1_000));
        c.demo_api_key = env::var("DEMO_API_KEY").ok().filter(|k| !k.is_empty());
        c
    }
}

fn var(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn num(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
