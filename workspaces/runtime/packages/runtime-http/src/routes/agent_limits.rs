//! User controls for worker turn limits and authoritative team budget totals.
use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_services::{budget_runtime, coordination, message_admission};
use studio_runtime_store::{AgentId, AgentRepository, QueueEventId, QueueEventRepository};

use super::{
    extractors::{ApiJson, QueryParams},
    manifest::{OperationStatus, RouteGroup, get, post},
};
use crate::{error::ApiError, router::HttpState};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/agents/continue",
            OperationStatus::Implemented,
            continue_worker,
        ),
        get(
            "/api/team-budget",
            OperationStatus::Implemented,
            team_budget,
        ),
    ])
}

// The HTTP boundary requires the user's canvas token. Agent tools retain their
// existing sender identity and cannot select this user-only operation.
async fn continue_worker(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::WorkerContinueRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let admission_store = Arc::clone(&store);
    let recipient = body.id.clone();
    let override_admission = state.message_admission.clone();
    let snapshot = store
        .read_async("runtime-http/continue-worker", move |connection| {
            if override_admission.is_some() {
                message_admission::PreparedAdmission::read(connection, &recipient)
                    .map(message_admission::AdmissionSnapshot::Workspace)
            } else {
                message_admission::MessageIngressAdmission::read(
                    admission_store,
                    connection,
                    &recipient,
                )
                .map(message_admission::AdmissionSnapshot::Ingress)
            }
        })
        .await
        .map_err(super::messages::coordination_error)?;
    let prepared = snapshot
        .prepare()
        .await
        .map_err(super::messages::coordination_error)?;
    let admission = state
        .message_admission
        .unwrap_or_else(|| Arc::from(prepared));
    let posted = store
        .transaction_with_error(move |tx| {
            let worker = AgentRepository::get(tx, &AgentId::from(body.id.as_str()))?.ok_or(
                coordination::CoordinationError::InvalidRequest("Worker not found"),
            )?;
            let root = AgentRepository::get(tx, &worker.root_id)?.ok_or(
                coordination::CoordinationError::InvalidRequest("Root lead not found"),
            )?;
            if worker.id == root.id
                || root.extra.get("isLead") != Some(&json!(true))
                || worker
                    .extra
                    .get("remoteWorker")
                    .is_some_and(|value| !value.is_null())
            {
                return Err(coordination::CoordinationError::InvalidRequest(
                    "Select a local worker of the root lead",
                ));
            }
            let replay =
                QueueEventRepository::get(tx, &QueueEventId::from(body.request_id.as_str()))?
                    .is_some();
            if !replay
                && worker.extra.get("terminalReason").and_then(Value::as_str)
                    != Some(studio_runtime_schema::worker_capabilities::TURN_LIMIT_REASON)
            {
                return Err(coordination::CoordinationError::InvalidRequest(
                    "The worker is not stopped at its turn limit",
                ));
            }
            coordination::send_followup(
                tx,
                coordination::AgentFollowup {
                    sender_id: &root.id.0,
                    sender_epoch: root.epoch,
                    recipient_id: &body.id,
                    id: &body.request_id,
                    text: "Continue the assigned work. The user reset the worker turn allowance.",
                    delivery: studio_runtime_schema::DeliveryMoment::NextTurn,
                    now: studio_runtime_services::event_write::unix_time().map_err(|_| {
                        coordination::CoordinationError::InvalidRequest("Clock unavailable")
                    })?,
                },
                admission.as_ref(),
            )
        })
        .await
        .map_err(super::messages::coordination_error)?;
    posted.notify_committed(|| state.event_wake.notify_one());
    Ok(Json(posted.receipt))
}

async fn team_budget(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let id = query
        .nonempty_str("agent")
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Supply an agent ID"))?
        .to_owned();
    let store = state.store.ok_or_else(ApiError::internal)?;
    let root_id = store
        .read_async("runtime-http/team-budget", move |connection| {
            AgentRepository::get(connection, &AgentId::from(id.as_str()))
                .map_err(ApiError::stored)?
                .map(|agent| agent.root_id.0)
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Agent not found"))
        })
        .await?;
    let histories = budget_runtime::read_histories(&store, &root_id)
        .await
        .map_err(|_| ApiError::internal())?;
    let now =
        studio_runtime_services::event_write::unix_time().map_err(|_| ApiError::internal())?;
    let budget_store = Arc::clone(&store);
    let status = store
        .transaction_with_error(move |tx| {
            let status = budget_runtime::status(&budget_store, tx, &root_id, &histories, now)?;
            // Cleanup begins after deletion. These agents no longer have live
            // sync rows, so the team view must also read the retained records.
            let retained = studio_runtime_store::budget::BudgetRepository::team(tx, &root_id)?
                .into_iter()
                .filter(|member| {
                    matches!(member.pointer("/workspaceCleanupIntent/phase").and_then(Value::as_str), Some("not_sent" | "unknown"))
                        && matches!(member.pointer("/workspaceCleanupIntent/effect/kind").and_then(Value::as_str), Some("remove_worktree" | "save_worktree_ref"))
                        && member.get("remoteWorker").is_none_or(Value::is_null)
                })
                .filter_map(|member| {
                    let projected = studio_runtime_services::sync_views::project_persisted_record("agent", &member)?;
                    Some(json!({"id":member["id"],"name":member.get("name").and_then(Value::as_str),"cleanup":projected["workspaceCleanupIntent"]}))
                })
                .collect::<Vec<_>>();
            let mut result = json!(status);
            result["retainedWorktrees"] = json!(retained);
            Ok::<_, studio_runtime_services::budget::BudgetError>(result)
        })
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(Json(status))
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "agent_limits",
        routes: Some(routes),
        dependencies: &[],
    }
}
