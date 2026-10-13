pub(crate) mod compose;
pub(crate) mod extractors;
pub(crate) mod manifest;

mod account_login;
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
mod terminals;
mod transcript;
mod work_items;
mod work_write;
mod workspace_files;
mod workspace_views;

use manifest::RouteGroup;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub(super) fn accounts_response(
    snapshot: studio_runtime_services::account_policy::AccountsSnapshot,
) -> Result<axum::Json<crate::generated::AccountsResponse>, crate::error::ApiError> {
    serde_json::from_value(serde_json::json!({
        "accounts": snapshot.accounts,
        "archivedAccounts": snapshot.archived_accounts,
        "defaultAccountKey": snapshot.default_account_key,
        "logins": snapshot.logins,
        "supportsDisconnect": true,
        "supportsDelete": true,
    }))
    .map(axum::Json)
    .map_err(|_| crate::error::ApiError::internal())
}

fn generated_json<T: DeserializeOwned + Serialize>(
    value: Value,
) -> Result<Value, crate::error::ApiError> {
    serde_json::from_value::<T>(value)
        .and_then(serde_json::to_value)
        .map_err(|_| crate::error::ApiError::internal())
}

pub(crate) fn groups() -> Vec<RouteGroup> {
    vec![
        account_login::routes(),
        agents_create::routes(),
        session::routes(),
        rules::routes(),
        questions_reads::routes(),
        sync::routes(),
        sync_documents::routes(),
        terminals::routes(),
        monitors::routes(),
        panels::routes(),
        transcript::routes(),
        work_items::routes(),
        messages::routes(),
        work_write::routes(),
        workspace_files::routes(),
        workspace_views::routes(),
        desktop::routes(),
        catalog::routes(),
    ]
}
