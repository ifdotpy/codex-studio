pub(crate) mod compose;
pub(crate) mod extractors;
pub(crate) mod manifest;

mod agents_create;
mod catalog;
mod desktop;
mod messages;
mod monitors;
mod panels;
mod questions_reads;
mod queue_read;
mod queue_write;
mod rules;
mod session;
mod sync;
mod sync_documents;
mod system_reads;
mod terminals;
mod tool_requests;
mod transcript;
mod work_items;
mod work_write;
mod workspace_files;
mod workspace_views;

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
        agents_create::routes(),
        catalog::routes(),
        desktop::routes(),
        messages::routes(),
        monitors::routes(),
        panels::routes(),
        questions_reads::routes(),
        rules::routes(),
        session::routes(),
        sync::routes(),
        sync_documents::routes(),
        system_reads::routes(),
        terminals::routes(),
        tool_requests::routes(),
        transcript::routes(),
        work_items::routes(),
        work_write::routes(),
        workspace_files::routes(),
        workspace_views::routes(),
    ]
}
