use axum::Json;
use axum::extract::State;
use serde_json::json;

use crate::router::HttpState;

use super::manifest::{OperationStatus, RouteGroup, get as register_get};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![register_get(
        "/api/session",
        OperationStatus::Implemented,
        get,
    )])
}

pub(crate) async fn get(State(state): State<HttpState>) -> Json<serde_json::Value> {
    Json(json!({"token": state.token}))
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "session",
        routes: Some(routes),
        dependencies: &[],
    }
}
