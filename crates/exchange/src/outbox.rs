use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::error::Result;

/// Dispatches a claimed outbox command to the resolved adapter. The server
/// owns the claim/lease/persist loop (SKIP LOCKED, sqlx) — this trait only
/// describes the dispatch call itself, so `crates/exchange` stays sqlx-free.
#[async_trait]
pub trait CommandDispatcher: Send + Sync {
    async fn dispatch(
        &self,
        account_id: Uuid,
        command_type: &str,
        payload: &Value,
    ) -> Result<()>;
}
