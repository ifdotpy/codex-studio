//! Local read operations for multi-server access state.

use serde_json::Value;
use studio_runtime_store::{MultiServerRepository, RuntimeStore, StoreError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MultiServerError {
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn audit(store: &RuntimeStore) -> Result<Value, MultiServerError> {
    let connection = store.read()?;
    Ok(MultiServerRepository::audit(&connection)?)
}
