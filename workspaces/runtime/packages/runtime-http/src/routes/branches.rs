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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use studio_runtime_providers::Method;
use studio_runtime_schema::transcript_asset::TranscriptAssetView;
use studio_runtime_schema::{queue_command::Field, workspace_operation::OperationPhase};
use studio_runtime_services::account_session_runtime::SessionOutcome as ReceiptOutcome;
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

// Number of turn metadata records per branch lookup page. This uses the
// retained session's existing actor-bound metadata read shape.
const BRANCH_TURNS_PER_PAGE: u32 = 10;

// Branch retries predate the strict persisted record and may contain partial
// or opaque provider evidence. Keep that evidence and its presence verbatim.
#[derive(Deserialize, Serialize)]
struct BranchOperation {
    signature: String,
    phase: OperationPhase,
    #[serde(rename = "forkTurnId", default)]
    fork_turn_id: Option<String>,
    #[serde(
        rename = "legacyPhase",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    legacy_phase: Field<Box<serde_json::value::RawValue>>,
    #[serde(
        rename = "legacyPhaseCollision",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    legacy_phase_collision: Field<Box<serde_json::value::RawValue>>,
    #[serde(
        rename = "legacyRecord",
        default,
        skip_serializing_if = "Field::is_missing"
    )]
    legacy_record: Field<Box<serde_json::value::RawValue>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    provider: Field<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "decode_result"
    )]
    result: Option<Box<serde_json::value::RawValue>>,
    #[serde(flatten)]
    other: serde_json::Map<String, Value>,
}

fn decode_result<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Box<serde_json::value::RawValue>>, D::Error> {
    let result = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
    // The inspected 2026-10-31 snapshot contained zero null branch results.
    if result.get() == "null" {
        return Err(serde::de::Error::custom("null branch result"));
    }
    Ok(Some(result))
}

fn decode_branch_operation(raw: &str) -> Result<BranchOperation, serde_json::Error> {
    serde_json::from_str::<studio_runtime_schema::json_object::JsonObject<BranchOperation>>(raw)
        .map(|object| object.0)
}

impl BranchOperation {
    fn set_phase(&mut self, phase: OperationPhase) {
        self.phase = phase;
    }

    fn value(field: &Field<Value>) -> Option<&Value> {
        match field {
            Field::Missing => None,
            Field::Null => Some(&Value::Null),
            Field::Value(value) => Some(value),
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchTurnPage {
    data: Vec<BranchTurnId>,
    next_cursor: Option<String>,
}

// Native turn fields other than the ID are deliberately not retained here.
#[derive(serde::Deserialize)]
struct BranchTurnId {
    id: String,
}

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/branch",
        OperationStatus::Implemented,
        branch,
    )])
}

fn branch_draft_request(
    source: &Value,
    child_id: String,
    account: String,
    model: &str,
) -> Result<crate::generated::CreateLeadRequest, ApiError> {
    Ok(crate::generated::CreateLeadRequest {
        id: Some(Some(child_id)),
        account_key: Some(account),
        cwd: serde_json::from_value(source["cwd"].clone()).map_err(|_| ApiError::internal())?,
        model: Some(Some(model.to_owned())),
        yolo_mode: serde_json::from_value(source["yoloMode"].clone())
            .map_err(|_| ApiError::internal())?,
        ..Default::default()
    })
}

async fn branch(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::BranchRequest>,
) -> Result<Json<crate::generated::BranchResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let port = state.account_session.clone().ok_or_else(unavailable)?;
    let agent_id = body.agent.clone();
    let account = store
        .read_async("runtime-http/routes/branches", move |db| {
            let agent = AgentRepository::get(db, &AgentId::from(agent_id.as_str()))?
                .ok_or_else(|| bad_request("Unknown managed agent"))?;
            Ok::<_, ApiError>(agent.account_key.as_str().to_owned())
        })
        .await?;
    let operation_id = format!("branch:{}", body.id);
    let signature = sha256_hex(
        serde_json::to_string(&json!({
            "agent":body.agent,"id":body.id,"message_id":body.message_id,"before":body.before
        }))
        .map_err(|_| ApiError::internal())?
        .as_bytes(),
    );
    let (snapshot, selected) = {
        let agent_id = body.agent.clone();
        let message_id = body.message_id.clone();
        store
            .read_async("runtime-http/routes/branches", move |db| {
                let snapshot = CheckpointsRepository::workspace_snapshot(db, &agent_id)?
                    .ok_or_else(|| bad_request("Unknown managed agent"))?;
                let selected = TranscriptRepository::resolve_item(db, &agent_id, &message_id)
                    .map_err(|_| ApiError::internal())?
                    .ok_or_else(|| bad_request("Unknown message in this conversation"))?;
                Ok::<_, ApiError>((snapshot, selected))
            })
            .await?
    };
    let source = snapshot.agent.value();
    if !selected.record.after_restore_field().is_missing()
        || selected
            .record
            .turn_id()
            .value()
            .map(String::as_str)
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
            == selected.record.turn_id().value().map(String::as_str)
    {
        return Err(bad_request("Wait for this turn to finish before branching"));
    }
    // Match Python's read-only request validation before requiring a live native
    // consumer. Invalid branch inputs must not depend on provider availability.
    port.ensure_session(&account).await.map_err(|error| {
        if matches!(error, AccountSessionError::Expired) {
            return super::account_login::session_error(error);
        }
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Could not ensure account session: {error}"),
        )
    })?;
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
                    let mut prior = decode_branch_operation(&prior)?;
                    if prior.signature != signature {
                        return Err(studio_runtime_store::StoreError::SchemaMismatch(
                            "branch request id has different content".into(),
                        ));
                    }
                    match prior.phase {
                        OperationPhase::Completed => {
                            return Ok(Some(prior));
                        }
                        OperationPhase::ProviderReady => {}
                        OperationPhase::Failed => {
                            prior.set_phase(OperationPhase::ProviderPending);
                            prior.provider = Field::Missing;
                            prior.result = None;
                            prior.other.remove("error");
                            tx.execute(
                                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                                params![operation_id, serde_json::to_string(&prior)?],
                            )?;
                        }
                        OperationPhase::ProviderPending | OperationPhase::RecoveryRequired => {
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
                            prior.other.insert("reserved".into(), json!(true));
                            tx.execute(
                                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                                params![operation_id, serde_json::to_string(&prior)?],
                            )?;
                            return Ok(Some(prior));
                        }
                        OperationPhase::CapturePending
                        | OperationPhase::CaptureRunning
                        | OperationPhase::LocalMutation => {
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
                    decode_branch_operation(&existing)?
                } else {
                    let other = serde_json::Map::from_iter([
                        ("id".into(), json!(operation_id)),
                        ("kind".into(), json!("branch")),
                        ("agent".into(), json!(agent_id)),
                        (
                            "rootRequestId".into(),
                            json!(
                                studio_runtime_schema::persistence::RequestIdentity::issue_root()
                            ),
                        ),
                        ("source".into(), source_stamp),
                        ("message_id".into(), json!(message_id)),
                        ("turn_id".into(), json!(selected.record.turn_id().value())),
                        ("previousStatus".into(), json!(source.get("status"))),
                        ("previousAutoWake".into(), json!(source.get("autoWake"))),
                        (
                            "previousWorkspaceOperation".into(),
                            json!(source.get("workspaceOperation")),
                        ),
                        ("created".into(), json!(unix_time())),
                    ]);
                    let record = BranchOperation {
                        signature,
                        phase: OperationPhase::ProviderPending,
                        fork_turn_id: None,
                        legacy_phase: Field::Missing,
                        legacy_phase_collision: Field::Missing,
                        legacy_record: Field::Missing,
                        provider: Field::Missing,
                        result: None,
                        other,
                    };
                    tx.execute(
                        "INSERT INTO runtime_workspace_operations(id,record) VALUES(?1,?2)",
                        params![operation_id, serde_json::to_string(&record)?],
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
                record.other.insert("reserved".into(), json!(true));
                tx.execute(
                    "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                    params![operation_id, serde_json::to_string(&record)?],
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
    if let Some(op) = operation
        .as_ref()
        .filter(|op| op.phase == OperationPhase::Completed)
    {
        let result = op.result.as_ref().ok_or_else(ApiError::internal)?;
        return serde_json::from_str::<crate::generated::BranchResponse>(result.get())
            .map(Json)
            .map_err(ApiError::json);
    }
    let prior_provider = operation
        .as_ref()
        .and_then(|op| BranchOperation::value(&op.provider))
        .cloned();
    let scope = port.ensure_session(&account).await.map_err(|error| {
        if matches!(error, AccountSessionError::Expired) {
            return super::account_login::session_error(error);
        }
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
    let selected_turn = selected.record.turn_id().value().cloned();
    let fork_turn = if before {
        async {
            // Stable per-page operation IDs let the retained broker replay a known
            // page without resending an uncertain one.
            let mut cursor: Option<String> = None;
            let mut seen_cursors = HashSet::new();
            let selected_turn = selected_turn.as_deref().ok_or_else(ApiError::internal)?;
            let mut selected_found = false;
            let mut page = 0_u64;
            loop {
                let mut params = json!({
                    "threadId":source["threadId"],
                    "limit":BRANCH_TURNS_PER_PAGE,
                    "sortDirection":"desc",
                    "itemsView":"notLoaded"
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
                let result: BranchTurnPage =
                    serde_json::from_value(result).map_err(|_| ApiError::internal())?;
                for turn in result.data {
                    if turn.id.is_empty() {
                        return Err(ApiError::internal());
                    }
                    if selected_found {
                        return Ok(Some(turn.id));
                    }
                    selected_found = turn.id == selected_turn;
                }
                let Some(next) = result.next_cursor.filter(|next| !next.is_empty()) else {
                    return if selected_found {
                        Ok(None)
                    } else {
                        Err(bad_request(
                            "The selected turn is absent from native history",
                        ))
                    };
                };
                if !seen_cursors.insert(next.to_owned()) {
                    return Err(ApiError::internal());
                }
                cursor = Some(next.to_owned());
                page = page.checked_add(1).ok_or_else(ApiError::internal)?;
            }
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
            root_request_id: operation
                .as_ref()
                .and_then(|owner| owner.other.get("rootRequestId"))
                .and_then(Value::as_str)
                .or_else(|| {
                    operation
                        .as_ref()
                        .and_then(|owner| owner.other.get("providerAdmission"))
                        .map(|_| "")
                })
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::CONFLICT,
                        "The branch request identity is unavailable; inspect workspace recovery",
                    )
                })?
                .to_owned(),
            admitted_request: operation
                .as_ref()
                .and_then(|owner| owner.other.get("providerAdmission"))
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(ApiError::json)?,
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
            Ok(studio_runtime_services::account_session_runtime::SessionReceipt {
                outcome: ReceiptOutcome::Written { response, .. },
                ..
            }) => match response {
                ProviderResponse::Reply(value) => value,
                ProviderResponse::Rejected(error) => {
                    settle_failure(&store, &operation_id, false, &error.to_string()).await?;
                    return Err(bad_request(&error.to_string()));
                }
            },
            Ok(receipt) if receipt.certainty() == RequestCertainty::NotSent => {
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
            let mut op = decode_branch_operation(&raw)?;
            let current = AgentRepository::get(tx, &AgentId::from(persist_agent_id.as_str()))?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let current_source = CheckpointsRepository::workspace_source(&current.value());
            if current_source != *op.other.get("source").unwrap_or(&Value::Null)
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
            op.provider = Field::Value(persist_provider);
            op.set_phase(OperationPhase::ProviderReady);
            tx.execute(
                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                params![persist_operation_id, serde_json::to_string(&op)?],
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
    let created = match branch_draft_request(&source, child_id, account, branch_model) {
        Ok(request) => super::agents_create::create_branch_draft(state.clone(), request).await,
        Err(error) => Err(error),
    };
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
        root_request_id: String::new(),
        admitted_request: None,
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
                AccountSessionError::Expired => ApiError::new(StatusCode::GONE, error.to_string()),
                AccountSessionError::Requires(name) => ApiError::dependency(name),
                AccountSessionError::Unknown => unavailable(),
                AccountSessionError::Refused(_)
                | AccountSessionError::InvalidResponse(_)
                | AccountSessionError::Persistence => bad_request(&error.to_string()),
            })?;
    if receipt.certainty() == RequestCertainty::NotSent {
        // A worker can disappear between ensuring the account session and
        // submitting the request. A NotSent receipt permits retrying, but the
        // operation identity and payload remain unchanged.
        request.expected_scope = Some(port.ensure_session(account).await.map_err(|error| {
            if matches!(error, AccountSessionError::Expired) {
                return super::account_login::session_error(error);
            }
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Could not ensure account session: {error}"),
            )
        })?);
        receipt = port
            .provider_request(request)
            .await
            .map_err(|error| match error {
                AccountSessionError::Expired => ApiError::new(StatusCode::GONE, error.to_string()),
                AccountSessionError::Requires(name) => ApiError::dependency(name),
                AccountSessionError::Unknown => unavailable(),
                AccountSessionError::Refused(_)
                | AccountSessionError::InvalidResponse(_)
                | AccountSessionError::Persistence => bad_request(&error.to_string()),
            })?;
    }
    match receipt.outcome {
        ReceiptOutcome::Written {
            response: ProviderResponse::Reply(value),
            ..
        } => Ok(value),
        ReceiptOutcome::Written {
            response: ProviderResponse::Rejected(error),
            ..
        } => Err(bad_request(&error.to_string())),
        ReceiptOutcome::Unknown { .. }
        | ReceiptOutcome::WorkerNotSent { .. }
        | ReceiptOutcome::NoLiveAccountWorker
        | ReceiptOutcome::ExpectedScopeMismatch
        | ReceiptOutcome::DeadlineExpired
        | ReceiptOutcome::NotHandedOff => Err(unavailable()),
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
            let mut operation = decode_branch_operation(&raw)?;
            operation.set_phase(if unknown {
                OperationPhase::RecoveryRequired
            } else {
                OperationPhase::Failed
            });
            operation.other.insert("error".into(), json!(error.clone()));
            tx.execute(
                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                params![operation_id.as_str(), serde_json::to_string(&operation)?],
            )?;
            if let Some(mut actor) = AgentRepository::get(
                tx,
                &AgentId::from(
                    operation
                        .other
                        .get("agent")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
            )? {
                if unknown {
                    actor.set_status(studio_runtime_store::AgentStatus::Interrupted);
                    actor.auto_wake = Some(false);
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
            let mut operation = decode_branch_operation(&raw)?;
            if operation.phase != OperationPhase::ProviderReady
                || BranchOperation::value(&operation.provider).is_none()
            {
                return Err(studio_runtime_store::StoreError::SchemaMismatch(
                    "branch provider result is not durably ready".into(),
                ));
            }
            operation.other.insert("reserved".into(), json!(false));
            tx.execute(
                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                params![operation_id.as_str(), serde_json::to_string(&operation)?],
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
            let mut operation = decode_branch_operation(&raw)?;
            operation.fork_turn_id = fork_turn;
            tx.execute(
                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                params![operation_id.as_str(), serde_json::to_string(&operation)?],
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
    state
        .model_catalog
        .retain_public_accounts(&snapshot.accounts);
    state
        .model_catalog
        .set_account_session(state.account_session.clone());
    let account = snapshot
        .accounts
        .iter()
        .find(|row| row.id.as_str() == Some(account_key))
        .ok_or_else(|| bad_request("Unknown Codex account"))?;
    if matches!(account.status.as_str(), Some("changed" | "error")) {
        return Err(bad_request("Account is unavailable"));
    }
    let home = account.home.as_str().unwrap_or_default();
    let provider = account
        .provider_kind()
        .ok_or_else(|| bad_request("Account provider is unknown"))?;
    match state
        .model_catalog
        .wait_for_admission_catalog(account_key, home, provider)
        .await
    {
        studio_runtime_services::model_catalog::AdmissionCatalogRead::Fresh { .. } => Ok(()),
        studio_runtime_services::model_catalog::AdmissionCatalogRead::Pending => {
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                super::models::PENDING_MESSAGE,
            ))
        }
        studio_runtime_services::model_catalog::AdmissionCatalogRead::Unavailable => {
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                super::models::UNAVAILABLE_MESSAGE,
            ))
        }
    }
}

async fn branch_needs_catalog(
    store: &std::sync::Arc<RuntimeStore>,
    operation_id: &str,
    signature: &str,
) -> Result<bool, ApiError> {
    let operation_id = operation_id.to_owned();
    let signature = signature.to_owned();
    store
        .read_async("runtime-http/routes/branches", move |db| {
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
            let operation = decode_branch_operation(&raw).map_err(|_| ApiError::internal())?;
            if operation.signature != signature {
                return Err(bad_request("branch request id has different content"));
            }
            Ok(operation.phase != OperationPhase::Completed)
        })
        .await
}

// Historical copy rows contain optional fields and opaque input evidence.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchTurn {
    turn_id: Option<String>,
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
        let mut operation=decode_branch_operation(&raw)?;
        let current=AgentRepository::get(tx,&AgentId::from(source.as_str()))?.ok_or(studio_runtime_store::StoreError::SchemaMismatch("missing branch source".into()))?;
        if CheckpointsRepository::workspace_source(&current.value()) != *operation.other.get("source").unwrap_or(&Value::Null)
            || current.extra.get("workspaceReservationId").and_then(Value::as_str) != Some(operation_id.as_str())
        {
            return Err(studio_runtime_store::StoreError::SchemaMismatch("branch source changed before local copy".into()));
        }
        let mut child=AgentRepository::get(tx,&AgentId::from(target.as_str()))?.ok_or(studio_runtime_store::StoreError::SchemaMismatch("missing branch draft".into()))?;
        child.extra.insert("yoloMode".into(), current.extra.get("yoloMode").cloned().unwrap_or(Value::Null));
        child.sandbox = provider.get("sandbox").map(Option::<studio_runtime_store::AgentSandbox>::deserialize).transpose()?.unwrap_or_else(|| current.sandbox.clone());
        child.approval_policy = provider.get("approvalPolicy").map(Option::<String>::deserialize).transpose()?.unwrap_or_else(|| current.approval_policy.clone());
        child.extra.insert("profileId".into(), provider.get("activePermissionProfile").cloned().or_else(|| current.extra.get("profileId").cloned()).unwrap_or(Value::Null));
        child.thread_id = match &provider["thread"]["id"] { Value::Null => None, Value::String(id) => Some(id.clone()), Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => return Err(studio_runtime_store::StoreError::SchemaMismatch("invalid branch thread".into())) };
        child.extra.insert("forkedFrom".into(),json!(source)); child.extra.insert("sourceMessage".into(),json!(selected.id));
        child.set_status(studio_runtime_store::AgentStatus::Idle); child.auto_wake = Some(true); child.extra.insert("needsTitle".into(),json!(false));
        child.extra.remove("quickCreate"); child.extra.remove("quickCreateRequest");
        let mut stmt=tx.prepare_cached("SELECT id,record,created FROM runtime_items WHERE agent=?1 AND json_extract(record,'$.afterRestore') IS NULL ORDER BY created,id")?;
        let rows=stmt.query_map([source.as_str()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,f64>(2)?)))?.collect::<Result<Vec<_>,_>>()?;
        drop(stmt);
        let cutoff=match fork_turn {
            Some(fork_turn) => rows.iter().filter_map(|(_,raw,created)|serde_json::from_str::<studio_runtime_schema::json_object::JsonObject<BranchTurn>>(raw).ok().map(|r| r.0).filter(|r|r.turn_id.as_deref()==Some(fork_turn.as_str())).map(|_|*created)).reduce(f64::max).unwrap_or(f64::NEG_INFINITY),
            None => f64::NEG_INFINITY,
        };
        let mut assets=BTreeMap::<String,TranscriptAssetView>::new();
        for (old_id, raw, _created) in rows.into_iter().take_while(|(_, _, created)| *created <= cutoff) {
            let source_record = serde_json::from_str::<studio_runtime_schema::transcript_record::TranscriptItemRecord>(&raw)
                .map_err(|_| studio_runtime_store::StoreError::InvalidStoredObject { table: "runtime_items", id: old_id.clone() })?;
            if before && source_record.turn_id().value()==selected.record.turn_id().value(){continue;}
            let local_id=old_id.split_once(':').map(|(_,id)|id).unwrap_or(&old_id);
            let id=format!("{target}:{local_id}");
            let role = source_record.role().as_str();
            let source_title = source_record.title()
                .filter(|title| !title.is_empty());
            let index_kind = source_title.unwrap_or(role).to_owned();
            let title = source_title.map(str::to_owned).unwrap_or_else(|| match role {
                    "assistant" => "Assistant".to_owned(),
                    "user" => "User".to_owned(),
                    "system" => "System".to_owned(),
                    other => other.to_owned(),
                });
            let mut item_assets = decode_asset_views(source_record.assets())?;
            copy_asset_views(tx,&target,&mut item_assets,&mut assets)?;
            let source_inputs = source_record.inputs().value();
            let mut inputs = Vec::new();
            if let Some(source_inputs) = source_inputs {
                let mut remaining = 20_000usize;
                for input in source_inputs {
                    let timestamp = input.created_or_at()?;
                    if input.id().is_none() || input.kind().is_none() || timestamp.is_none() || input.text().is_none() || input.assets().value().is_none() {
                        return Err(studio_runtime_store::StoreError::TranscriptInputsIncomplete { id: old_id.clone() });
                    }
                    let mut input_assets = decode_asset_views(input.assets())?;
                    copy_asset_views(tx,&target,&mut input_assets,&mut assets)?;
                    let input_text = input.text().unwrap_or_default();
                    let excerpt = input_text.chars().take(remaining).collect::<String>();
                    remaining = remaining.saturating_sub(excerpt.chars().count());
                    inputs.push(json!({
                        "id":input.id(),
                        "at":timestamp,
                        "kind":input.kind(),
                        "text":excerpt,
                        "truncated":excerpt.chars().count() < input_text.chars().count(),
                        "assets":input_assets
                    }));
                }
            }
            let text = source_record.text().value().map(String::as_str).unwrap_or_default();
            let visible_text = text.chars().take(20_000).collect::<String>();
            let copied_at = unix_time();
            let mut record = json!({
                "id":id,
                "role":role,
                "title":title,
                "text":visible_text,
                "truncated":text.chars().count() > 20_000,
                "at":copied_at,
                "turnId":source_record.turn_id().value(),
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
            child.draft = Some(Box::new(draft));
        }
        let value=child.value(); AgentRepository::put(tx,&child)?;
        studio_runtime_services::agents_create::publish_agent(tx,&value)?;
        let result=value.clone();
        operation.set_phase(OperationPhase::Completed);
        operation.result = Some(serde_json::value::to_raw_value(&result)?);
        operation.other.insert("updated".into(), json!(unix_time()));
        tx.execute("UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",params![operation_id.as_str(),serde_json::to_string(&operation)?])?;
        if let Some(mut actor)=AgentRepository::get(tx,&AgentId::from(source))? {
            actor.extra.remove("workspaceReservationId");
            actor.extra.insert("workspaceOperation".into(),Value::Null);
            let actor_value = actor.value();
            AgentRepository::put(tx,&actor)?;
            studio_runtime_services::agents_create::publish_agent(tx,&actor_value)?;
        }
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
            let operation = decode_branch_operation(&raw)?;
            let current = AgentRepository::get(tx, &AgentId::from(agent_id))?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            if CheckpointsRepository::workspace_source(&current.value())
                != *operation.other.get("source").unwrap_or(&Value::Null)
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
    assets: &mut BTreeMap<String, TranscriptAssetView>,
) -> Result<studio_runtime_store::AgentDraft, studio_runtime_store::StoreError> {
    let record = &selected.record;
    let inputs = record.inputs().value();
    let prompt_index = inputs.and_then(|inputs| {
        inputs.iter().enumerate().position(|(index, input)| {
            let display_id = input
                .id()
                .map(|id| format!("{source}:{id}"))
                .unwrap_or_else(|| format!("{}:{index}", selected.id));
            display_id == message_id
        })
    });
    let prompt = prompt_index.and_then(|index| inputs.and_then(|inputs| inputs.get(index)));
    let text = prompt
        .and_then(|input| input.text())
        .or_else(|| record.text().value().map(String::as_str))
        .unwrap_or("");
    let mut prefix = Vec::new();
    let mut prefix_assets = Vec::new();
    if let (Some(inputs), Some(prompt_index)) = (inputs, prompt_index) {
        for input in &inputs[..prompt_index] {
            if input.kind() != Some("user") {
                continue;
            }
            prefix.push(input.text().unwrap_or(""));
            prefix_assets.extend(decode_asset_views(input.assets())?);
        }
    }
    let mut prompt_assets = match prompt {
        Some(input) => decode_asset_views(input.assets())?,
        None => decode_asset_views(record.assets())?,
    };
    copy_asset_views(tx, target, &mut prompt_assets, assets)?;
    copy_asset_views(tx, target, &mut prefix_assets, assets)?;
    let mut all_assets = prefix_assets;
    all_assets.extend(prompt_assets);
    Ok(studio_runtime_store::AgentDraft {
        text: text.to_owned(),
        prefix_text: prefix.join("\n\n"),
        assets: all_assets,
        extra: BTreeMap::new(),
    })
}

fn decode_asset_views(
    views: &Field<Vec<Box<serde_json::value::RawValue>>>,
) -> Result<Vec<TranscriptAssetView>, studio_runtime_store::StoreError> {
    views
        .value()
        .into_iter()
        .flatten()
        .map(|view| serde_json::from_str(view.get()).map_err(Into::into))
        .collect()
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
    }
}

fn copy_asset_views(
    tx: &rusqlite::Transaction<'_>,
    target: &str,
    views: &mut [TranscriptAssetView],
    map: &mut BTreeMap<String, TranscriptAssetView>,
) -> Result<(), studio_runtime_store::StoreError> {
    for view in views {
        let old = view.id().to_owned();
        if let Some(existing) = map.get(&old) {
            *view = existing.clone();
            continue;
        }
        let Some(asset) = WorkspaceFilesRepository::asset(tx, &old)? else {
            return Err(studio_runtime_store::StoreError::SchemaMismatch(
                "missing transcript asset".into(),
            ));
        };
        let id = deterministic_uuid(&format!("{target}:{old}"));
        let mut asset = asset;
        asset.retarget(id.clone(), target.to_owned());
        WorkspaceFilesRepository::insert_asset(tx, &id, &asset)?;
        let copied_view = asset.into_view();
        map.insert(old, copied_view.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_operation_requires_a_known_phase() {
        assert!(decode_branch_operation(r#"{"phase":"local_mutation"}"#).is_err());
        assert!(decode_branch_operation(r#"{"phase":"local_mutation","signature":null}"#).is_err());
        assert!(decode_branch_operation("{}").is_err());
        assert!(
            decode_branch_operation(r#"{"phase":{"completed":null},"signature":"abc"}"#).is_err()
        );
        let operation = decode_branch_operation(r#"{"phase":"local_mutation","signature":"abc","legacyPhase":{"old":2},"legacyPhaseCollision":[{"older":3},{"new":1}]}"#).expect("migrated operation");
        assert_eq!(
            operation
                .legacy_phase
                .value()
                .expect("phase evidence")
                .get(),
            r#"{"old":2}"#
        );
        assert_eq!(
            operation
                .legacy_phase_collision
                .value()
                .expect("collision evidence")
                .get(),
            r#"[{"older":3},{"new":1}]"#
        );
    }

    #[test]
    fn branch_operation_rejects_non_string_signature() {
        assert!(
            decode_branch_operation(r#"{"phase":"local_mutation","signature":{"bad":true}}"#)
                .is_err()
        );
        assert!(decode_branch_operation(r#"{"phase":"local_mutation","signature":"abc"}"#).is_ok());
    }

    #[test]
    fn branch_operation_preserves_opaque_result_bytes() {
        let raw = r#"{"signature":"s","phase":"completed","result":{"vendor":  {"x":  1}}}"#;
        let operation = decode_branch_operation(raw).unwrap();
        let encoded = serde_json::to_string(&operation).unwrap();
        assert!(encoded.contains(r#""result":{"vendor":  {"x":  1}}"#));
        assert!(
            decode_branch_operation(r#"{"signature":"s","phase":"completed","result":null}"#)
                .is_err()
        );
    }

    #[test]
    fn branch_item_rejects_non_string_turn_id() {
        assert!(
            serde_json::from_str::<studio_runtime_schema::transcript_record::TranscriptItemRecord>(
                r#"{"id":"i","role":"assistant","turnId":7}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<studio_runtime_schema::transcript_record::TranscriptItemRecord>(
                r#"{"id":"i","role":"assistant","turnId":"turn"}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn branch_item_rejects_invalid_asset_view() {
        let invalid: studio_runtime_schema::transcript_record::TranscriptItemRecord =
            serde_json::from_str(r#"{"id":"i","role":"assistant","assets":[{"id":5}]}"#).unwrap();
        let valid: studio_runtime_schema::transcript_record::TranscriptItemRecord =
            serde_json::from_str(r#"{"id":"i","role":"assistant","assets":[{"id":"asset"}]}"#)
                .unwrap();
        assert!(decode_asset_views(invalid.assets()).is_err());
        assert!(decode_asset_views(valid.assets()).is_ok());
    }

    #[test]
    fn agent_record_rejects_invalid_branch_draft() {
        let raw = r#"{"id":"agent","rootId":"agent","epoch":0,"status":"idle","accountKey":"default","compactions":0,"draft":{"text":42,"prefixText":"","assets":[]}}"#;
        assert!(serde_json::from_str::<studio_runtime_store::AgentRecord>(raw).is_err());
        let value = json!({"id":"agent","rootId":"agent","epoch":0,"status":"idle","accountKey":"default","compactions":0,"draft":{"text":"hello","prefixText":"","assets":[{"id":"asset","vendor":{"x":1}}]}});
        let agent = studio_runtime_store::AgentRecord::from_value(value).unwrap();
        assert_eq!(agent.draft.unwrap().assets[0].id(), "asset");
    }

    #[tokio::test]
    async fn copied_asset_views_reuse_projection_without_storage_path() {
        let root = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(root.path())).await.unwrap();
        store.transaction(|tx| {
            let asset: studio_runtime_store::TranscriptAssetRecord = serde_json::from_str(
                r#"{"id":"asset","agent":"source","name":"n","path":"private","mime":"text/plain","image":false,"size":1,"hash":"h","created":2,"vendor":{"x":  1}}"#,
            )?;
            WorkspaceFilesRepository::insert_asset(tx, "asset", &asset)?;
            let mut views: Vec<TranscriptAssetView> = serde_json::from_str(r#"[{"id":"asset"},{"id":"asset"}]"#)?;
            let mut copied = BTreeMap::new();
            copy_asset_views(tx, "child", &mut views, &mut copied)?;
            assert_eq!(views[0].id(), views[1].id());
            assert_eq!(copied.len(), 1);
            let wire = serde_json::to_string(&views)?;
            assert!(!wire.contains("private"));
            assert!(wire.contains(r#""vendor":{"x":  1}"#));
            let selected = studio_runtime_store::TranscriptItem {
                id: "source:message".into(),
                agent: "source".into(),
                record: serde_json::from_str(r#"{"id":"source:message","role":"user","text":"hello","assets":[{"id":"asset"}]}"#)?,
            };
            let draft = branch_draft_payload(tx, "source", "child", &selected, "source:message", &mut copied)?;
            assert_eq!(draft.assets[0].id(), views[0].id());
            let wire = serde_json::to_string(&draft)?;
            assert!(!wire.contains("private"));
            assert!(wire.contains(r#""vendor":{"x":  1}"#));
            Ok::<_, studio_runtime_store::StoreError>(())
        }).await.unwrap();
    }
    use axum::response::IntoResponse;
    use std::sync::Mutex;
    use studio_runtime_services::account_session_runtime::SessionOutcome as ReceiptOutcome;
    use studio_runtime_services::account_session_runtime::{
        AccountSessionFuture, ConnectionScope, SessionReceipt,
    };
    use studio_runtime_store::AgentRecord;

    #[tokio::test]
    async fn branch_history_copy_refuses_incomplete_inputs_and_rolls_back() {
        let root = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(root.path())).await.unwrap();
        let source = json!({"compactions":0,"accountKey":"default","id":"source","rootId":"source","epoch":0,"status":"idle","provider":"codex","cwd":root.path(),"workspaceReservationId":"op"});
        let child = json!({"compactions":0,"accountKey":"default","id":"child","rootId":"child","epoch":0,"status":"idle","provider":"codex","cwd":root.path()});
        let source_identity = CheckpointsRepository::workspace_source(&source);
        let asset_path = root.path().join("file.txt");
        store.transaction(move |tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('source',?1)", [source.to_string()])?;
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('child',?1)", [child.to_string()])?;
            tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES('op',?1)", [json!({"source":source_identity,"signature":"s","phase":"provider_ready"}).to_string()])?;
            tx.execute("INSERT INTO runtime_assets(id,record) VALUES('asset',?1)", [json!({"id":"asset","agent":"source","name":"file.txt","path":asset_path,"mime":"text/plain","image":false,"size":1,"hash":"h","created":1}).to_string()])?;
            tx.execute("INSERT INTO runtime_items(id,agent,record,created) VALUES('source:a','source',?1,1)", [r#"{"id":"source:a","role":"assistant","text":"first","turnId":"t","assets":[{"id":"asset"}],"inputs":[{"text":"second","created":null,"at":2,"assets":[{"id":"asset"}]}]}"#])?;
            tx.execute("INSERT INTO runtime_items(id,agent,record,created) VALUES('source:b','source',?1,1)", [r#"{"turnId":"t","text":null,"assets":null}"#])?;
            tx.execute("INSERT INTO runtime_items(id,agent,record,created) VALUES('source:c','source',?1,1)", [json!({"turnId":7,"text":"🦀".repeat(20_001),"inputs":[{"text":"Ж".repeat(19_999)},{"text":"AB"}]}).to_string()])?;
            tx.execute("INSERT INTO runtime_items(id,agent,record,created) VALUES('source:after','source',?1,2)", [r#"{"turnId":"later","future":1e400}"#])?;
            Ok(())
        }).await.unwrap();
        let selected = studio_runtime_store::TranscriptItem {
            id: "source:a".into(),
            agent: "source".into(),
            record: serde_json::from_str(r#"{"id":"source:a","role":"assistant","turnId":"t"}"#)
                .unwrap(),
        };
        let provider = json!({"thread":{"id":"fork"}});
        let copy = || BranchCopy {
            source: "source",
            target: "child",
            selected: &selected,
            message_id: "source:a",
            fork_turn: Some("t"),
            before: false,
            provider: &provider,
            operation_id: "op",
        };
        let error = copy_branch_history(&store, copy(), Arc::new(tokio::sync::Notify::new()))
            .await
            .expect_err("incomplete historical inputs must not be copied");
        assert_eq!(
            error.into_response().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        store.read_async("fixture", |db| {
            let count: i64 = db.query_row("SELECT count(*) FROM runtime_items WHERE agent='child'", [], |r| r.get(0))?;
            let asset_count: i64 = db.query_row("SELECT count(*) FROM runtime_assets", [], |r| r.get(0))?;
            let phase: String = db.query_row("SELECT json_extract(record,'$.phase') FROM runtime_workspace_operations WHERE id='op'", [], |r| r.get(0))?;
            assert_eq!(count, 0);
            assert_eq!(asset_count, 1);
            assert_eq!(phase, "provider_ready");
            Ok::<_, studio_runtime_store::StoreError>(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn legacy_branch_operation_catalog_and_settlement_preserve_evidence() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
        for (raw, signature, expected) in [
            (
                r#"{"signature":"s","phase":"completed","provider":false}"#,
                "s",
                Some(false),
            ),
            (
                r#"{"signature":"s","phase":{"completed":null}}"#,
                "s",
                Some(true),
            ),
            (r#"{"signature":"s"}"#, "s", Some(true)),
            (r#"{"signature":"other","phase":"completed"}"#, "s", None),
            ("null", "s", None),
        ] {
            let raw = raw.to_owned();
            store.transaction(move |tx| {
                tx.execute("INSERT OR REPLACE INTO runtime_workspace_operations(id,record) VALUES('branch:legacy',?1)", [raw])?;
                studio_runtime_store::migrations::migrate_branch_phase(tx)?;
                Ok(())
            }).await.unwrap();
            match expected {
                Some(expected) => assert_eq!(
                    branch_needs_catalog(&store, "branch:legacy", signature)
                        .await
                        .unwrap(),
                    expected
                ),
                None => assert!(
                    branch_needs_catalog(&store, "branch:legacy", signature)
                        .await
                        .is_err()
                ),
            }
        }
        store.transaction(|tx| {
            tx.execute("INSERT OR REPLACE INTO runtime_workspace_operations(id,record) VALUES('branch:legacy',?1)", [r#"{"signature":"s","agent":null,"provider":[1,null],"result":false,"vendor":{"opaque":true}}"#])?;
            studio_runtime_store::migrations::migrate_branch_phase(tx)?;
            Ok(())
        }).await.unwrap();
        settle_failure(&store, "branch:legacy", true, "lost")
            .await
            .unwrap();
        let raw: String = store
            .read_async("fixture", |db| {
                Ok::<_, studio_runtime_store::StoreError>(db.query_row(
                    "SELECT record FROM runtime_workspace_operations WHERE id='branch:legacy'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        let record: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(record["phase"], "recovery_required");
        assert_eq!(record["provider"], json!([1, null]));
        assert_eq!(record["result"], false);
        assert_eq!(record["vendor"]["opaque"], true);
    }

    #[tokio::test]
    async fn branch_catalog_refuses_unreconstructible_signature_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
        for raw in [
            r#"{"phase":"completed"}"#,
            r#"{"phase":"completed","signature":null}"#,
        ] {
            let expected = raw.to_owned();
            let stored = expected.clone();
            store.transaction(move |tx| {
                tx.execute("INSERT OR REPLACE INTO runtime_workspace_operations(id,record) VALUES('branch:legacy',?1)", [stored])?;
                Ok(())
            }).await.unwrap();
            assert!(
                branch_needs_catalog(&store, "branch:legacy", "s")
                    .await
                    .is_err()
            );
            let actual: String = store
                .read_async("branch witness", |db| {
                    Ok::<_, studio_runtime_store::StoreError>(db.query_row(
                        "SELECT record FROM runtime_workspace_operations WHERE id='branch:legacy'",
                        [],
                        |row| row.get(0),
                    )?)
                })
                .await
                .unwrap();
            assert_eq!(actual, expected);
        }
    }

    #[tokio::test]
    async fn fork_turn_refuses_unreconstructible_operation_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(root.path())).await.unwrap();
        let raw = r#"{"phase":"provider_pending","signature":null}"#;
        store.transaction(move |tx| {
            tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES('branch:legacy',?1)", [raw])?;
            Ok(())
        }).await.unwrap();
        assert!(
            persist_fork_turn(&store, "branch:legacy", Some("turn"))
                .await
                .is_err()
        );
        let actual: String = store
            .read_async("branch witness", |db| {
                Ok::<_, studio_runtime_store::StoreError>(db.query_row(
                    "SELECT record FROM runtime_workspace_operations WHERE id='branch:legacy'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(actual, raw);
    }

    #[test]
    fn branch_draft_uses_typed_input_identity_and_prefix() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        let tx = db.transaction().unwrap();
        let record = serde_json::from_str(r#"{"id":"agent:parent","role":"user","text":"parent","inputs":[{"id":"one","kind":"user","text":"first"},{"id":null,"kind":"user","text":"second"}]}"#).unwrap();
        let selected = studio_runtime_store::TranscriptItem {
            id: "agent:parent".into(),
            agent: "agent".into(),
            record,
        };
        let mut assets = BTreeMap::new();
        let payload = branch_draft_payload(
            &tx,
            "agent",
            "child",
            &selected,
            "agent:parent:1",
            &mut assets,
        )
        .unwrap();
        assert_eq!(payload.text, "second");
        assert_eq!(payload.prefix_text, "first");
        assert!(payload.assets.is_empty());
        let parent = branch_draft_payload(
            &tx,
            "agent",
            "child",
            &selected,
            "agent:parent",
            &mut assets,
        )
        .unwrap();
        assert_eq!(parent.text, "parent");
        assert_eq!(parent.prefix_text, "");
        assert!(parent.assets.is_empty());
    }

    #[test]
    fn branch_draft_keeps_the_historical_generated_request_identity() {
        for (source, historical_wire) in [
            (
                json!({"cwd":"/project", "yoloMode":true, "workspaceMode":"worktree", "profileId":"profile"}),
                r#"{"id":"child","name":"Lead (branch)","account_key":"account","cwd":"/project","model":"model","yolo_mode":true,"workspace_mode":"worktree","profile_id":"profile"}"#,
            ),
            (
                json!({"cwd":null, "yoloMode":null}),
                r#"{"id":"child","name":"Lead (branch)","account_key":"account","cwd":null,"model":"model","yolo_mode":null,"workspace_mode":null,"profile_id":null}"#,
            ),
        ] {
            let old: crate::generated::CreateLeadRequest =
                serde_json::from_str(historical_wire).unwrap();
            let typed =
                branch_draft_request(&source, "child".into(), "account".into(), "model").unwrap();
            assert_eq!(
                serde_json::to_value(old).unwrap(),
                serde_json::to_value(typed).unwrap()
            );
        }
    }

    async fn status_fixture() -> (tempfile::TempDir, RuntimeStore) {
        let root = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        store.transaction(|tx| {
            let source = AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","id":"source","rootId":"source","epoch":0,"status":"running","workspaceReservationId":"operation","provider":"codex"}))?;
            let child = AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","id":"child","rootId":"child","epoch":0,"status":"queued","provider":"codex"}))?;
            AgentRepository::put(tx, &source)?;
            AgentRepository::put(tx, &child)?;
            let operation = json!({"id":"operation","signature":"s","agent":"source","phase":"provider_pending","source":CheckpointsRepository::workspace_source(&source.value())});
            tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES('operation',?1)", [operation.to_string()])?;
            Ok::<_, studio_runtime_store::StoreError>(())
        }).await.expect("seed");
        (root, store)
    }

    #[tokio::test]
    async fn branch_source_rejects_unreconstructible_operation() {
        let (_root, store) = status_fixture().await;
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_workspace_operations SET record=json_set(record,'$.signature',NULL) WHERE id='operation'", [])?;
            Ok(())
        }).await.unwrap();
        assert!(
            ensure_branch_source(&store, "operation", "source")
                .await
                .is_err()
        );
    }

    fn assert_stored_status(store: &RuntimeStore, id: &str, expected: &str) {
        let db = store.test_reader().expect("reader");
        let (status, count): (String, i64) = db.query_row("SELECT json_extract(record,'$.status'), (SELECT count(*) FROM json_each(record) WHERE key='status') FROM runtime_agents WHERE id=?1", [id], |row| Ok((row.get(0)?, row.get(1)?))).expect("agent status");
        assert_eq!((status.as_str(), count), (expected, 1));
    }

    #[tokio::test]
    async fn unknown_fork_sets_one_interrupted_status() {
        let (_root, store) = status_fixture().await;
        settle_failure(&store, "operation", true, "unknown")
            .await
            .expect("settle");
        assert_stored_status(&store, "source", "interrupted");
    }

    #[tokio::test]
    async fn branch_copy_sets_one_idle_status() {
        let (_root, store) = status_fixture().await;
        let selected = studio_runtime_store::TranscriptItem {
            id: "message".into(),
            agent: "source".into(),
            record: serde_json::from_str(r#"{"id":"message","role":"assistant","turnId":"turn"}"#)
                .expect("selected transcript item"),
        };
        let provider = json!({"thread":{"id":"new-thread"}});
        let result = copy_branch_history(
            &store,
            BranchCopy {
                source: "source",
                target: "child",
                selected: &selected,
                message_id: "message",
                fork_turn: None,
                before: false,
                provider: &provider,
                operation_id: "operation",
            },
            Arc::new(tokio::sync::Notify::new()),
        )
        .await
        .expect("copy");
        assert_eq!(result["status"], "idle");
        assert_stored_status(&store, "child", "idle");
    }

    #[tokio::test]
    async fn branch_copy_refuses_unreconstructible_operation() {
        let (_root, store) = status_fixture().await;
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_workspace_operations SET record=json_set(record,'$.signature',NULL) WHERE id='operation'", [])?;
            Ok(())
        }).await.unwrap();
        let selected = studio_runtime_store::TranscriptItem {
            id: "message".into(),
            agent: "source".into(),
            record: serde_json::from_str(r#"{"id":"message","role":"assistant","turnId":"turn"}"#)
                .unwrap(),
        };
        let provider = json!({"thread":{"id":"new-thread"}});
        let result = copy_branch_history(
            &store,
            BranchCopy {
                source: "source",
                target: "child",
                selected: &selected,
                message_id: "message",
                fork_turn: None,
                before: false,
                provider: &provider,
                operation_id: "operation",
            },
            Arc::new(tokio::sync::Notify::new()),
        )
        .await;
        assert!(result.is_err());
        assert_stored_status(&store, "child", "queued");
    }

    #[tokio::test]
    async fn branch_copy_refuses_unreconstructible_transcript_row() {
        let (_root, store) = status_fixture().await;
        store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_items(id,agent,record,created) VALUES('source:legacy','source',?1,1)", [r#"{"turnId":"t","text":"legacy"}"#])?;
            Ok(())
        }).await.unwrap();
        let selected = studio_runtime_store::TranscriptItem {
            id: "source:legacy".into(),
            agent: "source".into(),
            record: serde_json::from_str(
                r#"{"id":"source:legacy","role":"assistant","turnId":"t"}"#,
            )
            .unwrap(),
        };
        let provider = json!({"thread":{"id":"new-thread"}});
        let result = copy_branch_history(
            &store,
            BranchCopy {
                source: "source",
                target: "child",
                selected: &selected,
                message_id: "source:legacy",
                fork_turn: Some("t"),
                before: false,
                provider: &provider,
                operation_id: "operation",
            },
            Arc::new(tokio::sync::Notify::new()),
        )
        .await;
        assert!(result.is_err());
        assert_stored_status(&store, "child", "queued");
    }

    const HISTORY_TURNS: usize = 100;
    const TURN_BODY_BYTES: usize = 4096;

    #[derive(Default)]
    struct HistoryPort {
        requests: Mutex<Vec<ProviderRequest>>,
        fork_effects: Mutex<HashSet<String>>,
        body_bytes: Mutex<usize>,
        live: std::sync::atomic::AtomicBool,
        catalog_unavailable: bool,
    }

    impl AccountSessionPort for HistoryPort {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            if !self.live.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            Some(ConnectionScope {
                account: account.into(),
                connection: "fixture".into(),
                generation: 1,
            })
        }
        fn ensure_session<'a>(
            &'a self,
            account: &'a str,
        ) -> AccountSessionFuture<'a, ConnectionScope> {
            Box::pin(async move {
                self.live.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(self.current_scope(account).expect("fixture scope"))
            })
        }
        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'a, SessionReceipt> {
            Box::pin(async move {
                self.requests
                    .lock()
                    .expect("requests")
                    .push(request.clone());
                let result = match request.method {
                    Method::ModelList => {
                        if self.catalog_unavailable {
                            return Err(AccountSessionError::Unknown);
                        }
                        json!({"data":[{"model":"gpt-5.6-sol"}],"nextCursor":null})
                    }
                    Method::ThreadTurnsList => {
                        let start = request.params["cursor"]
                            .as_str()
                            .map(|s| s.parse::<usize>().expect("cursor"))
                            .unwrap_or(0);
                        let end = (start
                            + request.params["limit"].as_u64().expect("limit") as usize)
                            .min(HISTORY_TURNS);
                        let full = request.params["itemsView"] != "notLoaded";
                        let data: Vec<_> = (start..end).map(|index| {
                            let items = if full {
                                *self.body_bytes.lock().expect("bytes") += TURN_BODY_BYTES;
                                json!([{"type":"agentMessage","text":"x".repeat(TURN_BODY_BYTES)}])
                            } else { json!([]) };
                            json!({"id":format!("turn-{}", HISTORY_TURNS-index),"items":items})
                        }).collect();
                        json!({"data":data,"nextCursor":(end < HISTORY_TURNS).then(|| end.to_string())})
                    }
                    Method::ThreadFork | Method::ThreadStart => {
                        self.fork_effects
                            .lock()
                            .unwrap()
                            .insert(request.operation.clone());
                        return Err(AccountSessionError::Unknown);
                    }
                    other @ Method::Initialize
                    | other @ Method::Initialized
                    | other @ Method::Ack
                    | other @ Method::AccountLoginCancel
                    | other @ Method::AccountLoginStart
                    | other @ Method::AccountRateLimitResetCreditConsume
                    | other @ Method::ThreadLoadedList
                    | other @ Method::ConfigRead
                    | other @ Method::SkillsExtraRootsSet
                    | other @ Method::AccountRead
                    | other @ Method::AccountRateLimitsRead
                    | other @ Method::ThreadRealtimeStart
                    | other @ Method::ThreadRealtimeStop
                    | other @ Method::ThreadRealtimeAppendSpeech
                    | other @ Method::ThreadRealtimeItemStarted
                    | other @ Method::ThreadRealtimeTranscriptDelta
                    | other @ Method::ThreadRealtimeTranscriptDone
                    | other @ Method::ThreadRealtimeSdp
                    | other @ Method::ThreadRealtimeError
                    | other @ Method::ThreadRealtimeClosed
                    | other @ Method::ThreadResume
                    | other @ Method::ThreadRead
                    | other @ Method::TurnStart
                    | other @ Method::TurnSteer
                    | other @ Method::TurnInterrupt
                    | other @ Method::ThreadList
                    | other @ Method::ThreadArchive
                    | other @ Method::ThreadNameSet
                    | other @ Method::ThreadMetadataUpdate
                    | other @ Method::ThreadSettingsUpdate
                    | other @ Method::ThreadCompactStart
                    | other @ Method::ThreadRollback
                    | other @ Method::ThreadItemsList
                    | other @ Method::ThreadGoalSet
                    | other @ Method::ThreadGoalUpdated
                    | other @ Method::ThreadUnarchive
                    | other @ Method::ThreadUnsubscribe
                    | other @ Method::ProcessSpawn
                    | other @ Method::ProcessWriteStdin
                    | other @ Method::ProcessKill
                    | other @ Method::ProcessResizePty
                    | other @ Method::CommandExec
                    | other @ Method::CommandExecWrite
                    | other @ Method::CommandExecResize
                    | other @ Method::CommandExecTerminate
                    | other @ Method::ThreadBackgroundTerminalsList
                    | other @ Method::ThreadBackgroundTerminalsTerminate
                    | other @ Method::ThreadBackgroundTerminalsClean
                    | other @ Method::ThreadCommandExecutionBackground
                    | other @ Method::ThreadQueueList
                    | other @ Method::ClaudeState
                    | other @ Method::ClaudeRepairReport
                    | other @ Method::ClaudeFileBackups
                    | other @ Method::ClaudeSettings
                    | other @ Method::ClaudeActivity
                    | other @ Method::ClaudeStopTask
                    | other @ Method::ClaudeShellPolicy
                    | other @ Method::ClaudeDiagnostics
                    | other @ Method::ClaudeMoveVersions
                    | other @ Method::McpServerReconnect
                    | other @ Method::McpServerStatusList
                    | other @ Method::SkillsList
                    | other @ Method::OperationStatus
                    | other @ Method::ReviewStart
                    | other @ Method::BrowserAnswer
                    | other @ Method::BrowserCallback
                    | other @ Method::BrowserClose
                    | other @ Method::BrowserWait
                    | other @ Method::ClaudeCommands
                    | other @ Method::ClaudeMoveExport
                    | other @ Method::ClaudeMoveIdentity
                    | other @ Method::ClaudeMovePreflight
                    | other @ Method::ClaudeMoveProof
                    | other @ Method::Detach
                    | other @ Method::EventRead
                    | other @ Method::Exec
                    | other @ Method::Generate
                    | other @ Method::Health
                    | other @ Method::Info
                    | other @ Method::LayrHealth
                    | other @ Method::Next
                    | other @ Method::Open
                    | other @ Method::ProjectEnsure
                    | other @ Method::ProjectExec
                    | other @ Method::ProjectImport
                    | other @ Method::ProviderList
                    | other @ Method::Replay
                    | other @ Method::ResponseStatus
                    | other @ Method::RetireCommand
                    | other @ Method::SecretsList
                    | other @ Method::SecretsRemove
                    | other @ Method::SecretsSet
                    | other @ Method::ShareConfigure
                    | other @ Method::ShareName
                    | other @ Method::ShareStatus
                    | other @ Method::Sign
                    | other @ Method::Status
                    | other @ Method::SyncMacApply
                    | other @ Method::SyncMacHandoff
                    | other @ Method::SyncMacHashes
                    | other @ Method::SyncMacHead
                    | other @ Method::SyncMacRead
                    | other @ Method::SyncMacWorktree
                    | other @ Method::VaultConfigure
                    | other @ Method::VaultPush
                    | other @ Method::VaultStatus
                    | other @ Method::UploadBegin
                    | other @ Method::UploadChunk
                    | other @ Method::UploadCommit
                    | other @ Method::Verify
                    | other @ Method::Write
                    | other @ Method::CurrentTimeRead
                    | other @ Method::ItemToolCall
                    | other @ Method::ItemCommandExecutionRequestApproval
                    | other @ Method::ItemFileChangeRequestApproval
                    | other @ Method::ItemPermissionsRequestApproval
                    | other @ Method::McpServerElicitationRequest
                    | other @ Method::ItemToolRequestUserInput
                    | other @ Method::ItemStarted
                    | other @ Method::ItemCompleted
                    | other @ Method::ItemAgentMessageDelta
                    | other @ Method::ItemReasoningTextDelta
                    | other @ Method::ItemCommandExecutionOutputDelta
                    | other @ Method::CommandExecOutputDelta
                    | other @ Method::TurnStarted
                    | other @ Method::TurnCompleted
                    | other @ Method::TurnPlanUpdated
                    | other @ Method::TurnDiffUpdated
                    | other @ Method::ThreadClosed
                    | other @ Method::ThreadStatusChanged
                    | other @ Method::ThreadTokenUsageUpdated
                    | other @ Method::ThreadShellPolicyUpdated
                    | other @ Method::ThreadIdleCompactionUpdated
                    | other @ Method::AccountLoginCompleted
                    | other @ Method::AccountUpdated
                    | other @ Method::AccountRateLimitsUpdated
                    | other @ Method::ModelSafetyBufferingUpdated
                    | other @ Method::ProviderGenerationStarted
                    | other @ Method::ProtocolError => panic!("unexpected method {other:?}"),
                };
                Ok(SessionReceipt {
                    scope: self.current_scope(&request.account).expect("scope"),
                    operation: request.operation,
                    replayed: false,
                    outcome: ReceiptOutcome::Written {
                        ticket: studio_runtime_providers::RequestTicket {
                            generation: 1,
                            id: studio_runtime_providers::RequestId::String("fixture".into()),
                            method: request.method,
                        },
                        response: ProviderResponse::Reply(result),
                    },
                })
            })
        }
    }

    async fn lookup(
        selected: usize,
        before: bool,
        catalog_unavailable: bool,
    ) -> (Arc<HistoryPort>, Value, ApiError) {
        Box::pin(lookup_with_retry(
            selected,
            before,
            catalog_unavailable,
            false,
        ))
        .await
    }

    async fn lookup_with_retry(
        selected: usize,
        before: bool,
        catalog_unavailable: bool,
        retry: bool,
    ) -> (Arc<HistoryPort>, Value, ApiError) {
        let root = tempfile::tempdir().expect("state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let cwd = root.path().display().to_string();
        store
            .transaction(move |tx| {
                AgentRepository::put(
                    tx,
                    &AgentRecord::from_value(
                        json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,
                            "id":"source","rootId":"source","epoch":0,"status":"idle",
                            "cwd":cwd,"threadId":"thread","isLead":true,"model":"gpt-5.6-sol"
                        }),
                    )?,
                )?;
                TranscriptRepository::put_item(
                    tx,
                    "message",
                    "source",
                    &json!({
                        "id":"message","role":"assistant","text":"message","turnId":format!("turn-{selected}")
                    }),
                    1.0,
                    "message",
                    false,
                )
                .expect("transcript fixture");
                Ok(())
            })
            .await
            .expect("seed");
        let policy = Arc::new(studio_runtime_services::account_policy::AccountPolicyState::from_startup(
            Some(serde_json::json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"defaultAccountKey":"default","accounts":{"default":{"provider":studio_runtime_schema::ProviderKind::Codex,"id":"default","home":"fixture","label":"Default","source":"Codex CLI","status":"signedOut"}},"logins":{}}).to_string().as_bytes()), []
        ).expect("policy"));
        let mut state = HttpState::new(
            "token".into(),
            "fixture".into(),
            root.path().display().to_string(),
            4000,
            None,
            Some(store.clone()),
            Some(policy),
        );
        let port = Arc::new(HistoryPort {
            catalog_unavailable,
            ..Default::default()
        });
        state.account_session = Some(port.clone());
        let result = branch(
            State(state.clone()),
            ApiJson(
                serde_json::from_value(json!({
                    "id":"lookup","agent":"source","message_id":"message","before":before
                }))
                .expect("request"),
            ),
        )
        .await;
        let error = result.expect_err("fixture leaves catalog unavailable or fork outcome unknown");
        if retry {
            if selected == 0 {
                store.transaction(|tx| {
                    let raw: String = tx.query_row("SELECT record FROM runtime_workspace_operations WHERE id='branch:lookup'", [], |row| row.get(0))?;
                    let mut operation: Value = serde_json::from_str(&raw)?;
                    operation["provider"] = json!(["old"]);
                    operation["result"] = json!({"old":true});
                    operation["vendor"] = json!({"retained":true});
                    tx.execute("UPDATE runtime_workspace_operations SET record=?1 WHERE id='branch:lookup'", [operation.to_string()])?;
                    Ok(())
                }).await.unwrap();
            }
            let retried = branch(
                State(state.clone()),
                ApiJson(
                    serde_json::from_str(&format!(
                        r#"{{"id":"lookup","agent":"source","message_id":"message","before":{before}}}"#
                    ))
                    .expect("request"),
                ),
            )
            .await;
            assert!(retried.is_err());
        }
        let operation = store
            .read_async("branch witness", |db| {
                let raw: String = db
                    .query_row(
                        "SELECT record FROM runtime_workspace_operations WHERE id='branch:lookup'",
                        [],
                        |row| row.get(0),
                    )
                    .expect("operation");
                serde_json::from_str::<Value>(&raw).map_err(studio_runtime_store::StoreError::from)
            })
            .await
            .expect("operation read");
        state
            .model_catalog
            .shutdown()
            .await
            .expect("catalog shutdown");
        (port, operation, error)
    }

    #[tokio::test]
    async fn branch_retry_keeps_unknown_broker_identity() {
        let (port, operation, _) = Box::pin(lookup_with_retry(81, false, false, true)).await;
        assert_eq!(operation["phase"], "recovery_required");
        let requests = port.requests.lock().unwrap();
        let forks: Vec<_> = requests
            .iter()
            .filter(|r| r.method == Method::ThreadFork)
            .collect();
        assert_eq!(forks.len(), 2);
        assert_eq!(forks[0].operation, forks[1].operation);
        assert_eq!(forks[0].params, forks[1].params);
        assert_eq!(port.fork_effects.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn branch_failed_retry_clears_old_evidence_only() {
        let (port, operation, _) = Box::pin(lookup_with_retry(0, true, false, true)).await;
        assert_eq!(operation["phase"], "failed");
        assert!(operation.get("provider").is_none());
        assert!(operation.get("result").is_none());
        assert_eq!(operation["vendor"]["retained"], true);
        assert!(port.fork_effects.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn branch_before_reads_no_turn_bodies_and_stops_after_predecessor() {
        let (port, operation, _) = lookup(81, true, false).await;
        assert_eq!(
            *port.body_bytes.lock().expect("bytes"),
            0,
            "branch lookup must decode no item bodies"
        );
        let requests = port.requests.lock().expect("requests");
        let pages: Vec<_> = requests
            .iter()
            .filter(|r| r.method == Method::ThreadTurnsList)
            .collect();
        assert_eq!(pages.len(), 3, "predecessor crosses a page boundary");
        assert_eq!(pages[0].operation, "branch:lookup:turns:0");
        assert_eq!(pages[1].operation, "branch:lookup:turns:1");
        assert_eq!(pages[2].operation, "branch:lookup:turns:2");
        assert_eq!(operation["forkTurnId"], "turn-80");
        assert_eq!(operation["phase"], "recovery_required");
        assert_eq!(
            requests.last().expect("fork").params["lastTurnId"],
            "turn-80"
        );
    }
    #[tokio::test]
    async fn branch_before_handles_same_page_first_and_missing_turns() {
        for (selected, predecessor, pages, expected_method) in [
            (95, Some("turn-94"), 1, Some(Method::ThreadFork)),
            (1, None, 10, Some(Method::ThreadStart)),
            (0, None, 10, None),
        ] {
            let (port, operation, _) = lookup(selected, true, false).await;
            assert_eq!(*port.body_bytes.lock().expect("bytes"), 0);
            let requests = port.requests.lock().expect("requests");
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| r.method == Method::ThreadTurnsList)
                    .count(),
                pages
            );
            if selected == 0 {
                assert_eq!(operation["phase"], "failed");
                assert!(
                    requests
                        .iter()
                        .all(|r| !matches!(r.method, Method::ThreadFork | Method::ThreadStart))
                );
            } else {
                assert_eq!(
                    operation["forkTurnId"],
                    predecessor.map_or(Value::Null, |id| json!(id))
                );
                assert_eq!(
                    requests.last().expect("branch").method,
                    expected_method.expect("branch method")
                );
            }
        }
    }

    #[tokio::test]
    async fn branch_after_uses_selected_turn_without_history_read() {
        let (port, operation, _) = lookup(81, false, false).await;
        assert_eq!(operation["forkTurnId"], "turn-81");
        let requests = port.requests.lock().expect("requests");
        assert!(requests.iter().all(|r| r.method != Method::ThreadTurnsList));
        assert_eq!(
            requests.last().expect("fork").params["lastTurnId"],
            "turn-81"
        );
    }

    async fn error_response(error: ApiError) -> (StatusCode, Value) {
        let response = axum::response::IntoResponse::into_response(error);
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("error body");
        (status, serde_json::from_slice(&bytes).expect("error JSON"))
    }

    #[tokio::test]
    async fn branch_catalog_unavailable_returns_service_failure_without_a_native_fork() {
        let (port, operation, error) = lookup(81, false, true).await;
        let (status, body) = error_response(error).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("temporarily unavailable"),
            "{body}"
        );
        assert_eq!(operation["phase"], "provider_pending");
        let requests = port.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, Method::ModelList);
    }

    #[derive(Default)]
    struct LoadingCatalogPort {
        ensuring: tokio::sync::Notify,
        release: tokio::sync::Notify,
        requested: tokio::sync::Notify,
    }

    impl AccountSessionPort for LoadingCatalogPort {
        fn ensure_session<'a>(
            &'a self,
            account: &'a str,
        ) -> AccountSessionFuture<'a, ConnectionScope> {
            Box::pin(async move {
                self.ensuring.notify_one();
                self.release.notified().await;
                Ok(ConnectionScope {
                    account: account.into(),
                    connection: "loading".into(),
                    generation: 1,
                })
            })
        }

        fn provider_request(
            &self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'_, SessionReceipt> {
            assert_eq!(request.method, Method::ModelList);
            self.requested.notify_one();
            Box::pin(std::future::pending())
        }
    }

    fn catalog_state() -> HttpState {
        let policy = Arc::new(studio_runtime_services::account_policy::AccountPolicyState::from_startup(
            Some(br#"{"version":2,"defaultAccountKey":"default","accounts":{"default":{"provider":"codex","id":"default","home":"fixture","label":"Default","source":"Codex CLI","status":"signedOut"},"changed":{"provider":"codex","id":"changed","home":"fixture","label":"Changed","source":"Codex CLI","status":"changed"}},"logins":{}}"#), []
        ).expect("policy"));
        HttpState::new(
            "token".into(),
            "fixture".into(),
            "/fixture".into(),
            4000,
            None,
            None,
            Some(policy),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn branch_catalog_pending_returns_its_loading_error_after_the_bounded_wait() {
        let mut state = catalog_state();
        let port = Arc::new(LoadingCatalogPort::default());
        state.account_session = Some(port.clone());
        let waiting = state.clone();
        let mut request =
            tokio::spawn(async move { ensure_branch_model_catalog(&waiting, "default").await });
        port.ensuring.notified().await;
        // The admission timer starts before the provider request timer. Keep
        // their deadlines separate so this proves Pending, not a provider fault.
        tokio::time::advance(Duration::from_secs(1)).await;
        port.release.notify_one();
        port.requested.notified().await;
        tokio::time::advance(Duration::from_secs(
            studio_runtime_schema::shared_values::MODEL_CATALOG_ADMISSION_WAIT_SECONDS - 1,
        ))
        .await;
        let bounded = tokio::time::timeout(Duration::from_millis(1), &mut request).await;
        if bounded.is_err() {
            request.abort();
            let _cancelled = request.await;
        }
        state
            .model_catalog
            .shutdown()
            .await
            .expect("catalog shutdown");
        let error = bounded
            .expect("branch must return within the model admission budget")
            .expect("catalog read")
            .expect_err("catalog is still pending");
        let (status, body) = error_response(error).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("still loading"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn branch_catalog_preserves_account_validation_errors() {
        let mut state = catalog_state();
        let port = Arc::new(HistoryPort::default());
        state.account_session = Some(port.clone());
        for (account, message) in [
            ("missing", "Unknown Codex account"),
            ("changed", "Account is unavailable"),
        ] {
            let error = ensure_branch_model_catalog(&state, account)
                .await
                .expect_err("invalid account");
            let (status, body) = error_response(error).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["error"], message);
        }
        assert!(port.requests.lock().expect("requests").is_empty());
        state
            .model_catalog
            .shutdown()
            .await
            .expect("catalog shutdown");
    }
}
