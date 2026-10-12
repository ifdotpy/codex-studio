use axum::Json;
use axum::extract::State;
use serde_json::json;

use crate::router::HttpState;

pub(crate) async fn get(State(state): State<HttpState>) -> Json<serde_json::Value> {
    Json(json!({"token": state.token}))
}
