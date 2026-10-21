//! Capture and preview local Git checkpoints; provider-dependent effects stay gated.

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use serde_json::json;
use std::time::{Duration, Instant};
use studio_runtime_services::checkpoints::{self, CheckpointsError};
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
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/checkpoints",
        move || {
            let context = checkpoints::preview_context(&store, &body.agent, &body.checkpoint)
                .map_err(map_error)?;
            let value = checkpoints::preview(&store, context).map_err(map_error)?;
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
    let port = state
        .account_session
        .clone()
        .ok_or_else(|| ApiError::dependency(THREAD_FORK_DEPENDENCY))?;
    let (snapshot, checkpoint, operation_id, signature, current_tree) =
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/checkpoints",
            {
                let store = store.clone();
                let agent_id = body.agent.clone();
                let expected_tree = body.expected_tree.clone();
                move || {
                    let snapshot = checkpoints::capture_context(&store, &agent_id).map_err(map_error)?;
                    checkpoints::ensure_agent_available(&snapshot.agent).map_err(map_error)?;
                    if snapshot.agent.record.get("worktreeReady").and_then(Value::as_bool) != Some(true) {
                        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Restore is available only in an isolated worker worktree"));
                    }
                    let context = checkpoints::preview_context(&store, &agent_id, &checkpoint_id).map_err(map_error)?;
                    if context.checkpoint.record.get("agent").and_then(Value::as_str) != Some(agent_id.as_str()) {
                        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Checkpoint belongs to another agent"));
                    }
                    if expected_tree.is_empty() {
                        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Preview this checkpoint before restore"));
                    }
                    let current_tree = checkpoints::snapshot_tree(&store, snapshot.agent.record["cwd"].as_str().ok_or_else(ApiError::internal)?).map_err(map_error)?;
                    let op_id = format!("restore:{agent_id}:{checkpoint_id}:{}", &studio_runtime_store::sha256_hex(expected_tree.as_bytes())[..32]);
                    let own_reservation = (snapshot.agent.record.get("workspaceReservationId").and_then(Value::as_str) == Some(op_id.as_str())).then_some(op_id.as_str());
                    checkpoints::ensure_workspace_idle(&snapshot, own_reservation).map_err(map_error)?;
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
                    if old.get("signature").and_then(Value::as_str) != Some(signature.as_str()) {
                        return Err(ApiError::new(StatusCode::CONFLICT, "This restore request has different content"));
                    }
                    if old.get("source") != Some(&source) {
                        return Err(ApiError::new(StatusCode::CONFLICT, "Workspace operation source changed. Inspect the operation before retrying"));
                    }
                    if old["phase"] == "completed" { return Ok(old); }
                    if !matches!(old["phase"].as_str(), Some("provider_pending" | "provider_ready" | "local_mutation" | "recovery_required")) {
                        old["phase"] = json!("provider_pending");
                        tx.execute("UPDATE runtime_workspace_operations SET record=?1 WHERE id=?2", rusqlite::params![old.to_string(), operation_id]).map_err(|error| ApiError::stored(error.into()))?;
                    }
                    return Ok(old);
                }
                if current_tree != expected_tree {
                    return Err(ApiError::new(StatusCode::CONFLICT, "Files changed after the preview. Preview again"));
                }
                let operation = json!({
                    "id":operation_id,"kind":"restore","agent":agent_id,"checkpoint":checkpoint.id,
                    "expectedTree":expected_tree,"signature":signature,"source":source,
                    "phase":"provider_pending","created":now_seconds()
                });
                tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES(?1,?2)", rusqlite::params![operation_id, operation.to_string()]).map_err(|error| ApiError::stored(error.into()))?;
                let mut actor = AgentRepository::get(tx, &AgentRecord::from_value(snapshot.agent.record.clone()).map_err(|_| ApiError::internal())?.id).map_err(ApiError::stored)?
                    .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent"))?.value();
                actor["workspaceOperation"] = json!("restore");
                actor["workspaceReservationId"] = json!(operation_id);
                AgentRepository::put(tx, &AgentRecord::from_value(actor).map_err(|_| ApiError::internal())?).map_err(ApiError::stored)?;
                Ok(operation)
            }).await?
    };
    if operation_state["phase"] == "completed" {
        if current_tree != target_tree {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Files changed after the completed restore. Preview again",
            ));
        }
        return Ok(Json(operation_state.get("result").cloned().unwrap_or(
            json!({"status":"restored","checkpoint":checkpoint.id}),
        )));
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
    let method = if checkpoint
        .record
        .get("turnId")
        .and_then(Value::as_str)
        .is_some()
    {
        studio_runtime_providers::Method::ThreadFork
    } else {
        studio_runtime_providers::Method::ThreadStart
    };
    // Python `new_thread_params` always supplies config and serviceTier, and
    // omits `model` when the saved model is unset. A null model is observably
    // different to providers (and to the fake provider's documented fallback).
    let mut params = json!({
        "cwd":cwd,
        "serviceTier": if snapshot.agent.record.get("fastMode").and_then(Value::as_bool) == Some(true) { "priority" } else { "default" },
        "config": snapshot.agent.record.get("threadConfig").cloned().unwrap_or_else(|| json!({})),
    });
    if let Some(model) = snapshot
        .agent
        .record
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
    {
        params["model"] = json!(model);
    }
    if method == studio_runtime_providers::Method::ThreadFork {
        params["threadId"] = checkpoint
            .record
            .get("threadId")
            .cloned()
            .unwrap_or(Value::Null);
        params["lastTurnId"] = checkpoint
            .record
            .get("turnId")
            .cloned()
            .unwrap_or(Value::Null);
        params["config"] = snapshot
            .agent
            .record
            .get("threadConfig")
            .cloned()
            .unwrap_or_else(|| json!({}));
        for key in ["approvalPolicy", "sandbox"] {
            if let Some(value) = snapshot.agent.record.get(key) {
                params[key] = value.clone();
            }
        }
    }
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
            studio_runtime_services::account_session_runtime::AccountSessionError::Requires(
                name,
            ) => (ApiError::dependency(name), false),
            studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                message,
            ) => (ApiError::new(StatusCode::BAD_REQUEST, message), false),
            _ => (
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
        || receipt.certainty
            != studio_runtime_services::account_session_runtime::RequestCertainty::Written
    {
        let unknown = receipt.certainty
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
    let response = match receipt.result {
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
            if op["phase"] == "provider_pending" || op["phase"] == "recovery_required" {
                op["provider"] = provider_saved;
                op["phase"] = json!("provider_ready");
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
                op["phase"].as_str(),
                Some("provider_ready" | "recovery_required")
            ) {
                op["phase"] = json!("local_mutation");
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
        if let Err(error) = checkpoints::capture_before_restore(
            store.clone(),
            agent_id.clone(),
            operation_id.clone(),
            expected_tree.clone(),
        )
        .await
        {
            settle_restore_failure(&store, &operation_id, &agent_id, true, &error.to_string())
                .await?;
            return Err(map_error(error));
        }
        let restore_store = store.clone();
        let restore_cwd = cwd.clone();
        let restore_expected = expected_tree.clone();
        let restore_target = target_tree.clone();
        let restore = studio_runtime_process::blocking_pool::spawn_blocking(
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
        .map_err(|_| ApiError::internal())?;
        if let Err(error) = restore {
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
                &AgentRecord::from_value(agent).map_err(|_| ApiError::internal())?,
            )
            .map_err(ApiError::stored)?;
            tx.execute(
                "UPDATE runtime_events SET status='cancelled' WHERE agent=?1 AND status='pending'",
                [&final_agent_id],
            )
            .map_err(|error| ApiError::stored(error.into()))?;
            let visible_ids = checkpoint_history_ids(tx, &final_checkpoint)?;
            let mut statement = tx
                .prepare(
                    "SELECT id,record,created FROM runtime_items WHERE agent=?1 ORDER BY rowid",
                )
                .map_err(|error| ApiError::stored(error.into()))?;
            let rows = statement
                .query_map([&final_agent_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                })
                .map_err(|error| ApiError::stored(error.into()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| ApiError::stored(error.into()))?;
            drop(statement);
            for (item_id, raw, _created) in rows {
                let mut item: Value =
                    serde_json::from_str(&raw).map_err(|_| ApiError::internal())?;
                let visible = visible_ids.contains(&item_id);
                let was_visible = item.get("afterRestore").is_none();
                if visible == was_visible {
                    continue;
                }
                if visible {
                    if let Some(object) = item.as_object_mut() {
                        object.remove("afterRestore");
                    }
                } else {
                    item["afterRestore"] = final_checkpoint["id"].clone();
                }
                tx.execute(
                    "UPDATE runtime_items SET record=?1 WHERE id=?2 AND agent=?3",
                    rusqlite::params![item.to_string(), item_id, final_agent_id],
                )
                .map_err(|error| ApiError::stored(error.into()))?;
            }
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
            op["phase"] = json!("completed");
            op["result"] = saved_result;
            op["updated"] = json!(now_seconds());
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
            operation["phase"] = json!(if unknown {
                "recovery_required"
            } else {
                "failed"
            });
            operation["error"] = json!(message.clone());
            operation["updated"] = json!(now_seconds());
            WorkspaceOperationsRepository::put_operation(tx, &operation)
                .map_err(ApiError::stored)?;
            if let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))
                .map_err(ApiError::stored)?
            {
                if unknown {
                    agent
                        .extra
                        .insert("workspaceOperation".into(), json!("restore_recovery"));
                    agent.extra.insert("status".into(), json!("interrupted"));
                    agent.extra.insert("autoWake".into(), json!(false));
                    agent.extra.insert("error".into(), json!(message.clone()));
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
        CheckpointsError::ExecutionWorkspace => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Studio Git checkpoints are unavailable for image workspaces",
        ),
        CheckpointsError::GitToolsUnavailable => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Studio Git tools are unavailable for image workspaces",
        ),
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
