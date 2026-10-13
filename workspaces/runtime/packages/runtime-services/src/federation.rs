//! Local, database-only federation state operations.

use serde_json::Value;
use studio_runtime_store::{FederationRepository, RuntimeStore, StoreError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FederationError {
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn snapshot(store: &RuntimeStore) -> Result<Value, FederationError> {
    let connection = store.read()?;
    Ok(FederationRepository::snapshot(&connection)?)
}

pub fn set_enabled(store: &RuntimeStore, enabled: bool) -> Result<(), FederationError> {
    store.transaction(|transaction| FederationRepository::set_enabled(transaction, enabled))?;
    Ok(())
}
