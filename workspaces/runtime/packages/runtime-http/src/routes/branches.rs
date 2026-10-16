//! Durable provider thread branching over the retained account session.
//!
//! The committed `BranchResponse` contract omits Python's `forkedFrom` and
//! `sourceMessage` response fields. They remain persisted on the child agent;
//! the web client does not read them from the branch response.

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use axum::{Json, extract::State, http::StatusCode};
use rusqlite::OptionalExtension;
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use studio_runtime_providers::Method;
use studio_runtime_services::account_session_runtime::{
    AccountSessionError, AccountSessionPort, ProviderRequest, ProviderResponse, RequestCertainty,
};
use studio_runtime_services::account_worker::retained::RequestActor;
use studio_runtime_store::{
    AgentId, AgentRepository, CheckpointsRepository, RuntimeStore, TranscriptRepository,
    TranscriptSearchKind, WorkspaceFilesRepository, sha256_hex,
};

use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/branch",
        OperationStatus::Implemented,
        branch,
    )])
}

async fn branch(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::BranchRequest>,
) -> Result<Json<crate::generated::BranchResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let port = state.account_session.clone().ok_or_else(unavailable)?;
    let agent_id = body.agent.clone();
    let account_store = store.clone();
    let account = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/branches",
        move || {
            let db = account_store.read().map_err(ApiError::stored)?;
            let agent = AgentRepository::get(&db, &AgentId::from(agent_id.as_str()))
                .map_err(ApiError::stored)?
                .ok_or_else(|| bad_request("Unknown managed agent"))?;
            Ok::<_, ApiError>(
                agent
                    .extra
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .to_owned(),
            )
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    let operation_id = format!("branch:{}", body.id);
    let signature = sha256_hex(
        serde_json::to_string(&json!({
            "agent":body.agent,"id":body.id,"message_id":body.message_id,"before":body.before
        }))
        .map_err(|_| ApiError::internal())?
        .as_bytes(),
    );
    let (snapshot, source, selected) =
        studio_runtime_process::blocking_pool::spawn_blocking("runtime-http/routes/branches", {
            let store = store.clone();
            let port = port.clone();
            let scope_account = account.clone();
            let agent_id = body.agent.clone();
            let message_id = body.message_id.clone();
            move || {
                let snapshot = {
                    let db = store.read().map_err(ApiError::stored)?;
                    CheckpointsRepository::workspace_snapshot(&db, &agent_id)
                        .map_err(ApiError::stored)?
                        .ok_or_else(|| bad_request("Unknown managed agent"))?
                };
                let source = snapshot.agent.record.clone();
                let selected = {
                    let db = store.read().map_err(ApiError::stored)?;
                    TranscriptRepository::resolve_item(&db, &agent_id, &message_id)
                        .map_err(|_| ApiError::internal())?
                        .ok_or_else(|| bad_request("Unknown message in this conversation"))?
                };
                if selected.record.get("afterRestore").is_some()
                    || selected
                        .record
                        .get("turnId")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                {
                    return Err(bad_request(
                        "This message has no active native turn reference",
                    ));
                }
                if source
                    .get("threadId")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                    || source.get("inFlight").and_then(Value::as_bool) == Some(true)
                    || source.get("turnId").and_then(Value::as_str)
                        == selected.record.get("turnId").and_then(Value::as_str)
                {
                    return Err(bad_request("Wait for this turn to finish before branching"));
                }
                // Match Python's read-only request validation before requiring a live native
                // consumer. Invalid branch inputs must not depend on provider availability.
                let _scope = port.current_scope(&scope_account).ok_or_else(unavailable)?;
                Ok::<_, ApiError>((snapshot, source, selected))
            }
        })
        .await
        .map_err(|_| ApiError::internal())??;
    let operation = {
        let snapshot = snapshot.clone();
        let source = source.clone();
        let selected = selected.clone();
        let agent_id = body.agent.clone();
        let message_id = body.message_id.clone();
        let operation_id = operation_id.clone();
        let signature = signature.clone();
        store
            .transaction_with(move |tx| {
                if !CheckpointsRepository::workspace_unchanged(tx, &snapshot)? {
                    return Err(studio_runtime_store::StoreError::SchemaMismatch(
                        "branch workspace changed".into(),
                    ));
                }
                if let Some(prior) = tx
                    .query_row(
                        "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                        [&operation_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                {
                    let mut prior: Value = serde_json::from_str(&prior)?;
                    if prior.get("signature").and_then(Value::as_str) != Some(signature.as_str()) {
                        return Err(studio_runtime_store::StoreError::SchemaMismatch(
                            "branch request id has different content".into(),
                        ));
                    }
                    match prior.get("phase").and_then(Value::as_str) {
                        Some("completed") => {
                            return Ok(Some(prior));
                        }
                        Some("provider_ready") => {}
                        Some("failed") => {
                            prior["phase"] = json!("provider_pending");
                            if let Some(value) = prior.as_object_mut() {
                                value.remove("provider");
                                value.remove("result");
                                value.remove("error");
                            }
                            tx.execute(
                                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                                params![operation_id, prior.to_string()],
                            )?;
                        }
                        Some("provider_pending" | "recovery_required") => {
                            // Re-submit the same broker operation identity
                            // below. The retained broker returns its saved
                            // receipt or Unknown and never sends a duplicate.
                            let mut actor =
                                AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                                    .ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;
                            if (actor
                                .extra
                                .get("workspaceOperation")
                                .is_some_and(|value| !value.is_null() && value != "")
                                && actor
                                    .extra
                                    .get("workspaceReservationId")
                                    .and_then(Value::as_str)
                                    != Some(operation_id.as_str()))
                                || snapshot.agents.iter().any(|entry| {
                                    entry.id != agent_id
                                        && entry.record.get("cwd") == source.get("cwd")
                                        && entry
                                            .record
                                            .get("workspaceOperation")
                                            .is_some_and(|value| !value.is_null() && value != "")
                                })
                            {
                                return Err(studio_runtime_store::StoreError::SchemaMismatch(
                                    "workspace is busy".into(),
                                ));
                            }
                            actor
                                .extra
                                .insert("workspaceOperation".into(), json!("branch"));
                            actor
                                .extra
                                .insert("workspaceReservationId".into(), json!(operation_id));
                            AgentRepository::put(tx, &actor)?;
                            prior["reserved"] = json!(true);
                            tx.execute(
                                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                                params![operation_id, prior.to_string()],
                            )?;
                            return Ok(Some(prior));
                        }
                        _ => {
                            return Err(studio_runtime_store::StoreError::SchemaMismatch(
                                "Workspace recovery is required before retrying this branch".into(),
                            ));
                        }
                    }
                }
                if (source
                    .get("workspaceOperation")
                    .is_some_and(|value| !value.is_null() && value != "")
                    && source.get("workspaceReservationId").and_then(Value::as_str)
                        != Some(operation_id.as_str()))
                    || snapshot.agents.iter().any(|entry| {
                        entry.id != agent_id
                            && entry.record.get("cwd") == source.get("cwd")
                            && entry
                                .record
                                .get("workspaceOperation")
                                .is_some_and(|value| !value.is_null() && value != "")
                    })
                {
                    return Err(studio_runtime_store::StoreError::SchemaMismatch(
                        "workspace is busy".into(),
                    ));
                }
                let source_stamp = CheckpointsRepository::workspace_source(&source);
                let mut record = if let Some(existing) = tx
                    .query_row(
                        "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                        [&operation_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                {
                    serde_json::from_str::<Value>(&existing)?
                } else {
                    let record = json!({
                        "id":operation_id,"kind":"branch","agent":agent_id,
                        "phase":"provider_pending","signature":signature,
                        "source":source_stamp,"message_id":message_id,
                        "turn_id":selected.record["turnId"],
                        "previousStatus":source.get("status"),
                        "previousAutoWake":source.get("autoWake"),
                        "previousWorkspaceOperation":source.get("workspaceOperation"),
                        "created":unix_time()
                    });
                    tx.execute(
                        "INSERT INTO runtime_workspace_operations(id,record) VALUES(?1,?2)",
                        params![operation_id, record.to_string()],
                    )?;
                    record
                };
                let mut actor = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;
                actor
                    .extra
                    .insert("workspaceOperation".into(), json!("branch"));
                actor
                    .extra
                    .insert("workspaceReservationId".into(), json!(operation_id));
                AgentRepository::put(tx, &actor)?;
                record["reserved"] = json!(true);
                tx.execute(
                    "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                    params![operation_id, record.to_string()],
                )?;
                Ok(Some(record))
            })
            .await
            .map_err(|error| bad_request(&error.to_string()))?
    };
    let needs_catalog = branch_needs_catalog(&store, &operation_id, &signature).await?;
    if needs_catalog {
        ensure_branch_model_catalog(&state, &account).await?;
    }
    if let Some(result) = operation
        .as_ref()
        .and_then(|op| op.get("result"))
        .filter(|_| {
            operation
                .as_ref()
                .is_some_and(|op| op["phase"] == "completed")
        })
    {
        return Ok(Json(super::generated_json::<
            crate::generated::BranchResponse,
        >(result.clone())?));
    }
    let prior_provider = operation
        .as_ref()
        .and_then(|op| op.get("provider"))
        .cloned();
    let scope = port.ensure_session(&account).await.map_err(|error| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Could not ensure account session: {error}"),
        )
    })?;
    let branch_model = if source.get("isLead").and_then(Value::as_bool) == Some(true) {
        source["model"].as_str().unwrap_or("gpt-5.6-sol")
    } else {
        "gpt-5.6-sol"
    };
    let before = body.before.unwrap_or(false);
    let selected_turn = selected.record["turnId"].as_str().map(str::to_owned);
    let fork_turn = if before {
        async {
            // Python reads the full thread history before resolving `before`.
            // Stable per-page operation IDs let the retained broker replay a known
            // page without resending an uncertain one.
            let mut cursor: Option<String> = None;
            let mut seen_cursors = HashSet::new();
            let mut turn_ids = Vec::new();
            let mut page = 0_u64;
            loop {
                let mut params = json!({
                    "threadId":source["threadId"],
                    "limit":20,
                    "sortDirection":"desc",
                    "itemsView":"full"
                });
                if let Some(cursor_value) = &cursor {
                    params["cursor"] = json!(cursor_value);
                }
                let result = provider_request(
                    Arc::clone(&port),
                    &account,
                    &scope,
                    format!("{operation_id}:turns:{page}"),
                    Some(RequestActor {
                        agent: body.agent.clone(),
                        epoch: source["epoch"].as_i64().unwrap_or(0),
                    }),
                    Method::ThreadTurnsList,
                    params,
                )
                .await?;
                let turns = result
                    .get("data")
                    .and_then(Value::as_array)
                    .ok_or_else(ApiError::internal)?;
                for turn in turns {
                    let id = turn
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(ApiError::internal)?;
                    turn_ids.push(id.to_owned());
                }
                let Some(next) = result
                    .get("nextCursor")
                    .and_then(Value::as_str)
                    .filter(|next| !next.is_empty())
                else {
                    break;
                };
                if !seen_cursors.insert(next.to_owned()) {
                    return Err(ApiError::internal());
                }
                cursor = Some(next.to_owned());
                page = page.checked_add(1).ok_or_else(ApiError::internal)?;
            }
            let selected_turn = selected_turn.as_deref().ok_or_else(ApiError::internal)?;
            let position = turn_ids
                .iter()
                .position(|turn| turn == selected_turn)
                .ok_or_else(|| bad_request("The selected turn is absent from native history"))?;
            Ok::<_, ApiError>(turn_ids.get(position + 1).cloned())
        }
        .await
    } else {
        Ok::<_, ApiError>(selected_turn)
    };
    let fork_turn = match fork_turn {
        Ok(turn) => turn,
        Err(error) => {
            settle_failure(&store, &operation_id, false, "Native turn lookup failed").await?;
            return Err(error);
        }
    };
    persist_fork_turn(&store, &operation_id, fork_turn.as_deref()).await?;
    let provider = if let Some(provider) = prior_provider {
        provider
    } else {
        let method = if fork_turn.is_some() {
            Method::ThreadFork
        } else {
            Method::ThreadStart
        };
        let mut params = json!({
            "cwd":source["cwd"],"model":branch_model,"approvalPolicy":source["approvalPolicy"],
            "sandbox":source["sandbox"],"developerInstructions":source["developerInstructions"]
        });
        if let Some(turn) = fork_turn.as_ref() {
            params["threadId"] = source["threadId"].clone();
            params["lastTurnId"] = json!(turn);
            params["config"] = source
                .get("threadConfig")
                .cloned()
                .unwrap_or_else(|| json!({}));
        }
        let request = ProviderRequest {
            account: account.clone(),
            operation: format!("{operation_id}:fork"),
            actor: Some(RequestActor {
                agent: body.agent.clone(),
                epoch: source["epoch"].as_i64().unwrap_or(0),
            }),
            expected_scope: Some(scope.clone()),
            deleted_actor_session: None,
            method,
            params,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        let result = port.provider_request(request).await;
        match result {
            Ok(receipt) if receipt.certainty == RequestCertainty::Written => match receipt.result {
                Some(ProviderResponse::Reply(value)) => value,
                Some(ProviderResponse::Rejected(error)) => {
                    settle_failure(&store, &operation_id, false, &error.to_string()).await?;
                    return Err(bad_request(&error.to_string()));
                }
                _ => {
                    settle_failure(
                        &store,
                        &operation_id,
                        true,
                        "Provider fork outcome is unknown",
                    )
                    .await?;
                    return Err(unavailable());
                }
            },
            Ok(receipt) if receipt.certainty == RequestCertainty::NotSent => {
                settle_failure(&store, &operation_id, false, "Provider fork was not sent").await?;
                return Err(unavailable());
            }
            Err(AccountSessionError::Unknown) => {
                settle_failure(
                    &store,
                    &operation_id,
                    true,
                    "Provider fork outcome is unknown",
                )
                .await?;
                return Err(unavailable());
            }
            Ok(_) => {
                settle_failure(
                    &store,
                    &operation_id,
                    true,
                    "Provider fork outcome is unknown",
                )
                .await?;
                return Err(unavailable());
            }
            Err(error) => {
                settle_failure(&store, &operation_id, false, &error.to_string()).await?;
                return Err(bad_request(&error.to_string()));
            }
        }
    };
    if provider
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .is_none_or(|thread| Some(thread) == source.get("threadId").and_then(Value::as_str))
    {
        settle_failure(
            &store,
            &operation_id,
            true,
            "Provider returned an invalid fork thread",
        )
        .await?;
        return Err(unavailable());
    }
    let persist_operation_id = operation_id.clone();
    let persist_agent_id = body.agent.clone();
    let persist_provider = provider.clone();
    let persisted = store
        .transaction_with(move |tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                [&persist_operation_id],
                |row| row.get(0),
            )?;
            let mut op: Value = serde_json::from_str(&raw)?;
            let current = AgentRepository::get(tx, &AgentId::from(persist_agent_id.as_str()))?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let current_source = CheckpointsRepository::workspace_source(&current.value());
            if current_source != op["source"]
                || current
                    .extra
                    .get("workspaceReservationId")
                    .and_then(Value::as_str)
                    != Some(persist_operation_id.as_str())
            {
                return Err(studio_runtime_store::StoreError::SchemaMismatch(
                    "branch source changed after provider hand-off".into(),
                ));
            }
            op["provider"] = persist_provider;
            op["phase"] = json!("provider_ready");
            tx.execute(
                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                params![persist_operation_id, op.to_string()],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await;
    if let Err(error) = persisted {
        settle_failure(
            &store,
            &operation_id,
            true,
            "Branch source changed after provider hand-off",
        )
        .await?;
        return Err(ApiError::stored(error));
    }
    ensure_branch_source(&store, &operation_id, &body.agent).await?;
    let child_id = deterministic_uuid(&operation_id);
    let created = super::agents_create::create_branch_draft(
        state.clone(),
        json!({
        "id":child_id,"name":format!("{} (branch)",source["name"].as_str().unwrap_or("Lead").chars().take(90).collect::<String>()),
        "account_key":account,"cwd":source["cwd"],"model":branch_model,
            "yolo_mode":source["yoloMode"],"workspace_mode":source["workspaceMode"],
            "profile_id":source["profileId"]
        }),
    )
    .await;
    let Json(created) = match created {
        Ok(created) => created,
        Err(error) => {
            release_provider_ready_reservation(&store, &operation_id, &body.agent).await?;
            return Err(error);
        }
    };
    let child_id = created.id;
    let branch_result = copy_branch_history(
        &store,
        BranchCopy {
            source: &body.agent,
            target: &child_id,
            selected: &selected,
            message_id: &body.message_id,
            fork_turn: fork_turn.as_deref(),
            before,
            provider: &provider,
            operation_id: &operation_id,
        },
        Arc::clone(&state.event_wake),
    )
    .await?;
    Ok(Json(super::generated_json::<
        crate::generated::BranchResponse,
    >(branch_result)?))
}

async fn provider_request(
    port: Arc<dyn AccountSessionPort>,
    account: &str,
    scope: &studio_runtime_services::account_session_runtime::ConnectionScope,
    operation: String,
    actor: Option<RequestActor>,
    method: Method,
    params: Value,
) -> Result<Value, ApiError> {
    let mut request = ProviderRequest {
        account: account.to_owned(),
        operation,
        actor,
        expected_scope: Some(scope.clone()),
        deleted_actor_session: None,
        method,
        params,
        deadline: Instant::now() + Duration::from_secs(30),
    };
    let initial_request = request.clone();
    let mut receipt =
        port.provider_request(initial_request)
            .await
            .map_err(|error| match error {
                AccountSessionError::Requires(name) => ApiError::dependency(name),
                AccountSessionError::Unknown => unavailable(),
                _ => bad_request(&error.to_string()),
            })?;
    if receipt.certainty == RequestCertainty::NotSent {
        // A worker can disappear between ensuring the account session and
        // submitting the request. A NotSent receipt permits retrying, but the
        // operation identity and payload remain unchanged.
        request.expected_scope = Some(port.ensure_session(account).await.map_err(|error| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Could not ensure account session: {error}"),
            )
        })?);
        receipt = port
            .provider_request(request)
            .await
            .map_err(|error| match error {
                AccountSessionError::Requires(name) => ApiError::dependency(name),
                AccountSessionError::Unknown => unavailable(),
                _ => bad_request(&error.to_string()),
            })?;
    }
    match (receipt.certainty, receipt.result) {
        (RequestCertainty::Written, Some(ProviderResponse::Reply(value))) => Ok(value),
        (RequestCertainty::Written, Some(ProviderResponse::Rejected(error))) => {
            Err(bad_request(&error.to_string()))
        }
        _ => Err(unavailable()),
    }
}

async fn settle_failure(
    store: &RuntimeStore,
    operation_id: &str,
    unknown: bool,
    error: &str,
) -> Result<(), ApiError> {
    let operation_id = operation_id.to_owned();
    let error = error.to_owned();
    store
        .transaction_with(move |tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                [operation_id.as_str()],
                |row| row.get(0),
            )?;
            let mut operation: Value = serde_json::from_str(&raw)?;
            operation["phase"] = json!(if unknown {
                "recovery_required"
            } else {
                "failed"
            });
            operation["error"] = json!(error.clone());
            tx.execute(
                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                params![operation_id.as_str(), operation.to_string()],
            )?;
            if let Some(mut actor) = AgentRepository::get(
                tx,
                &AgentId::from(operation["agent"].as_str().unwrap_or_default()),
            )? {
                if unknown {
                    actor.extra.insert("status".into(), json!("interrupted"));
                    actor.extra.insert("autoWake".into(), json!(false));
                    actor.extra.insert("error".into(), json!(error.clone()));
                } else {
                    actor.extra.remove("workspaceReservationId");
                    actor.extra.insert("workspaceOperation".into(), Value::Null);
                }
                AgentRepository::put(tx, &actor)?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .map_err(ApiError::stored)
}

async fn release_provider_ready_reservation(
    store: &RuntimeStore,
    operation_id: &str,
    agent_id: &str,
) -> Result<(), ApiError> {
    let operation_id = operation_id.to_owned();
    let agent_id = agent_id.to_owned();
    store
        .transaction_with(move |tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                [operation_id.as_str()],
                |row| row.get(0),
            )?;
            let mut operation: Value = serde_json::from_str(&raw)?;
            if operation.get("phase").and_then(Value::as_str) != Some("provider_ready")
                || operation.get("provider").is_none()
            {
                return Err(studio_runtime_store::StoreError::SchemaMismatch(
                    "branch provider result is not durably ready".into(),
                ));
            }
            operation["reserved"] = json!(false);
            tx.execute(
                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                params![operation_id.as_str(), operation.to_string()],
            )?;
            if let Some(mut actor) = AgentRepository::get(tx, &AgentId::from(agent_id))?
                && actor
                    .extra
                    .get("workspaceReservationId")
                    .and_then(Value::as_str)
                    == Some(operation_id.as_str())
            {
                actor.extra.remove("workspaceReservationId");
                actor.extra.insert("workspaceOperation".into(), Value::Null);
                AgentRepository::put(tx, &actor)?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .map_err(ApiError::stored)
}

async fn persist_fork_turn(
    store: &RuntimeStore,
    operation_id: &str,
    fork_turn: Option<&str>,
) -> Result<(), ApiError> {
    let operation_id = operation_id.to_owned();
    let fork_turn = fork_turn.map(str::to_owned);
    store
        .transaction_with(move |tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                [operation_id.as_str()],
                |row| row.get(0),
            )?;
            let mut operation: Value = serde_json::from_str(&raw)?;
            operation["forkTurnId"] = fork_turn.map_or(Value::Null, |turn| json!(turn));
            tx.execute(
                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                params![operation_id.as_str(), operation.to_string()],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .map_err(ApiError::stored)
}

async fn ensure_branch_model_catalog(state: &HttpState, account_key: &str) -> Result<(), ApiError> {
    let policy = state
        .account_policy
        .clone()
        .ok_or_else(ApiError::internal)?;
    let driver = state.account_driver.clone();
    let snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/branches",
        move || match driver {
            Some(driver) => driver.snapshot().map_err(|_| ApiError::internal()),
            None => Ok(policy.accounts()),
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    state.model_catalog.retain_accounts(&snapshot.accounts);
    state
        .model_catalog
        .set_account_session(state.account_session.clone());
    let account = snapshot
        .accounts
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(account_key))
        .ok_or_else(|| bad_request("Unknown Codex account"))?;
    if matches!(
        account.get("status").and_then(Value::as_str),
        Some("changed" | "error")
    ) {
        return Err(bad_request("Account is unavailable"));
    }
    let home = account
        .get("home")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let provider = account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("codex");
    match state
        .model_catalog
        .read_until_fresh(account_key, home, provider)
        .await
    {
        studio_runtime_services::model_catalog::CatalogRead::Ready(_) => Ok(()),
        studio_runtime_services::model_catalog::CatalogRead::Pending
        | studio_runtime_services::model_catalog::CatalogRead::Unavailable => {
            Err(ApiError::dependency("provider-session/model-list"))
        }
    }
}

async fn branch_needs_catalog(
    store: &std::sync::Arc<RuntimeStore>,
    operation_id: &str,
    signature: &str,
) -> Result<bool, ApiError> {
    let store = store.clone();
    let operation_id = operation_id.to_owned();
    let signature = signature.to_owned();
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/branches",
        move || {
            let db = store.read().map_err(ApiError::stored)?;
            let Some(raw) = db
                .query_row(
                    "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                    [&operation_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|_| ApiError::internal())?
            else {
                return Ok(true);
            };
            let operation: Value = serde_json::from_str(&raw).map_err(|_| ApiError::internal())?;
            if operation.get("signature").and_then(Value::as_str) != Some(signature.as_str()) {
                return Err(bad_request("branch request id has different content"));
            }
            Ok(operation.get("phase").and_then(Value::as_str) != Some("completed"))
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
}

struct BranchCopy<'a> {
    source: &'a str,
    target: &'a str,
    selected: &'a studio_runtime_store::TranscriptItem,
    message_id: &'a str,
    fork_turn: Option<&'a str>,
    before: bool,
    provider: &'a Value,
    operation_id: &'a str,
}

async fn copy_branch_history(
    store: &RuntimeStore,
    copy: BranchCopy<'_>,
    wake: Arc<tokio::sync::Notify>,
) -> Result<Value, ApiError> {
    let BranchCopy {
        source,
        target,
        selected,
        message_id,
        fork_turn,
        before,
        provider,
        operation_id,
    } = copy;
    let source = source.to_owned();
    let target = target.to_owned();
    let selected = selected.clone();
    let message_id = message_id.to_owned();
    let fork_turn = fork_turn.map(str::to_owned);
    let provider = provider.clone();
    let operation_id = operation_id.to_owned();
    let (result, _) = store.transaction_with_post_commit(move |tx|{
        let raw:String=tx.query_row("SELECT record FROM runtime_workspace_operations WHERE id=?1",[operation_id.as_str()],|r|r.get(0))?;
        let mut operation:Value=serde_json::from_str(&raw)?;
        let current=AgentRepository::get(tx,&AgentId::from(source.as_str()))?.ok_or(studio_runtime_store::StoreError::SchemaMismatch("missing branch source".into()))?;
        if CheckpointsRepository::workspace_source(&current.value()) != operation["source"]
            || current.extra.get("workspaceReservationId").and_then(Value::as_str) != Some(operation_id.as_str())
        {
            return Err(studio_runtime_store::StoreError::SchemaMismatch("branch source changed before local copy".into()));
        }
        let mut child=AgentRepository::get(tx,&AgentId::from(target.as_str()))?.ok_or(studio_runtime_store::StoreError::SchemaMismatch("missing branch draft".into()))?;
        child.extra.insert("yoloMode".into(), current.extra.get("yoloMode").cloned().unwrap_or(Value::Null));
        child.extra.insert("sandbox".into(), provider.get("sandbox").cloned().or_else(|| current.extra.get("sandbox").cloned()).unwrap_or(Value::Null));
        child.extra.insert("approvalPolicy".into(), provider.get("approvalPolicy").cloned().or_else(|| current.extra.get("approvalPolicy").cloned()).unwrap_or(Value::Null));
        child.extra.insert("profileId".into(), provider.get("activePermissionProfile").cloned().or_else(|| current.extra.get("profileId").cloned()).unwrap_or(Value::Null));
        child.extra.insert("threadId".into(),provider["thread"]["id"].clone());
        child.extra.insert("forkedFrom".into(),json!(source)); child.extra.insert("sourceMessage".into(),json!(selected.id));
        child.extra.insert("status".into(),json!("idle")); child.extra.insert("autoWake".into(),json!(true)); child.extra.insert("needsTitle".into(),json!(false));
        child.extra.remove("quickCreate"); child.extra.remove("quickCreateRequest");
        let mut stmt=tx.prepare_cached("SELECT id,record,created FROM runtime_items WHERE agent=?1 AND json_extract(record,'$.afterRestore') IS NULL ORDER BY created,id")?;
        let rows=stmt.query_map([source.as_str()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,f64>(2)?)))?.collect::<Result<Vec<_>,_>>()?;
        drop(stmt);
        let cutoff=match fork_turn {
            Some(fork_turn) => rows.iter().filter_map(|(_,raw,created)|serde_json::from_str::<Value>(raw).ok().filter(|r|r["turnId"].as_str()==Some(fork_turn.as_str())).map(|_|*created)).reduce(f64::max).unwrap_or(f64::NEG_INFINITY),
            None => f64::NEG_INFINITY,
        };
        let mut assets=BTreeMap::<String,Value>::new();
        for (old_id, raw, _created) in rows.into_iter().take_while(|(_, _, created)| *created <= cutoff) {
            let source_record: Value = serde_json::from_str(&raw)?;
            if before && source_record["turnId"].as_str()==selected.record["turnId"].as_str(){continue;}
            let local_id=old_id.split_once(':').map(|(_,id)|id).unwrap_or(&old_id);
            let id=format!("{target}:{local_id}");
            let role = source_record["role"].as_str().unwrap_or("assistant");
            let source_title = source_record
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty());
            let index_kind = source_title.unwrap_or(role).to_owned();
            let title = source_title.map(str::to_owned).unwrap_or_else(|| match role {
                    "assistant" => "Assistant".to_owned(),
                    "user" => "User".to_owned(),
                    "system" => "System".to_owned(),
                    other => other.to_owned(),
                });
            let mut item_assets = source_record.get("assets").cloned().unwrap_or_else(|| json!([]));
            copy_asset_views(tx,&source,&target,&mut item_assets,&mut assets)?;
            let source_inputs = source_record.get("inputs").and_then(Value::as_array);
            let mut inputs = Vec::new();
            if let Some(source_inputs) = source_inputs {
                let mut remaining = 20_000usize;
                for input in source_inputs {
                    let mut input_assets = input.get("assets").cloned().unwrap_or_else(|| json!([]));
                    copy_asset_views(tx,&source,&target,&mut input_assets,&mut assets)?;
                    let input_text = input.get("text").and_then(Value::as_str).unwrap_or_default();
                    let excerpt = input_text.chars().take(remaining).collect::<String>();
                    remaining = remaining.saturating_sub(excerpt.chars().count());
                    inputs.push(json!({
                        "id":input.get("id"),
                        "at":input.get("created").or_else(|| input.get("at")),
                        "kind":input.get("kind"),
                        "text":excerpt,
                        "truncated":excerpt.chars().count() < input_text.chars().count(),
                        "assets":input_assets
                    }));
                }
            }
            let text = source_record.get("text").and_then(Value::as_str).unwrap_or_default();
            let visible_text = text.chars().take(20_000).collect::<String>();
            let copied_at = unix_time();
            let mut record = json!({
                "id":id,
                "role":role,
                "title":title,
                "text":visible_text,
                "truncated":text.chars().count() > 20_000,
                "at":copied_at,
                "turnId":source_record.get("turnId"),
                "streaming":false,
                "assets":item_assets
            });
            if source_inputs.is_some() {
                record["inputs"] = json!(inputs);
            }
            TranscriptRepository::put_item_with_search_kind(
                tx,
                &id,
                &target,
                &record,
                copied_at,
                text,
                TranscriptSearchKind {
                    indexed: true,
                    kind: Some(&index_kind),
                },
            )
                .map_err(transcript_store_error)?;
        }
        if before {
            let draft = branch_draft_payload(
                tx,
                &source,
                &target,
                &selected,
                &message_id,
                &mut assets,
            )?;
            child.extra.insert("draft".into(), draft);
        }
        let value=child.value(); AgentRepository::put(tx,&child)?;
        studio_runtime_services::agents_create::publish_agent(tx,&value)?;
        let result=value.clone();
        operation["phase"]=json!("completed"); operation["result"]=result.clone(); operation["updated"]=json!(unix_time());
        tx.execute("UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",params![operation_id.as_str(),operation.to_string()])?;
        if let Some(mut actor)=AgentRepository::get(tx,&AgentId::from(source))? {
            actor.extra.remove("workspaceReservationId");
            actor.extra.insert("workspaceOperation".into(),Value::Null);
            let actor_value = actor.value();
            AgentRepository::put(tx,&actor)?;
            studio_runtime_services::agents_create::publish_agent(tx,&actor_value)?;
        }
        operation["result"] = result.clone();
        tx.execute("UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",params![operation_id.as_str(),operation.to_string()])?;
        Ok::<_,studio_runtime_store::StoreError>(result)
    }, move |_| wake.notify_one()).await.map_err(ApiError::stored)?;
    Ok(result)
}

async fn ensure_branch_source(
    store: &RuntimeStore,
    operation_id: &str,
    agent_id: &str,
) -> Result<(), ApiError> {
    let operation_id = operation_id.to_owned();
    let agent_id = agent_id.to_owned();
    store
        .transaction_with(move |tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                [operation_id.as_str()],
                |row| row.get(0),
            )?;
            let operation: Value = serde_json::from_str(&raw)?;
            let current = AgentRepository::get(tx, &AgentId::from(agent_id))?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            if CheckpointsRepository::workspace_source(&current.value()) != operation["source"]
                || current
                    .extra
                    .get("workspaceReservationId")
                    .and_then(Value::as_str)
                    != Some(operation_id.as_str())
            {
                return Err(studio_runtime_store::StoreError::SchemaMismatch(
                    "branch source changed before local copy".into(),
                ));
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .map_err(ApiError::stored)
}

fn branch_draft_payload(
    tx: &rusqlite::Transaction<'_>,
    source: &str,
    target: &str,
    selected: &studio_runtime_store::TranscriptItem,
    message_id: &str,
    assets: &mut BTreeMap<String, Value>,
) -> Result<Value, studio_runtime_store::StoreError> {
    let record = &selected.record;
    let inputs = record.get("inputs").and_then(Value::as_array);
    let prompt_index = inputs.and_then(|inputs| {
        inputs.iter().enumerate().position(|(index, input)| {
            let display_id = input
                .get("id")
                .and_then(Value::as_str)
                .map(|id| format!("{source}:{id}"))
                .unwrap_or_else(|| format!("{}:{index}", selected.id));
            display_id == message_id
        })
    });
    let prompt = prompt_index
        .and_then(|index| inputs.and_then(|inputs| inputs.get(index)))
        .unwrap_or(record);
    let text = prompt
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| record.get("text").and_then(Value::as_str).unwrap_or(""));
    let mut prefix = Vec::new();
    let mut prefix_assets = Vec::new();
    if let (Some(inputs), Some(prompt_index)) = (inputs, prompt_index) {
        for input in inputs[..prompt_index].iter() {
            if input.get("kind").and_then(Value::as_str) != Some("user") {
                continue;
            }
            prefix.push(input.get("text").and_then(Value::as_str).unwrap_or(""));
            if let Some(views) = input.get("assets").cloned() {
                prefix_assets.push(views);
            }
        }
    }
    let mut prompt_assets = prompt.get("assets").cloned().unwrap_or_else(|| json!([]));
    let mut prefix_asset_views = Value::Array(
        prefix_assets
            .into_iter()
            .flat_map(|value| value.as_array().cloned().unwrap_or_default())
            .collect(),
    );
    copy_asset_views(tx, source, target, &mut prompt_assets, assets)?;
    copy_asset_views(tx, source, target, &mut prefix_asset_views, assets)?;
    let mut all_assets = prefix_asset_views.as_array().cloned().unwrap_or_default();
    all_assets.extend(prompt_assets.as_array().cloned().unwrap_or_default());
    Ok(json!({
        "text": text,
        "prefixText": prefix.join("\n\n"),
        "assets": all_assets,
    }))
}

fn transcript_store_error(
    error: studio_runtime_store::TranscriptStoreError,
) -> studio_runtime_store::StoreError {
    use studio_runtime_store::TranscriptStoreError;
    match error {
        TranscriptStoreError::Sqlite(error) => error.into(),
        TranscriptStoreError::Store(error) => error,
        TranscriptStoreError::InvalidStoredObject { table, id } => {
            studio_runtime_store::StoreError::SchemaMismatch(format!(
                "invalid stored object {table}/{id}"
            ))
        }
        TranscriptStoreError::ActiveTurn => studio_runtime_store::StoreError::SchemaMismatch(
            "transcript unexpectedly belongs to an active turn".into(),
        ),
    }
}

fn copy_asset_views(
    tx: &rusqlite::Transaction<'_>,
    _source: &str,
    target: &str,
    views: &mut Value,
    map: &mut BTreeMap<String, Value>,
) -> Result<(), studio_runtime_store::StoreError> {
    let Some(array) = views.as_array_mut() else {
        return Ok(());
    };
    for view in array {
        let Some(old) = view.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(existing) = map.get(old) {
            *view = existing.clone();
            continue;
        }
        let Some(asset) = WorkspaceFilesRepository::asset(tx, old)? else {
            return Err(studio_runtime_store::StoreError::SchemaMismatch(
                "missing transcript asset".into(),
            ));
        };
        let mut record = asset.value().clone();
        let id = deterministic_uuid(&format!("{target}:{old}"));
        record["id"] = json!(id);
        record["agent"] = json!(target);
        tx.execute(
            "INSERT INTO runtime_assets(id,record) VALUES(?1,?2)",
            params![id, record.to_string()],
        )?;
        let mut copied_view = record.clone();
        if let Some(object) = copied_view.as_object_mut() {
            object.remove("path");
        }
        map.insert(old.to_owned(), copied_view.clone());
        *view = copied_view;
    }
    Ok(())
}

fn deterministic_uuid(seed: &str) -> String {
    let hash = sha256_hex(seed.as_bytes());
    format!(
        "{}-{}-4{}-a{}-{}",
        &hash[0..8],
        &hash[8..12],
        &hash[13..16],
        &hash[17..20],
        &hash[20..32]
    )
}
fn unix_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "No live provider session for the selected account",
    )
}
fn bad_request(message: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}

inventory::submit! { crate::routes::manifest::RouteModuleRegistration { name:"branches",routes:Some(routes),dependencies:&[] } }
