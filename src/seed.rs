//! There is no sign-up endpoint (deliberately cut). Businesses and keys are
//! created here: a demo business at startup, and fresh ones in tests.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::auth::{generate_api_key, hash_key, lookup_prefix};

pub const DEMO_BUSINESS_ID: Uuid = Uuid::from_u128(1);

pub async fn create_business_with_key(pool: &PgPool, name: &str) -> Result<(Uuid, String), sqlx::Error> {
    let business_id = Uuid::now_v7();
    let key = generate_api_key();
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO businesses (id, name) VALUES ($1, $2)")
        .bind(business_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    insert_key(&mut tx, business_id, &key).await?;
    tx.commit().await?;
    Ok((business_id, key))
}

/// Idempotent: safe to run on every startup.
pub async fn ensure_demo_business(pool: &PgPool, key: &str) -> Result<Uuid, String> {
    if lookup_prefix(key).is_none() {
        return Err("DEMO_API_KEY must start with 'sk_live_' and be at least 24 characters".into());
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("INSERT INTO businesses (id, name) VALUES ($1, 'Demo Business') ON CONFLICT (id) DO NOTHING")
        .bind(DEMO_BUSINESS_ID)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    insert_key(&mut tx, DEMO_BUSINESS_ID, key).await.map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(DEMO_BUSINESS_ID)
}

async fn insert_key(conn: &mut PgConnection, business_id: Uuid, key: &str) -> Result<(), sqlx::Error> {
    let prefix = lookup_prefix(key).expect("generated keys always have a valid prefix");
    sqlx::query(
        "INSERT INTO api_keys (id, business_id, prefix, key_hash) VALUES ($1, $2, $3, $4)
         ON CONFLICT (prefix) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(business_id)
    .bind(prefix)
    .bind(hash_key(key))
    .execute(conn)
    .await?;
    Ok(())
}
