pub(crate) mod compose;
pub(crate) mod extractors;
pub(crate) mod manifest;

mod catalog;
mod desktop;
mod rules;
mod session;
mod sync;
mod sync_documents;
mod terminals;
mod work_items;

use manifest::RouteGroup;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

fn generated_json<T: DeserializeOwned + Serialize>(
    value: Value,
) -> Result<Value, crate::error::ApiError> {
    serde_json::from_value::<T>(value)
        .and_then(serde_json::to_value)
        .map_err(|_| crate::error::ApiError::internal())
}

pub(crate) fn groups() -> Vec<RouteGroup> {
    vec![
        session::routes(),
        rules::routes(),
        sync::routes(),
        sync_documents::routes(),
        terminals::routes(),
        work_items::routes(),
        desktop::routes(),
        catalog::routes(),
    ]
}
