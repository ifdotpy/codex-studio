//! Transaction adapters for route mutations; entity delivery belongs to the stream.

use std::future::Future;
use std::sync::Arc;

use serde_json::Value;
use studio_runtime_store::{RuntimeStore, StoreError, Transaction};

use crate::error::ApiError;

pub(crate) async fn write<E>(
    store: Arc<RuntimeStore>,
    operation: impl FnOnce(&Transaction<'_>) -> Result<Value, E> + Send + 'static,
    map_error: impl FnOnce(E) -> ApiError + Send + 'static,
) -> Result<Value, ApiError>
where
    E: From<StoreError> + Send + 'static,
{
    store
        .transaction_with_error(operation)
        .await
        .map_err(map_error)
}

pub(crate) async fn write_external_async<E, F>(
    _store: Arc<RuntimeStore>,
    operation: impl FnOnce() -> F + Send + 'static,
    map_error: impl FnOnce(E) -> ApiError,
) -> Result<Value, ApiError>
where
    E: From<StoreError>,
    F: Future<Output = Result<Value, E>> + Send,
{
    operation().await.map_err(map_error)
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "sync_delta",
        routes: None,
        dependencies: &[],
    }
}
