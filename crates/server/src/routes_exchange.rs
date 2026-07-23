//! Exchange config CRUD. Credential encryption/decryption happens here,
//! just-in-time, and only here — never inside `db.rs`, never inside
//! `crates/exchange`. Mode-switch guard layer 1 (app-level check) runs
//! before the UPDATE; the DB trigger is layer 2, a backstop.

use axum::extract::{Path, State};
use axum::Json;
use confluence_exchange::config::EncryptionKey;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::db;
use crate::error::ApiError;
use crate::routes::AppState;

#[derive(Deserialize)]
pub struct PutExchangeConfig {
    pub mode: String,
    pub testnet: bool,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_secret: Option<String>,
    pub version: i32,
}

#[derive(Serialize)]
pub struct ExchangeConfigView {
    pub mode: String,
    pub testnet: bool,
    pub has_credentials: bool,
    pub version: i32,
}

fn encryption_key() -> Result<EncryptionKey, ApiError> {
    let hex_key = std::env::var("EXCHANGE_ENCRYPTION_KEY")
        .map_err(|_| ApiError::Invalid("EXCHANGE_ENCRYPTION_KEY not configured".into()))?;
    let bytes = hex::decode(&hex_key)
        .map_err(|_| ApiError::Invalid("EXCHANGE_ENCRYPTION_KEY must be 64 hex chars (32 bytes)".into()))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| ApiError::Invalid("EXCHANGE_ENCRYPTION_KEY must be exactly 32 bytes".into()))?;
    Ok(EncryptionKey::from_bytes(&arr))
}

mod hex {
    pub fn decode(s: &str) -> Result<Vec<u8>, ()> {
        if !s.len().is_multiple_of(2) {
            return Err(());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
            .collect()
    }
}

pub async fn get_exchange_config(
    State(state): State<AppState>,
    Path(account_id): Path<Uuid>,
) -> Result<Json<ExchangeConfigView>, ApiError> {
    let mut tx = db::tenant_tx(&state.pool, account_id).await?;
    let row = sqlx::query(
        "SELECT mode, testnet, api_key_ciphertext IS NOT NULL AS has_creds, version
         FROM exchange_configs WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Json(ExchangeConfigView {
        mode: row.get("mode"),
        testnet: row.get("testnet"),
        has_credentials: row.get("has_creds"),
        version: row.get("version"),
    }))
}

pub async fn put_exchange_config(
    State(state): State<AppState>,
    Path(account_id): Path<Uuid>,
    Json(body): Json<PutExchangeConfig>,
) -> Result<Json<ExchangeConfigView>, ApiError> {
    if body.mode != "paper" && body.mode != "live" {
        return Err(ApiError::Invalid("mode must be 'paper' or 'live'".into()));
    }

    let mut tx = db::tenant_tx(&state.pool, account_id).await?;
    db::lock_account(&mut tx, account_id).await?;

    let existing = sqlx::query(
        "SELECT mode, version, api_key_ciphertext IS NOT NULL AS has_creds
         FROM exchange_configs WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;

    let (old_mode, current_version, had_credentials) = match &existing {
        Some(r) => (
            Some(r.get::<String, _>("mode")),
            r.get::<i32, _>("version"),
            r.get::<bool, _>("has_creds"),
        ),
        None => (None, 0, false),
    };

    if let Some(old_mode) = &old_mode {
        if current_version != body.version {
            return Err(ApiError::Conflict(format!(
                "exchange config version is {current_version}, request had {}",
                body.version
            )));
        }
        // Mode-switch guard, layer 1 (application-level; the DB trigger in
        // migration 0008 is layer 2, a backstop against paths that bypass
        // this handler).
        if old_mode != &body.mode && db::has_open_activity(&mut tx, account_id).await? {
            return Err(ApiError::Conflict(
                "cannot switch exchange mode: account has open orders or positions".into(),
            ));
        }
    }

    let (key_ct, secret_ct): (Option<Vec<u8>>, Option<Vec<u8>>) =
        if body.api_key.is_some() || body.api_secret.is_some() {
            let key = encryption_key()?;
            let aad = account_id.to_string();
            let key_ct = body
                .api_key
                .as_ref()
                .map(|k| key.encrypt_fresh(k.as_bytes(), aad.as_bytes()))
                .transpose()?;
            let secret_ct = body
                .api_secret
                .as_ref()
                .map(|s| key.encrypt_fresh(s.as_bytes(), aad.as_bytes()))
                .transpose()?;
            (key_ct, secret_ct)
        } else {
            (None, None)
        };

    let new_version = current_version + 1;

    if existing.is_some() {
        sqlx::query(
            "UPDATE exchange_configs SET mode = $2, testnet = $3,
                 api_key_ciphertext = COALESCE($4, api_key_ciphertext),
                 api_secret_ciphertext = COALESCE($5, api_secret_ciphertext),
                 version = $6, updated_at = now()
             WHERE account_id = $1 AND version = $7",
        )
        .bind(account_id)
        .bind(&body.mode)
        .bind(body.testnet)
        .bind(&key_ct)
        .bind(&secret_ct)
        .bind(new_version)
        .bind(current_version)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO exchange_configs (account_id, mode, testnet, api_key_ciphertext,
                 api_secret_ciphertext, version)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(account_id)
        .bind(&body.mode)
        .bind(body.testnet)
        .bind(&key_ct)
        .bind(&secret_ct)
        .bind(new_version)
        .execute(&mut *tx)
        .await?;
    }

    let event_type = if old_mode.as_deref() != Some(body.mode.as_str()) {
        "exchange_mode_switched"
    } else if key_ct.is_some() || secret_ct.is_some() {
        "exchange_credentials_rotated"
    } else {
        "exchange_mode_switched"
    };
    db::insert_risk_event(
        &mut tx,
        account_id,
        event_type,
        &json!({ "mode": body.mode, "testnet": body.testnet, "version": new_version }),
    )
    .await?;

    tx.commit().await?;

    Ok(Json(ExchangeConfigView {
        mode: body.mode,
        testnet: body.testnet,
        has_credentials: key_ct.is_some() || had_credentials,
        version: new_version,
    }))
}
