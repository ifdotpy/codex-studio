use axum::Json;
use axum::extract::State;
use serde_json::json;

use crate::router::HttpState;

use super::manifest::{OperationStatus, RouteGroup, get as register_get};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![register_get(
        "/api/sync/identity",
        OperationStatus::Implemented,
        identity,
    )])
}

pub(crate) async fn identity(State(state): State<HttpState>) -> Json<serde_json::Value> {
    Json(json!({"workspaceId": state.workspace_id, "syncProtocol": 2}))
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "sync",
        routes: Some(routes),
        dependencies: &[],
    }
}
