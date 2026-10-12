use axum::Json;
use axum::extract::State;
use serde_json::json;

use crate::router::HttpState;

pub(crate) async fn identity(State(state): State<HttpState>) -> Json<serde_json::Value> {
    Json(json!({"workspaceId": state.workspace_id, "syncProtocol": 2}))
}
