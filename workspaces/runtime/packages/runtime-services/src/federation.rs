//! Local, database-only federation state operations.

use serde_json::Value;
use studio_runtime_store::{FederationRepository, RuntimeStore, StoreError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FederationError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("federation action refused: {0}")]
    Refused(String),
    #[error("local federation identity is required")]
    NeedsIdentity,
}

pub fn snapshot(store: &RuntimeStore) -> Result<Value, FederationError> {
    Ok(store.read_transaction(|transaction| FederationRepository::snapshot(transaction))?)
}

pub fn manage(store: &RuntimeStore, body: &Value) -> Result<Value, FederationError> {
    store
        .transaction_with(|tx| {
            let action = body
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match action {
                "set_enabled" => FederationRepository::set_enabled(
                    tx,
                    body["enabled"].as_bool().unwrap_or(false),
                )?,
                "approve_peer" => FederationRepository::approve_peer(
                    tx,
                    body["state_id"].as_str().unwrap_or_default(),
                    body["accept_missing_whois"].as_bool().unwrap_or(false),
                )?,
                "revoke_peer" => FederationRepository::revoke_peer(
                    tx,
                    body["state_id"].as_str().unwrap_or_default(),
                )?,
                "room_options" => FederationRepository::room_options(
                    tx,
                    body["room_id"].as_str().unwrap_or_default(),
                    &body["disclosure"],
                )?,
                "create_room" => FederationRepository::room_action(tx, body, false)?,
                "approve_room" => FederationRepository::room_action(tx, body, true)?,
                _ => {
                    return Err(FederationError::Refused(format!(
                        "unsupported local federation action: {action}"
                    )));
                }
            }
            FederationRepository::snapshot(tx).map_err(FederationError::from)
        })
        .map_err(|error| match error {
            FederationError::Store(StoreError::SchemaMismatch(message))
                if message.starts_with("missing local federation identity") =>
            {
                FederationError::NeedsIdentity
            }
            FederationError::Store(StoreError::SchemaMismatch(message))
                if !message.starts_with("invalid stored object") =>
            {
                FederationError::Refused(message)
            }
            other => other,
        })
}
