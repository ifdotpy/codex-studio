//! The worktrees that cleanup kept for workers of a team, including deleted workers.
use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use studio_runtime_store::{AgentId, AgentIndexedReads, AgentRepository};

use super::{
    extractors::QueryParams,
    manifest::{OperationStatus, RouteGroup, get},
};
use crate::{error::ApiError, router::HttpState};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![get(
        "/api/retained-worktrees",
        OperationStatus::Implemented,
        retained_worktrees,
    )])
}

async fn retained_worktrees(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let id = query
        .nonempty_str("agent")
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Supply an agent ID"))?
        .to_owned();
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = store
        .read_async("runtime-http/retained-worktrees", move |connection| {
            let root_id = AgentRepository::get(connection, &AgentId::from(id.as_str()))
                .map_err(ApiError::stored)?
                .map(|agent| agent.root_id.0)
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Agent not found"))?;
            // Cleanup ends after deletion. A deleted worker has no live sync row, so the
            // team view reads the stored records.
            let members =
                AgentIndexedReads::team_records(connection, &root_id).map_err(ApiError::stored)?;
            let retained_worktrees = members
                .iter()
                .filter_map(|(_, raw)| serde_json::from_str::<Value>(raw).ok())
                .filter_map(|member| {
                    let mut value =
                        studio_runtime_services::sync_views::retained_worktree_entity(&member)?;
                    value.as_object_mut()?.remove("rootId");
                    Some(value)
                })
                .collect::<Vec<_>>();
            Ok::<_, ApiError>(json!({"rootId": root_id, "retainedWorktrees": retained_worktrees}))
        })
        .await?;
    Ok(Json(result))
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "retained_worktrees",
        routes: Some(routes),
        dependencies: &[],
    }
}
