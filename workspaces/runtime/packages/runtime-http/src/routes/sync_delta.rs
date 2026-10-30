//! Transaction adapters for route mutations; entity delivery belongs to the stream.

use std::sync::Arc;

use studio_runtime_store::{RuntimeStore, StoreError, Transaction};

use crate::error::ApiError;

pub(crate) async fn write<T, E>(
    store: Arc<RuntimeStore>,
    operation: impl FnOnce(&Transaction<'_>) -> Result<T, E> + Send + 'static,
    map_error: impl FnOnce(E) -> ApiError + Send + 'static,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    E: From<StoreError> + Send + 'static,
{
    store
        .transaction_with_error(operation)
        .await
        .map_err(map_error)
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "sync_delta",
        routes: None,
        dependencies: &[],
    }
}
