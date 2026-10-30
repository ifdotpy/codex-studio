//! Capture, preview, and restore local Git checkpoints with durable reservations.

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::time::{Duration, Instant};
use studio_runtime_schema::{
    queue_command::Field,
    workspace_operation::{OperationPhase, WorkspaceOperationRecord},
};
use studio_runtime_services::checkpoints::{self, CheckpointsError};
use studio_runtime_services::edit_turn;
use studio_runtime_services::provider_adapter::{ProviderKind, ThreadRequest, adapter};
use studio_runtime_store::StoreError;
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, CheckpointsRepository, TranscriptRepository,
    workspace_operations::WorkspaceOperationsRepository,
};

use crate::error::ApiError;
use crate::router::HttpState;

const THREAD_FORK_DEPENDENCY: &str = "provider-session/thread-fork";

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post("/api/checkpoint", OperationStatus::Implemented, capture),
        post(
            "/api/checkpoint/preview",
            OperationStatus::Implemented,
            preview,
        ),
        post(
            "/api/checkpoint/restore",
            OperationStatus::Implemented,
            restore,
        ),
    ])
}

pub(crate) async fn capture(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::CheckpointCaptureRequest>,
) -> Result<Json<crate::generated::CheckpointCaptureResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let agent_id = body.agent;
    let label = body.label.unwrap_or_else(|| "Checkpoint".to_owned());
    let record = checkpoints::capture(store.clone(), agent_id, label)
        .await
        .map_err(map_error)?;
    let result = super::generated_json::<crate::generated::CheckpointCaptureResponse>(record)?;
    Ok(Json(result))
}

pub(crate) async fn preview(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::CheckpointPreviewRequest>,
) -> Result<Json<crate::generated::CheckpointPreviewResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let context = checkpoints::preview_context(&store, &body.agent, &body.checkpoint)
        .await
        .map_err(map_error)?;
    let provider_available = state.account_session.is_some();
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/checkpoints",
        move || {
            let mut value = checkpoints::preview(&store, context).map_err(map_error)?;
            if value["canRestore"] == true && !provider_available {
                value["canRestore"] = json!(false);
                value["restoreUnavailableReason"] =
                    json!("The provider session service is unavailable.");
            }
            super::generated_json::<crate::generated::CheckpointPreviewResponse>(value)
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

pub(crate) async fn restore(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::CheckpointRestoreRequest>,
) -> Result<Json<Value>, ApiError> {
    // The owned workflow must outlive a cancelled HTTP caller. A blocking Git
    // child cannot be cancelled by dropping its awaiting future.
    tokio::spawn(async move {
        let wake = state.event_wake.clone();
        let result = restore_owned(state, body).await;
        wake.notify_one();
        result
    })
    .await
    .map_err(|_| ApiError::internal())?
}

async fn restore_owned(
    state: HttpState,
    body: crate::generated::CheckpointRestoreRequest,
) -> Result<Json<Value>, ApiError> {
    // Port codex_workspace.py:1244-1455: reserve the workspace, fork/start
    // the provider thread, preserve the pre-restore tree, then restore Git and
    // durably settle transcript and workspace metadata.
    let checkpoint_id = body
        .checkpoint_id
        .flatten()
        .or(body.checkpoint.flatten())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Select a checkpoint"))?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let _execution = checkpoints::RestoreExecution::acquire(format!(
        "{}:{}",
        store.state_dir().display(),
        body.agent
    ))
    .map_err(map_error)?;
    let port = state
        .account_session
        .clone()
        .ok_or_else(|| ApiError::dependency(THREAD_FORK_DEPENDENCY))?;
    let snapshot = checkpoints::capture_context(&store, &body.agent)
        .await
        .map_err(map_error)?;
    let context = checkpoints::preview_context(&store, &body.agent, &checkpoint_id)
        .await
        .map_err(map_error)?;
    let (snapshot, checkpoint, operation_id, signature, current_tree) =
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/checkpoints",
            {
                let store = store.clone();
                let agent_id = body.agent.clone();
                let expected_tree = body.expected_tree.clone();
                move || {
                    if context.checkpoint.record.get("agent").and_then(Value::as_str) != Some(agent_id.as_str()) {
                        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Checkpoint belongs to another agent"));
                    }
                    if expected_tree.is_empty() {
                        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Preview this checkpoint before restore"));
                    }
                    let op_id = format!("restore:{agent_id}:{checkpoint_id}:{}", &studio_runtime_store::sha256_hex(expected_tree.as_bytes())[..32]);
                    let own_reservation = (snapshot.agent.record.get("workspaceReservationId").and_then(Value::as_str) == Some(op_id.as_str())).then_some(op_id.as_str());
                    checkpoints::ensure_restore_available(&snapshot, own_reservation).map_err(map_error)?;
                    let current_tree = checkpoints::snapshot_tree(&store, snapshot.agent.record["cwd"].as_str().ok_or_else(ApiError::internal)?).map_err(map_error)?;
                    let signature = studio_runtime_store::sha256_hex(
                        studio_runtime_store::python_json(&json!({"agent":agent_id,"checkpoint":checkpoint_id,"expectedTree":expected_tree})).as_bytes()
                    );
                    Ok::<_, ApiError>((snapshot, context.checkpoint, op_id, signature, current_tree))
                }
            },
        ).await.map_err(|_| ApiError::internal())??;
    let target_tree = checkpoint
        .record
        .get("tree")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(ApiError::internal)?
        .to_owned();
    let expected_tree = body.expected_tree;
    let cwd = snapshot
        .agent
        .record
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(ApiError::internal)?
        .to_owned();
    let provider = ProviderKind::of_record(&snapshot.agent.record)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "Unknown provider"))?;
    let validation_cwd = cwd.clone();
    let validation_tree = target_tree.clone();
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/checkpoints/validate-tree",
        move || checkpoints::validate_tree(&validation_cwd, &validation_tree).map_err(map_error),
    )
    .await
    .map_err(|_| ApiError::internal())??;
    let agent_id = body.agent;
    let source = CheckpointsRepository::workspace_source(&snapshot.agent.record);
    let account = source
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();
    let operation_state = {
        let store = store.clone();
        let snapshot = snapshot.clone();
        let checkpoint = checkpoint.clone();
        let operation_id = operation_id.clone();
        let signature = signature.clone();
        let agent_id = agent_id.clone();
        let expected_tree = expected_tree.clone();
        let current_tree = current_tree.clone();
        store.transaction_with_error(move |tx| {
                if !CheckpointsRepository::workspace_unchanged(tx, &snapshot).map_err(ApiError::stored)? {
                    return Err(ApiError::new(StatusCode::CONFLICT, "Workspace activity changed before restore"));
                }
                if let Some(mut old) = WorkspaceOperationsRepository::operation(tx, &operation_id).map_err(ApiError::stored)? {
                    if old.signature.value().map(String::as_str) != Some(signature.as_str()) {
                        return Err(ApiError::new(StatusCode::CONFLICT, "This restore request has different content"));
                    }
                    let mut expected_source = old.source.value().cloned().map(Value::Object).unwrap_or(Value::Null);
                    if old.phase == OperationPhase::Completed {
                        expected_source["threadId"] = old.provider.value().and_then(|provider| provider.get("thread")).and_then(|thread| thread.get("id")).cloned().unwrap_or(Value::Null);
                        if snapshot.agent.record["restoredCheckpoint"] != checkpoint.id {
                            return Err(ApiError::new(StatusCode::CONFLICT, "Restore state changed. Inspect the operation before retrying"));
                        }
                    }
                    if expected_source != source {
                        return Err(ApiError::new(StatusCode::CONFLICT, "Workspace operation source changed. Inspect the operation before retrying"));
                    }
                    if old.phase == OperationPhase::Completed { return Ok(old); }
                    let reacquire = old.phase == OperationPhase::Failed;
                    if reacquire && current_tree != expected_tree {
                        return Err(ApiError::new(StatusCode::CONFLICT, "Files changed after the preview. Preview again"));
                    }
                    if !matches!(old.phase, OperationPhase::ProviderPending | OperationPhase::ProviderReady | OperationPhase::LocalMutation | OperationPhase::RecoveryRequired) {
                        old.phase = OperationPhase::ProviderPending;
                        WorkspaceOperationsRepository::put_operation(tx, &old).map_err(ApiError::stored)?;
                    }
                    let mut actor = AgentRepository::get(tx, &AgentId::from(agent_id.as_str())).map_err(ApiError::stored)?.ok_or_else(ApiError::internal)?.value();
                    if actor["workspaceReservationId"] != operation_id && !(reacquire && actor["workspaceReservationId"].is_null()) {
                        return Err(ApiError::new(StatusCode::CONFLICT, "Restore reservation changed"));
                    }
                    actor["workspaceOperation"] = json!("restore");
                    actor["workspaceReservationId"] = json!(operation_id);
                    AgentRepository::put(tx, &AgentRecord::from_value(actor).map_err(|_| ApiError::internal())?).map_err(ApiError::stored)?;
                    return Ok(old);
                }
                if current_tree != expected_tree {
                    return Err(ApiError::new(StatusCode::CONFLICT, "Files changed after the preview. Preview again"));
                }
                let operation = WorkspaceOperationRecord::deserialize(json!({
                    "id":operation_id,"kind":"restore","agent":agent_id,"checkpoint":checkpoint.id,
                    "expectedTree":expected_tree,"signature":signature,"source":source,
                    "phase":"provider_pending","created":now_seconds(),
                    "rootRequestId":studio_runtime_schema::persistence::RequestIdentity::issue_root()
                })).map_err(ApiError::json)?;
                tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES(?1,?2)", rusqlite::params![operation_id, serde_json::to_string(&operation).map_err(ApiError::json)?]).map_err(|error| ApiError::stored(error.into()))?;
                let mut actor = AgentRepository::get(tx, &AgentRecord::from_value(snapshot.agent.record.clone()).map_err(|_| ApiError::internal())?.id).map_err(ApiError::stored)?
                    .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent"))?.value();
                actor["workspaceOperation"] = json!("restore");
                actor["workspaceReservationId"] = json!(operation_id);
                AgentRepository::put(tx, &AgentRecord::from_value(actor).map_err(|_| ApiError::internal())?).map_err(ApiError::stored)?;
                Ok(operation)
            }).await?
    };
    if operation_state.phase == OperationPhase::Completed {
        if current_tree != target_tree {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Files changed after the completed restore. Preview again",
            ));
        }
        return Ok(Json(match &operation_state.result {
            Field::Missing => json!({"status":"restored","checkpoint":checkpoint.id}),
            Field::Null => Value::Null,
            Field::Value(value) => value.clone(),
        }));
    }
    let scope = match port.ensure_session(&account).await {
        Ok(scope) => scope,
        Err(_) => {
            settle_restore_failure(
                &store,
                &operation_id,
                &agent_id,
                true,
                "Provider session outcome is unknown",
            )
            .await?;
            return Err(ApiError::dependency("provider-session/account-session"));
        }
    };
    // The adapter of the agent's provider builds the new thread request.
    let ThreadRequest { method, params } =
        adapter(provider).edit_thread_request(&snapshot.agent.record, &checkpoint.record);
    let provider_operation = format!("{operation_id}:provider");
    let receipt = {
        let port = port.clone();
        let account = account.clone();
        let expected_scope = scope.clone();
        let provider_operation = provider_operation.clone();
        let actor = studio_runtime_services::account_worker::retained::RequestActor {
            agent: agent_id.clone(),
            epoch: snapshot.agent.record["epoch"].as_i64().unwrap_or_default(),
        };
        let params = params.clone();
        port.provider_request(
            studio_runtime_services::account_session_runtime::ProviderRequest {
                root_request_id: operation_state.extensions().get("rootRequestId").and_then(Value::as_str).or_else(|| operation_state.extensions().get("providerAdmission").map(|_| "")).ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "The restore request identity is unavailable; inspect workspace recovery"))?.to_owned(),
                admitted_request: operation_state.extensions().get("providerAdmission").map(Deserialize::deserialize).transpose().map_err(ApiError::json)?,
                account,
                operation: provider_operation.clone(),
                actor: Some(actor),
                expected_scope: Some(expected_scope),
                deleted_actor_session: None,
                method,
                params,
                deadline: Instant::now() + Duration::from_secs(60),
            },
        )
        .await
        .map_err(|error| match error {
            studio_runtime_services::account_session_runtime::AccountSessionError::Expired => (ApiError::new(StatusCode::GONE, error.to_string()), false),
            studio_runtime_services::account_session_runtime::AccountSessionError::Requires(
                name,
            ) => (ApiError::dependency(name), false),
            studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                message,
            ) => (ApiError::new(StatusCode::BAD_REQUEST, message), false),
            studio_runtime_services::account_session_runtime::AccountSessionError::InvalidResponse(_) | studio_runtime_services::account_session_runtime::AccountSessionError::Persistence | studio_runtime_services::account_session_runtime::AccountSessionError::Unknown => (
                ApiError::new(
                    StatusCode::CONFLICT,
                    "Native thread result is unknown; inspect workspace recovery before retrying",
                ),
                true,
            ),
        })
    };
    let receipt = match receipt {
        Ok(receipt) => receipt,
        Err((error, unknown)) => {
            settle_restore_failure(
                &store,
                &operation_id,
                &agent_id,
                unknown,
                &format!("{error:?}"),
            )
            .await?;
            return Err(error);
        }
    };
    if receipt.operation != provider_operation
        || receipt.scope != scope
        || receipt.certainty()
            != studio_runtime_services::account_session_runtime::RequestCertainty::Written
    {
        let unknown = receipt.certainty()
            == studio_runtime_services::account_session_runtime::RequestCertainty::Unknown;
        settle_restore_failure(
            &store,
            &operation_id,
            &agent_id,
            unknown,
            "Provider receipt was not a known written result",
        )
        .await?;
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Native thread result is unknown; inspect workspace recovery before retrying",
        ));
    }
    let response = match receipt.into_response() {
        Some(studio_runtime_services::account_session_runtime::ProviderResponse::Reply(value)) => {
            value
        }
        Some(studio_runtime_services::account_session_runtime::ProviderResponse::Rejected(_)) => {
            settle_restore_failure(
                &store,
                &operation_id,
                &agent_id,
                false,
                "Provider rejected checkpoint restore",
            )
            .await?;
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Provider rejected checkpoint restore",
            ));
        }
        None => {
            settle_restore_failure(
                &store,
                &operation_id,
                &agent_id,
                true,
                "Provider result was missing",
            )
            .await?;
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Native thread result is unknown; inspect workspace recovery before retrying",
            ));
        }
    };
    if response
        .get("thread")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        settle_restore_failure(
            &store,
            &operation_id,
            &agent_id,
            true,
            "Native workspace response has no thread id",
        )
        .await?;
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "The native workspace response has no thread id",
        ));
    }
    let store_provider = store.clone();
    let provider_saved = response.clone();
    let operation_for_save = operation_id.clone();
    store_provider
        .transaction_with_error(move |tx| {
            let mut op = WorkspaceOperationsRepository::operation(tx, &operation_for_save)
                .map_err(ApiError::stored)?
                .ok_or_else(ApiError::internal)?;
            if matches!(
                op.phase,
                OperationPhase::ProviderPending | OperationPhase::RecoveryRequired
            ) {
                op.provider = Field::Value(
                    provider_saved
                        .as_object()
                        .cloned()
                        .ok_or_else(ApiError::internal)?,
                );
                op.phase = OperationPhase::ProviderReady;
                WorkspaceOperationsRepository::put_operation(tx, &op).map_err(ApiError::stored)?;
            }
            Ok::<_, ApiError>(())
        })
        .await?;
    let store_phase = store.clone();
    let op_phase = operation_id.clone();
    store_phase
        .transaction_with_error(move |tx| {
            let mut op = WorkspaceOperationsRepository::operation(tx, &op_phase)
                .map_err(ApiError::stored)?
                .ok_or_else(ApiError::internal)?;
            if matches!(
                op.phase,
                OperationPhase::ProviderReady | OperationPhase::RecoveryRequired
            ) {
                op.phase = OperationPhase::LocalMutation;
                WorkspaceOperationsRepository::put_operation(tx, &op).map_err(ApiError::stored)?;
            }
            Ok::<_, ApiError>(())
        })
        .await?;
    let mut tree_now = match snapshot_tree(store.clone(), &cwd).await {
        Ok(tree) => tree,
        Err(error) => {
            settle_restore_failure(
                &store,
                &operation_id,
                &agent_id,
                true,
                "Checkpoint restore snapshot failed",
            )
            .await?;
            return Err(error);
        }
    };
    if tree_now != target_tree {
        if tree_now != expected_tree {
            settle_restore_failure(
                &store,
                &operation_id,
                &agent_id,
                true,
                "Workspace tree changed during restore",
            )
            .await?;
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Workspace recovery is required before retrying restore",
            ));
        }
        // The snapshot of the current files always comes first (edit(turn), step 2).
        let snapshot_store = store.clone();
        let snapshot_agent = agent_id.clone();
        let snapshot_operation = operation_id.clone();
        let snapshot_expected = expected_tree.clone();
        let restore_store = store.clone();
        let restore_cwd = cwd.clone();
        let restore_expected = expected_tree.clone();
        let restore_target = target_tree.clone();
        if let Err(error) = edit_turn::snapshot_then_restore(
            || async move {
                checkpoints::capture_before_restore(
                    snapshot_store,
                    snapshot_agent,
                    snapshot_operation,
                    snapshot_expected,
                )
                .await
                .map(|_| ())
            },
            || async move {
                studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-http/routes/checkpoints/restore-files",
                    move || {
                        checkpoints::restore_files(
                            &restore_store,
                            &restore_cwd,
                            &restore_expected,
                            &restore_target,
                        )
                    },
                )
                .await
                .map_err(|_| CheckpointsError::Worker)?
            },
        )
        .await
        {
            settle_restore_failure(&store, &operation_id, &agent_id, true, &error.to_string())
                .await?;
            return Err(map_error(error));
        }
        tree_now = match snapshot_tree(store.clone(), &cwd).await {
            Ok(tree) => tree,
            Err(error) => {
                settle_restore_failure(
                    &store,
                    &operation_id,
                    &agent_id,
                    true,
                    "Checkpoint restore verification failed",
                )
                .await?;
                return Err(error);
            }
        };
    }
    if tree_now != target_tree {
        settle_restore_failure(
            &store,
            &operation_id,
            &agent_id,
            true,
            "Workspace tree did not reach the target checkpoint",
        )
        .await?;
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Workspace recovery is required before retrying restore",
        ));
    }
    let result = json!({"status":"restored","checkpoint":checkpoint.id});
    let final_store = store.clone();
    let final_op_id = operation_id.clone();
    let final_agent_id = agent_id.clone();
    let final_checkpoint = checkpoint.record.clone();
    let final_response = response.clone();
    let saved_result = result.clone();
    final_store
        .transaction_with_error(move |tx| {
            let mut op = WorkspaceOperationsRepository::operation(tx, &final_op_id)
                .map_err(ApiError::stored)?
                .ok_or_else(ApiError::internal)?;
            let mut agent = AgentRepository::get(
                tx,
                &AgentRecord::from_value(snapshot.agent.record.clone())
                    .map_err(|_| ApiError::internal())?
                    .id,
            )
            .map_err(ApiError::stored)?
            .ok_or_else(ApiError::internal)?
            .value();
            if agent.get("workspaceReservationId").and_then(Value::as_str)
                != Some(final_op_id.as_str())
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "Workspace source changed before restore completion",
                ));
            }
            agent["threadId"] = final_response["thread"]["id"].clone();
            agent["sandbox"] = final_response
                .get("sandbox")
                .cloned()
                .unwrap_or(Value::Null);
            agent["approvalPolicy"] = final_response
                .get("approvalPolicy")
                .cloned()
                .unwrap_or(Value::Null);
            agent["profile"] = final_response
                .get("activePermissionProfile")
                .cloned()
                .unwrap_or(Value::Null);
            agent["turnId"] = Value::Null;
            agent["inFlight"] = json!(false);
            agent["status"] = json!("paused");
            agent["autoWake"] = json!(false);
            agent["workspaceOperation"] = Value::Null;
            agent
                .as_object_mut()
                .map(|object| object.remove("workspaceReservationId"));
            agent["restoredCheckpoint"] = json!(final_checkpoint["id"]);
            agent["lastCompletedTurn"] = final_checkpoint
                .get("turnId")
                .cloned()
                .unwrap_or(Value::Null);
            agent["checkpointHistoryHead"] = final_checkpoint["id"].clone();
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(agent.clone()).map_err(|_| ApiError::internal())?,
            )
            .map_err(ApiError::stored)?;
            let fields = [
                "threadId",
                "sandbox",
                "approvalPolicy",
                "profile",
                "turnId",
                "inFlight",
                "status",
                "autoWake",
                "workspaceOperation",
                "workspaceReservationId",
                "restoredCheckpoint",
                "lastCompletedTurn",
                "checkpointHistoryHead",
            ]
            .map(|key| (key, agent.get(key).cloned().unwrap_or(Value::Null)));
            studio_runtime_store::EventWriteRepository::set_agent_sync_values(
                tx,
                &final_agent_id,
                &fields,
            )
            .map_err(ApiError::stored)?;
            tx.execute(
                "UPDATE runtime_events SET status='cancelled' WHERE agent=?1 AND status='pending'",
                [&final_agent_id],
            )
            .map_err(|error| ApiError::stored(error.into()))?;
            let visible_ids = checkpoint_history_ids(tx, &final_checkpoint)?;
            // Items after the turn are hidden, never deleted.
            edit_turn::set_item_visibility(
                tx,
                &final_agent_id,
                &visible_ids,
                &final_checkpoint["id"],
            )
            .map_err(|error| match error {
                edit_turn::EditTurnError::Store(error) => ApiError::stored(error.into()),
                edit_turn::EditTurnError::Record(_) => ApiError::internal(),
            })?;
            let label = final_checkpoint
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("Checkpoint");
            let system_text = format!("Restored workspace and conversation to {label}");
            let item_id = format!(
                "{}:restore:{}",
                final_agent_id,
                final_checkpoint["id"].as_str().unwrap_or_default()
            );
            TranscriptRepository::put_item(
                tx,
                &item_id,
                &final_agent_id,
                &json!({
                    "id":item_id,"agent":final_agent_id,"role":"system","kind":"system",
                    "title":"Checkpoint","text":system_text
                }),
                now_seconds(),
                &system_text,
                true,
            )
            .map_err(|_| ApiError::internal())?;
            op.phase = OperationPhase::Completed;
            op.result = Field::Value(saved_result);
            op.set_extension("updated", json!(now_seconds()))
                .map_err(|_| ApiError::internal())?;
            WorkspaceOperationsRepository::put_operation(tx, &op).map_err(ApiError::stored)?;
            Ok::<_, ApiError>(())
        })
        .await?;
    Ok(Json(result))
}

async fn settle_restore_failure(
    store: &studio_runtime_store::RuntimeStore,
    operation_id: &str,
    agent_id: &str,
    unknown: bool,
    message: &str,
) -> Result<(), ApiError> {
    let operation_id = operation_id.to_owned();
    let agent_id = agent_id.to_owned();
    let message = message.to_owned();
    store
        .transaction_with_error(move |tx| {
            let mut operation = WorkspaceOperationsRepository::operation(tx, &operation_id)
                .map_err(ApiError::stored)?
                .ok_or_else(ApiError::internal)?;
            if operation.phase == OperationPhase::Completed {
                return Ok(());
            }
            let owner = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))
                .map_err(ApiError::stored)?
                .ok_or_else(ApiError::internal)?;
            if owner
                .extra
                .get("workspaceReservationId")
                .and_then(Value::as_str)
                != Some(operation_id.as_str())
                || operation
                    .source
                    .value()
                    .map(|source| Value::Object(source.clone()))
                    .as_ref()
                    != Some(&CheckpointsRepository::workspace_source(&owner.value()))
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "Restore reservation changed",
                ));
            }
            operation.phase = if unknown {
                OperationPhase::RecoveryRequired
            } else {
                OperationPhase::Failed
            };
            operation
                .set_extension("error", json!(message))
                .map_err(|_| ApiError::internal())?;
            operation
                .set_extension("updated", json!(now_seconds()))
                .map_err(|_| ApiError::internal())?;
            WorkspaceOperationsRepository::put_operation(tx, &operation)
                .map_err(ApiError::stored)?;
            if let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))
                .map_err(ApiError::stored)?
            {
                if unknown {
                    agent
                        .extra
                        .insert("workspaceOperation".into(), json!("restore_recovery"));
                    agent.set_status(studio_runtime_store::AgentStatus::Interrupted);
                    agent.auto_wake = Some(false);
                    agent.extra.insert("error".into(), json!(message));
                } else if agent
                    .extra
                    .get("workspaceReservationId")
                    .and_then(Value::as_str)
                    == Some(operation_id.as_str())
                {
                    agent.extra.remove("workspaceReservationId");
                    agent.extra.insert("workspaceOperation".into(), Value::Null);
                }
                AgentRepository::put(tx, &agent).map_err(ApiError::stored)?;
            }
            Ok::<_, ApiError>(())
        })
        .await?;
    Ok(())
}

async fn snapshot_tree(
    store: std::sync::Arc<studio_runtime_store::RuntimeStore>,
    cwd: &str,
) -> Result<String, ApiError> {
    let cwd = cwd.to_owned();
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/checkpoints/snapshot-tree",
        move || checkpoints::snapshot_tree(&store, &cwd).map_err(map_error),
    )
    .await
    .map_err(|_| ApiError::internal())?
}

fn checkpoint_history_ids(
    tx: &rusqlite::Transaction<'_>,
    checkpoint: &Value,
) -> Result<std::collections::HashSet<String>, ApiError> {
    let agent_id = checkpoint
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(ApiError::internal)?;
    let mut visible = std::collections::HashSet::new();
    let mut seen = std::collections::HashSet::new();
    let mut current = checkpoint.clone();
    loop {
        let id = current
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(ApiError::internal)?;
        if !seen.insert(id.to_owned()) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Checkpoint history lineage contains a cycle",
            ));
        }
        if let Some(items) = current
            .get("items")
            .and_then(Value::as_array)
            .or_else(|| current.get("historyDelta").and_then(Value::as_array))
        {
            for item in items {
                visible.insert(item.as_str().ok_or_else(ApiError::internal)?.to_owned());
            }
        } else {
            let created = current
                .get("created")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let mut statement = tx
                .prepare("SELECT id FROM runtime_items WHERE agent=?1 AND created<=?2")
                .map_err(|error| ApiError::stored(error.into()))?;
            let ids = statement
                .query_map(rusqlite::params![agent_id, created], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|error| ApiError::stored(error.into()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| ApiError::stored(error.into()))?;
            visible.extend(ids);
        }
        let Some(parent_id) = current.get("historyParent").and_then(Value::as_str) else {
            break;
        };
        let parent = CheckpointsRepository::checkpoint(tx, parent_id)
            .map_err(ApiError::stored)?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "Checkpoint history parent is missing",
                )
            })?;
        current = parent.record;
    }
    Ok(visible)
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn map_error(error: CheckpointsError) -> ApiError {
    match error {
        CheckpointsError::Worker => ApiError::internal(),
        CheckpointsError::RestoreWorkspaceRequired => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Restore is available only in an isolated worker worktree",
        ),
        CheckpointsError::UnknownAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent")
        }
        CheckpointsError::UnknownCheckpoint => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown checkpoint")
        }
        CheckpointsError::WrongAgent => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Checkpoint belongs to another agent",
        ),
        CheckpointsError::DeletedAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "This agent was deleted")
        }
        CheckpointsError::ExecutionWorkspace => {
            ApiError::new(StatusCode::BAD_REQUEST, "This legacy workspace is finished")
        }
        CheckpointsError::GitToolsUnavailable => {
            ApiError::new(StatusCode::BAD_REQUEST, "This legacy workspace is finished")
        }
        CheckpointsError::ActivityChanged => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Workspace activity changed before checkpoint capture",
        ),
        CheckpointsError::SettlementFailed { .. } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Checkpoint capture failed and workspace reservation cleanup failed",
        ),
        CheckpointsError::RecoveryRequired => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Workspace recovery is required before using this workspace",
        ),
        CheckpointsError::AgentBusy => {
            ApiError::new(StatusCode::BAD_REQUEST, "An agent is using this workspace")
        }
        CheckpointsError::MonitorBusy => {
            ApiError::new(StatusCode::BAD_REQUEST, "A monitor is using this workspace")
        }
        CheckpointsError::TaskBusy => {
            ApiError::new(StatusCode::BAD_REQUEST, "A command or tool is still active")
        }
        CheckpointsError::GitFailed => {
            ApiError::new(StatusCode::BAD_REQUEST, "Git operation failed")
        }
        CheckpointsError::GitTimeout => {
            ApiError::new(StatusCode::BAD_REQUEST, "Git operation timed out")
        }
        CheckpointsError::WorkspaceUnavailable => {
            ApiError::new(StatusCode::BAD_REQUEST, "The workspace cannot be accessed")
        }
        CheckpointsError::InvalidStoredObject { object, id } => {
            ApiError::invalid_stored(object, &id)
        }
        CheckpointsError::Store(StoreError::SchemaMismatch(message))
            if message.starts_with("Invalid stored object ") =>
        {
            let id = message
                .strip_prefix("Invalid stored object ")
                .unwrap_or(&message);
            ApiError::invalid_stored("checkpoint", id)
        }
        CheckpointsError::Store(_) => ApiError::internal(),
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "checkpoints",
        routes: Some(routes),
        dependencies: &[
        "provider-session/thread-fork",
        ],
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use studio_runtime_store::RuntimeStore;

    #[tokio::test]
    async fn unknown_restore_sets_one_interrupted_status() {
        let root = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        store.transaction(|tx| {
            let agent = AgentRecord::from_value(json!({"accountKey":"default","id":"agent","rootId":"agent","epoch":0,"status":"running","workspaceReservationId":"operation","provider":"codex"}))?;
            AgentRepository::put(tx, &agent)?;
            let operation = json!({"id":"operation","kind":"restore","agent":"agent","phase":"provider_pending","source":CheckpointsRepository::workspace_source(&agent.value())});
            tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES('operation',?1)", [operation.to_string()])?;
            Ok::<_, studio_runtime_store::StoreError>(())
        }).await.expect("seed");
        settle_restore_failure(&store, "operation", "agent", true, "unknown")
            .await
            .expect("settle");
        let db = store.test_reader().expect("reader");
        let (status, count): (String, i64) = db.query_row("SELECT json_extract(record,'$.status'), (SELECT count(*) FROM json_each(record) WHERE key='status') FROM runtime_agents WHERE id='agent'", [], |row| Ok((row.get(0)?, row.get(1)?))).expect("status");
        assert_eq!((status.as_str(), count), ("interrupted", 1));
    }
}
