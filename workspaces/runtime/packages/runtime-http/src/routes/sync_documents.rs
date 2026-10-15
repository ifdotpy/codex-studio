//! HTTP adapter for the durable sync document operations.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use studio_runtime_schema::shared_values::SYNC_STREAM_BYTE_LIMIT;
use studio_runtime_schema::{SYNC_ENTITY_SCOPE, SYNC_PROTOCOL_VERSION};
use studio_runtime_services::sync_documents::{self, DraftWrite};
use studio_runtime_services::sync_policy::{self, PreferenceScope, SyncPolicyError};
use studio_runtime_services::transcript;
use studio_runtime_store::Transaction;
use studio_runtime_store::{RuntimeStore, StoreError, SyncDocumentsRepository};

use crate::error::ApiError;
use crate::router::HttpState;

const ENTITY_PROJECTION_PREPARATION_MAX_ATTEMPTS: usize = 3;
const ENTITY_PROJECTION_PREPARATION_CONFLICT: &str =
    "Entity projection inputs kept changing while preparing the sync response; retry the pull";

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/sync/protocol", OperationStatus::Implemented, protocol),
        get("/api/sync/pull", OperationStatus::Implemented, pull),
        post(
            "/api/sync/drafts",
            OperationStatus::Implemented,
            push_drafts,
        ),
        post(
            "/api/sync/preferences",
            OperationStatus::Implemented,
            push_preferences,
        ),
    ])
}

pub(crate) async fn protocol(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::SyncProtocolResponse>, ApiError> {
    let socket_path = PathBuf::from(state.state_dir).join("canvas.sock");
    let unix_socket = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/sync_documents",
        move || socket_path.exists(),
    )
    .await
    .map_err(|_| ApiError::internal())?;
    let mut capabilities = vec![
        "pull",
        "stream",
        "streamChanges",
        "entityReset",
        "typedResources",
        "tokenRates",
        "entitySeedExcluded:canvasAgentThreads",
        "entitySeedExcluded:canvasSpawnEdges",
    ];
    if unix_socket {
        capabilities.push("unixSocket");
    }
    let response = json!({
        "protocolVersion": SYNC_PROTOCOL_VERSION,
        "supportedVersions": [SYNC_PROTOCOL_VERSION],
        "capabilities": capabilities,
        "scopes": [SYNC_ENTITY_SCOPE, "transcript:<agent-id>", "drafts"],
        "pullEndpoint": "/api/sync/pull",
        "streamEndpoint": "/api/sync/stream",
        "maxEntityPage": sync_policy::ENTITY_PAGE_LIMIT,
        "maxOtherPage": sync_policy::OTHER_PAGE_LIMIT,
        "maxStreamDocuments": sync_policy::OTHER_PAGE_LIMIT,
        "maxStreamBytes": SYNC_STREAM_BYTE_LIMIT,
    });
    Ok(Json(super::generated_json::<
        crate::generated::SyncProtocolResponse,
    >(response)?))
}

pub(crate) async fn pull(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<SyncPullResponseBody>, ApiError> {
    let scope = query
        .typed_value("scope")
        .and_then(Value::as_str)
        .unwrap_or(SYNC_ENTITY_SCOPE)
        .to_owned();
    if scope != "drafts"
        && scope != SYNC_ENTITY_SCOPE
        && !(scope.starts_with("transcript:") && scope.chars().count() < 300)
    {
        return Err(ApiError::validation(vec![
            Value::String("query".to_owned()),
            Value::String("scope".to_owned()),
        ]));
    }
    let after = query_integer(&query, "after", 0)?.max(0);
    let limit = sync_policy::effective_limit(
        &scope,
        query_integer(&query, "limit", sync_policy::OTHER_PAGE_LIMIT)?,
    );
    let fresh = query.typed_value("fresh").and_then(Value::as_str) == Some("1");
    let initial_high = query_integer(&query, "initialHigh", 0)?.max(0);
    let reset_support = query.typed_value("reset").and_then(Value::as_str) == Some("1");
    let priority_id = query
        .typed_value("priorityId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let state_dir = state.state_dir.clone();
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/sync_documents",
        move || -> Result<SyncPullResponseBody, ApiError> {
            let generation = store.generation().map_err(|_| ApiError::internal())?;
            if let Some(agent) = scope.strip_prefix("transcript:") {
                let mut response = transcript::pull_scope(&store, &scope, agent, after)
                    .map_err(super::transcript::map_transcript_error)?;
                // Python samples generation after transcript maintenance commits.
                response["generation"] =
                    json!(store.generation().map_err(|_| ApiError::internal())?);
                if !validators.validate_stored("SyncPullResponse", &response) {
                    return Err(invalid_entity("sync_entities", "transcript-pull-response").into());
                }
                return Ok(SyncPullResponseBody::Page(super::generated_json::<
                    crate::generated::SyncPullResponse,
                >(response)?));
            }
            if scope == "drafts" {
                let connection = store.read().map_err(|_| ApiError::internal())?;
                let workspace_id = SyncDocumentsRepository::workspace_id(&connection)
                    .map_err(|_| ApiError::internal())?;
                let documents = SyncDocumentsRepository::pull_drafts(&connection, after, limit)
                    .map_err(|_| ApiError::internal())?;
                for document in &documents {
                    if sync_policy::contains_unrepresentable_integer(&document.payload) {
                        return Err(invalid_stored_draft(&document.id));
                    }
                    let payload: Value = serde_json::from_str(&document.payload)
                        .map_err(|_| invalid_stored_draft(&document.id))?;
                    validate_draft_value(&validators, &document.id, &payload)
                        .map_err(|_| invalid_stored_draft(&document.id))?;
                }
                let checkpoint = documents.last().map_or(after, |document| document.sequence);
                let response = crate::generated::SyncPullResponse {
                    sync_entities: None,
                    sync_entities_after: None,
                    checkpoint: crate::generated::SyncCheckpoint { seq: checkpoint },
                    documents: documents
                        .into_iter()
                        .map(|document| crate::generated::SyncDocument {
                            id: document.id,
                            payload: document.payload,
                            seq: document.sequence,
                            deleted: document.deleted,
                        })
                        .collect(),
                    floor: None,
                    generation,
                    initial_high: None,
                    max_seq: None,
                    reset: None,
                    workspace_id,
                };
                if !validate_generated("SyncPullResponse", &response, &validators) {
                    return Err(invalid_stored_draft("page"));
                }
                return Ok(SyncPullResponseBody::Page(response));
            }
            let generation_before_vault_read = store.committed_change_generation();
            let (_, vault_snapshot) = capture_project_vault_snapshot(&store, &state_dir)?;
            let vault_signature = vault_snapshot.signature();
            drop(vault_snapshot);
            let observed_generation = store.committed_change_generation();
            if generation_before_vault_read != observed_generation
                || store
                    .entity_projections_need_maintenance(&vault_signature)
                    .map_err(map_sync_store_error)?
            {
                return pull_with_maintenance(
                    &store,
                    &state_dir,
                    &validators,
                    after,
                    limit,
                    fresh,
                    initial_high,
                    reset_support,
                    priority_id.as_deref(),
                    generation,
                );
            }
            let read_result = store.read_transaction(|transaction| {
                pull_entities(
                    transaction,
                    &state_dir,
                    false,
                    None,
                    None,
                    None,
                    after,
                    limit,
                    fresh,
                    initial_high,
                    reset_support,
                    priority_id.as_deref(),
                    generation,
                    &validators,
                )
            });
            match read_result {
                Ok(response) if store.committed_change_generation() == observed_generation => {
                    Ok(response)
                }
                Ok(_) => pull_with_maintenance(
                    &store,
                    &state_dir,
                    &validators,
                    after,
                    limit,
                    fresh,
                    initial_high,
                    reset_support,
                    priority_id.as_deref(),
                    generation,
                ),
                Err(StoreError::Sqlite(studio_runtime_store::SqliteError::SqliteFailure(
                    error,
                    _,
                ))) if error.code == studio_runtime_store::ErrorCode::ReadOnly => {
                    let (response, _) = store
                        .transaction_with_commit_generation(|transaction| {
                            if store.committed_change_generation() != observed_generation
                                || store.entity_projections_need_maintenance(&vault_signature)?
                            {
                                return Ok(None);
                            }
                            pull_entities(
                                transaction,
                                &state_dir,
                                false,
                                None,
                                None,
                                None,
                                after,
                                limit,
                                fresh,
                                initial_high,
                                reset_support,
                                priority_id.as_deref(),
                                generation,
                                &validators,
                            )
                            .map(Some)
                        })
                        .map_err(map_sync_store_error)?;
                    match response {
                        Some(response) => Ok(response),
                        None => pull_with_maintenance(
                            &store,
                            &state_dir,
                            &validators,
                            after,
                            limit,
                            fresh,
                            initial_high,
                            reset_support,
                            priority_id.as_deref(),
                            generation,
                        ),
                    }
                }
                Err(error) => Err(map_sync_store_error(error)),
            }
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

#[allow(clippy::too_many_arguments)]
fn pull_with_maintenance(
    store: &RuntimeStore,
    state_dir: &str,
    validators: &crate::validation::Registry,
    after: i64,
    limit: i64,
    fresh: bool,
    initial_high: i64,
    reset_support: bool,
    priority_id: Option<&str>,
    generation: i64,
) -> Result<SyncPullResponseBody, ApiError> {
    pull_with_maintenance_interleaved(
        store,
        state_dir,
        validators,
        after,
        limit,
        fresh,
        initial_high,
        reset_support,
        priority_id,
        generation,
        &mut || {},
    )
}

#[allow(clippy::too_many_arguments)]
fn pull_with_maintenance_interleaved(
    store: &RuntimeStore,
    state_dir: &str,
    validators: &crate::validation::Registry,
    after: i64,
    limit: i64,
    fresh: bool,
    initial_high: i64,
    reset_support: bool,
    priority_id: Option<&str>,
    generation: i64,
    before_room_recheck: &mut dyn FnMut(),
) -> Result<SyncPullResponseBody, ApiError> {
    for _ in 0..ENTITY_PROJECTION_PREPARATION_MAX_ATTEMPTS {
        let maintenance_lock = store
            .lock_entity_projection_maintenance()
            .map_err(map_sync_store_error)?;
        let generation_before_vault_read = store.committed_change_generation();
        let (reader, vault_snapshot) = capture_project_vault_snapshot(store, state_dir)?;
        let vault_signature = vault_snapshot.signature();
        let observed_generation = store.committed_change_generation();
        if generation_before_vault_read == observed_generation
            && !store
                .entity_projections_need_maintenance(&vault_signature)
                .map_err(map_sync_store_error)?
        {
            drop(reader);
            let response = store
                .read_transaction(|transaction| {
                    pull_entities(
                        transaction,
                        state_dir,
                        false,
                        None,
                        None,
                        None,
                        after,
                        limit,
                        fresh,
                        initial_high,
                        reset_support,
                        priority_id,
                        generation,
                        validators,
                    )
                })
                .map_err(map_sync_store_error)?;
            drop(maintenance_lock);
            if store.committed_change_generation() == observed_generation {
                return Ok(response);
            }
            continue;
        }

        if generation_before_vault_read == observed_generation
            && sync_policy::has_complete_entity_seed(&reader).map_err(map_sync_store_error)?
            && vault_snapshot
                .matches_entity_projections(&reader)
                .map_err(map_sync_store_error)?
        {
            drop(reader);
            store
                .mark_entity_projections_maintained(observed_generation, vault_signature)
                .map_err(map_sync_store_error)?;
            let response = store
                .read_transaction(|transaction| {
                    pull_entities(
                        transaction,
                        state_dir,
                        false,
                        None,
                        None,
                        None,
                        after,
                        limit,
                        fresh,
                        initial_high,
                        reset_support,
                        priority_id,
                        generation,
                        validators,
                    )
                })
                .map_err(map_sync_store_error)?;
            drop(maintenance_lock);
            if store.committed_change_generation() == observed_generation {
                return Ok(response);
            }
            continue;
        }

        let project_server_id = store.local_server_id().map_err(map_sync_store_error)?;
        let rooms =
            studio_runtime_services::sync_entity_context::RoomDirectory::prepare_for_sync(&reader)
                .map_err(map_sync_store_error)?;
        drop(reader);
        before_room_recheck();
        let (response, committed_generation) = store
            .transaction_with_commit_generation(|transaction| {
                if !rooms.matches_for_sync(transaction)? {
                    return Ok(None);
                }
                pull_entities(
                    transaction,
                    state_dir,
                    true,
                    project_server_id.as_deref(),
                    Some(&vault_snapshot),
                    Some(&rooms),
                    after,
                    limit,
                    fresh,
                    initial_high,
                    reset_support,
                    priority_id,
                    generation,
                    validators,
                )
                .map(Some)
            })
            .map_err(map_sync_store_error)?;
        let Some(response) = response else {
            drop(maintenance_lock);
            continue;
        };
        store
            .mark_entity_projections_maintained(committed_generation, vault_signature)
            .map_err(map_sync_store_error)?;
        drop(maintenance_lock);
        return Ok(response);
    }

    Err(ApiError::new(
        StatusCode::CONFLICT,
        ENTITY_PROJECTION_PREPARATION_CONFLICT,
    ))
}

fn capture_project_vault_snapshot(
    store: &RuntimeStore,
    state_dir: &str,
) -> Result<
    (
        studio_runtime_store::ReadConnection,
        sync_policy::ProjectVaultSnapshot,
    ),
    ApiError,
> {
    let reader = store.read().map_err(map_sync_store_error)?;
    let snapshot = sync_policy::ProjectVaultSnapshot::capture_for_sync(
        std::path::Path::new(state_dir),
        &reader,
    )
    .map_err(map_sync_store_error)?;
    Ok((reader, snapshot))
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub(crate) enum SyncPullResponseBody {
    Reset(crate::generated::SyncPullResetResponse),
    Page(crate::generated::SyncPullResponse),
}

#[derive(Debug, Deserialize)]
pub(crate) struct PreferencePushRequest {
    scope: String,
    #[serde(default)]
    fields: serde_json::Map<String, Value>,
}

pub(crate) async fn push_drafts(
    State(state): State<HttpState>,
    ApiJson(parsed): ApiJson<crate::generated::DraftPushRequest>,
) -> Result<Json<Vec<crate::generated::SyncDocument>>, ApiError> {
    let validators = state.validators.clone();
    let mut writes = Vec::with_capacity(parsed.rows.len());
    for (index, row) in parsed.rows.into_iter().enumerate() {
        if sync_policy::contains_unrepresentable_integer(&row.new_document_state.payload) {
            return Err(draft_validation_error(index, "newDocumentState", "payload"));
        }
        let payload: Value = serde_json::from_str(&row.new_document_state.payload)
            .map_err(|_| draft_validation_error(index, "newDocumentState", "payload"))?;
        if !validators.validate_stored("DraftPayload", &payload) {
            return Err(draft_validation_error(index, "newDocumentState", "payload"));
        }
        sync_policy::validate_draft(&row.new_document_state.id, &payload)
            .map_err(|_| draft_validation_error(index, "newDocumentState", "id"))?;
        let assumed = row
            .assumed_master_state
            .flatten()
            .map(|assumed| {
                if sync_policy::contains_unrepresentable_integer(&assumed.payload) {
                    return Err(draft_validation_error(
                        index,
                        "assumedMasterState",
                        "payload",
                    ));
                }
                let payload: Value = serde_json::from_str(&assumed.payload)
                    .map_err(|_| draft_validation_error(index, "assumedMasterState", "payload"))?;
                if !validators.validate_stored("DraftPayload", &payload) {
                    return Err(draft_validation_error(
                        index,
                        "assumedMasterState",
                        "payload",
                    ));
                }
                sync_policy::validate_draft(&row.new_document_state.id, &payload)
                    .map_err(|_| draft_validation_error(index, "assumedMasterState", "payload"))?;
                Ok::<_, ApiError>((payload, assumed.deleted.unwrap_or(false)))
            })
            .transpose()?;
        writes.push(DraftWrite {
            id: row.new_document_state.id,
            payload,
            deleted: row.new_document_state.deleted.unwrap_or(false),
            assumed_payload: assumed.as_ref().map(|(payload, _)| payload.clone()),
            assumed_deleted: assumed.is_some_and(|(_, deleted)| deleted),
        });
    }
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let wake = state.event_wake.clone();
    let validators = validators.clone();
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/sync_documents",
        move || {
            let result = sync_documents::push_drafts(&store, &writes, |payload| {
                validators.validate_stored("DraftPayload", payload)
            })
            .map_err(|error| match error {
                sync_documents::DraftPushError::InvalidStored { id } => invalid_stored_draft(&id),
                sync_documents::DraftPushError::Store(_) => ApiError::internal(),
            })?;
            if result.changed {
                wake.notify_one();
            }
            Ok::<_, ApiError>(result.conflicts)
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(super::generated_json::<
        Vec<crate::generated::SyncDocument>,
    >(Value::Array(
        result.iter().map(|document| document.response()).collect(),
    ))?))
}

pub(crate) async fn push_preferences(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<PreferencePushRequest>,
) -> Result<Json<Value>, ApiError> {
    let scope = if body.scope == "user" {
        PreferenceScope::User
    } else {
        PreferenceScope::Server
    };
    let fields = sync_policy::parse_preferences(&Value::Object(body.fields), scope)
        .map_err(|_| ApiError::validation(vec![Value::String("body".to_owned())]))?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let merged = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/sync_documents",
        move || {
            sync_policy::push_preferences(&store, scope, &fields, move |field| {
                validators.validate_preference_field(field)
            })
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|error| match error {
        SyncPolicyError::InvalidPreferences => {
            ApiError::validation(vec![Value::String("body".to_owned())])
        }
        SyncPolicyError::InvalidStoredObject { .. } => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
        _ => ApiError::internal(),
    })?;
    Ok(Json(json!({"fields": merged})))
}

#[allow(clippy::too_many_arguments)]
fn pull_entities(
    transaction: &Transaction<'_>,
    state_dir: &str,
    maintain_projections: bool,
    project_server_id: Option<&str>,
    vault_snapshot: Option<&sync_policy::ProjectVaultSnapshot>,
    rooms: Option<&studio_runtime_services::sync_entity_context::RoomDirectory>,
    after: i64,
    limit: i64,
    fresh: bool,
    initial_high: i64,
    reset_support: bool,
    priority_id: Option<&str>,
    generation: i64,
    validators: &crate::validation::Registry,
) -> Result<SyncPullResponseBody, StoreError> {
    if maintain_projections {
        let vault_snapshot = vault_snapshot.ok_or(StoreError::RecordContract(
            "entity projection maintenance is missing its vault snapshot",
        ))?;
        let rooms = rooms.ok_or(StoreError::RecordContract(
            "entity projection maintenance is missing its room directory",
        ))?;
        sync_policy::seed_workspace_projection(
            transaction,
            state_dir,
            |collection, id, value, deleted| {
                validate_entity_envelope(
                    &json!({"collection":collection,"id":id,"value":value}),
                    collection,
                    id,
                    deleted,
                    validators,
                )
            },
        )?;
        sync_policy::maintain_entity_projections_with_server_id(
            transaction,
            state_dir,
            project_server_id,
            vault_snapshot,
            rooms,
            |collection, id, value, deleted| {
                validate_entity_envelope(
                    &json!({"collection":collection,"id":id,"value":value}),
                    collection,
                    id,
                    deleted,
                    validators,
                )
            },
        )?;
    }
    let mut page = SyncDocumentsRepository::pull_entities(
        transaction,
        after,
        limit,
        fresh,
        initial_high,
        reset_support,
        priority_id,
    )?;
    let workspace_id = SyncDocumentsRepository::workspace_id(transaction)?;
    if page.reset {
        let response = crate::generated::SyncPullResetResponse {
            sync_entities: None,
            sync_entities_after: None,
            floor: page.tombstone_floor,
            generation,
            max_seq: page.max_sequence,
            reset: true,
            workspace_id,
        };
        if !validate_generated("SyncPullResetResponse", &response, validators) {
            return Err(invalid_entity("sync_entities", "pull-response"));
        }
        return Ok(SyncPullResponseBody::Reset(response));
    }
    let initial_high = page.initial_high;
    let max_sequence = page.max_sequence;
    let page_limit = usize::try_from(limit.clamp(
        1,
        studio_runtime_schema::shared_values::SYNC_ENTITY_PAGE_LIMIT,
    ))
    .unwrap_or(1);
    let mut documents = Vec::with_capacity(page_limit);
    let mut seen_sequences = std::collections::HashSet::new();
    let mut checkpoint = after;
    let mut scan_after = after;
    loop {
        let raw_count = page.rows.len();
        let mut reached_limit = false;
        for row in &page.rows {
            checkpoint = row.sequence;
            if !seen_sequences.insert(row.sequence) {
                continue;
            }
            let Some(raw) = row.payload.as_deref() else {
                continue;
            };
            let Ok(mut payload) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            let Some(schema) = entity_payload_schema(&row.collection) else {
                continue;
            };
            if !readable_entity_envelope(&payload, &row.collection, &row.id) {
                continue;
            }
            if row.deleted {
                if let Some(object) = payload.as_object_mut() {
                    object.retain(|key, _| matches!(key.as_str(), "collection" | "id" | "value"));
                    object.insert("value".to_owned(), json!({}));
                }
            } else {
                if let Some(object) = payload.as_object_mut() {
                    object.retain(|key, _| matches!(key.as_str(), "collection" | "id" | "value"));
                } else {
                    continue;
                }
                validators.strip_stored_extra_fields(schema, &mut payload);
            }
            let Ok(payload) = serde_json::to_string(&payload) else {
                continue;
            };
            documents.push(crate::generated::SyncDocument {
                id: format!("entity:{}:{}", row.collection, row.id),
                payload,
                seq: row.sequence,
                deleted: row.deleted,
            });
            if documents.len() == page_limit {
                reached_limit = true;
                break;
            }
        }
        if reached_limit {
            break;
        }
        if raw_count < page_limit {
            checkpoint = page.checkpoint;
            break;
        }
        let next_after = page.rows.last().map_or(checkpoint, |row| row.sequence);
        if next_after <= scan_after {
            checkpoint = max_sequence;
            break;
        }
        scan_after = next_after;
        page = SyncDocumentsRepository::pull_entities(
            transaction,
            next_after,
            limit,
            fresh,
            initial_high,
            reset_support,
            None,
        )?;
        if page.rows.is_empty() {
            checkpoint = page.checkpoint;
            break;
        }
        checkpoint = next_after;
    }
    let response = crate::generated::SyncPullResponse {
        sync_entities: None,
        sync_entities_after: None,
        checkpoint: crate::generated::SyncCheckpoint { seq: checkpoint },
        documents,
        floor: None,
        generation,
        initial_high: Some(Some(initial_high)),
        max_seq: Some(Some(max_sequence)),
        reset: None,
        workspace_id,
    };
    if !validate_generated("SyncPullResponse", &response, validators) {
        return Err(invalid_entity("sync_entities", "pull-response"));
    }
    Ok(SyncPullResponseBody::Page(response))
}

fn validate_generated<T: serde::Serialize>(
    schema: &str,
    response: &T,
    validators: &crate::validation::Registry,
) -> bool {
    serde_json::to_value(response).is_ok_and(|value| validators.validate_stored(schema, &value))
}

fn map_sync_store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::InvalidSyncObject { scope, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored sync object {scope}/{id}"),
        ),
        _ => ApiError::internal(),
    }
}

fn draft_validation_error(index: usize, document: &str, field: &str) -> ApiError {
    ApiError::validation(vec![
        Value::String("body".to_owned()),
        Value::String("rows".to_owned()),
        Value::String(index.to_string()),
        Value::String(document.to_owned()),
        Value::String(field.to_owned()),
    ])
}

fn validate_entity_envelope(
    payload: &Value,
    collection: &str,
    id: &str,
    deleted: bool,
    validators: &crate::validation::Registry,
) -> bool {
    let Some(object) = payload.as_object() else {
        return false;
    };
    if object.len() != 3
        || object.get("collection").and_then(Value::as_str) != Some(collection)
        || object.get("id").and_then(Value::as_str) != Some(id)
        || !object.get("value").is_some_and(Value::is_object)
    {
        return false;
    }
    if deleted {
        return object
            .get("value")
            .and_then(Value::as_object)
            .filter(|value| value.is_empty())
            .is_some();
    }
    validators.validate_stored("SyncEntityPayload", payload)
}

fn readable_entity_envelope(payload: &Value, collection: &str, id: &str) -> bool {
    let Some(object) = payload.as_object() else {
        return false;
    };
    object.get("collection").and_then(Value::as_str) == Some(collection)
        && object.get("id").and_then(Value::as_str) == Some(id)
        && object.get("value").is_some_and(Value::is_object)
}

fn entity_payload_schema(collection: &str) -> Option<&'static str> {
    Some(match collection {
        "agent" => "SyncAgentEntityPayload",
        "room" => "SyncRoomEntityPayload",
        "task" => "SyncTaskEntityPayload",
        "monitor" => "SyncMonitorEntityPayload",
        "complaint" => "SyncComplaintEntityPayload",
        "request" => "SyncRequestEntityPayload",
        "rule" => "SyncRuleEntityPayload",
        "project" => "SyncProjectEntityPayload",
        "peerTeam" => "SyncPeerTeamEntityPayload",
        "chat" => "SyncChatEntityPayload",
        "edge" => "SyncEdgeEntityPayload",
        "event" => "SyncEventEntityPayload",
        "work" => "SyncWorkEntityPayload",
        "workspace" => "SyncWorkspaceEntityPayload",
        "uiPreferences" => "SyncUiPreferencesEntityPayload",
        _ => return None,
    })
}

fn invalid_entity(collection: &str, id: &str) -> StoreError {
    StoreError::InvalidSyncObject {
        scope: collection.to_owned(),
        id: id.to_owned(),
    }
}

fn invalid_stored_draft(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored sync object drafts/{id}"),
    )
}

fn validate_draft_value(
    validators: &crate::validation::Registry,
    id: &str,
    payload: &Value,
) -> Result<(), ()> {
    validators
        .validate_stored("DraftPayload", payload)
        .then_some(())
        .ok_or(())?;
    sync_policy::validate_draft(id, payload).map_err(|_| ())
}

fn query_integer(query: &QueryParams, name: &str, default: i64) -> Result<i64, ApiError> {
    match query.typed_value(name) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value.as_i64().ok_or_else(|| {
            ApiError::validation(vec![
                Value::String("query".to_owned()),
                Value::String(name.to_owned()),
            ])
        }),
    }
}

#[cfg(test)]
mod vault_maintenance_tests {
    use super::sync_policy;
    use super::{
        ENTITY_PROJECTION_PREPARATION_MAX_ATTEMPTS, SyncPullResponseBody,
        capture_project_vault_snapshot, pull_with_maintenance, pull_with_maintenance_interleaved,
    };
    use axum::response::IntoResponse;
    use serde_json::{Value, json};
    use studio_runtime_store::{ProjectsRepository, RuntimeStore, SyncDocumentsRepository};

    fn response_value(response: &SyncPullResponseBody) -> Value {
        serde_json::to_value(response).expect("generated sync response serializes")
    }

    #[test]
    fn direct_sync_dtos_match_legacy_generated_wire_bytes() {
        let page_json = json!({
            "workspaceId":"workspace-1",
            "generation":7,
            "documents":[{"id":"entity:agent:a","payload":"{}","seq":8,"_deleted":false}],
            "checkpoint":{"seq":8},
            "maxSeq":10,
            "initialHigh":9
        });
        let page: crate::generated::SyncPullResponse =
            serde_json::from_value(page_json).expect("legacy page projects to DTO");
        let direct_page = crate::generated::SyncPullResponse {
            sync_entities: None,
            sync_entities_after: None,
            checkpoint: crate::generated::SyncCheckpoint { seq: 8 },
            documents: vec![crate::generated::SyncDocument {
                id: "entity:agent:a".to_owned(),
                payload: "{}".to_owned(),
                seq: 8,
                deleted: false,
            }],
            floor: None,
            generation: 7,
            initial_high: Some(Some(9)),
            max_seq: Some(Some(10)),
            reset: None,
            workspace_id: "workspace-1".to_owned(),
        };
        assert_eq!(
            serde_json::to_vec(&direct_page).expect("direct page serializes"),
            serde_json::to_vec(&page).expect("legacy page serializes")
        );

        let reset_json = json!({
            "workspaceId":"workspace-1", "generation":7,
            "reset":true, "floor":3, "maxSeq":10
        });
        let reset: crate::generated::SyncPullResetResponse =
            serde_json::from_value(reset_json).expect("legacy reset projects to DTO");
        let direct_reset = crate::generated::SyncPullResetResponse {
            sync_entities: None,
            sync_entities_after: None,
            floor: 3,
            generation: 7,
            max_seq: 10,
            reset: true,
            workspace_id: "workspace-1".to_owned(),
        };
        assert_eq!(
            serde_json::to_vec(&direct_reset).expect("direct reset serializes"),
            serde_json::to_vec(&reset).expect("legacy reset serializes")
        );
    }

    fn maintenance_fixture() -> (
        tempfile::TempDir,
        RuntimeStore,
        Value,
        crate::validation::Registry,
        String,
    ) {
        let directory = tempfile::tempdir().expect("isolated state directory");
        let store = RuntimeStore::open(Some(directory.path())).expect("runtime store opens");
        let project_path = directory
            .path()
            .join("Project")
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(&project_path).expect("project directory is created");
        let project = json!({
            "id": project_path,
            "path": project_path,
            "projectAliases": [],
        });
        store
            .transaction(|transaction| ProjectsRepository::put_project(transaction, &project))
            .expect("project source is stored");
        store
            .transaction(|transaction| {
                transaction.execute("DELETE FROM sync_entity_meta WHERE key='seeded'", [])?;
                Ok(())
            })
            .expect("force entity projections through maintenance path");
        let validators = crate::validation::Registry::compile(&[(
            "GET".to_owned(),
            "/api/sync/pull".to_owned(),
        )])
        .expect("sync pull validators compile");
        let state_dir = store.root().to_string_lossy().into_owned();
        store
            .mark_entity_projections_maintained(store.committed_change_generation(), "stale".into())
            .expect("force one projection refresh");
        (directory, store, project, validators, state_dir)
    }

    #[test]
    fn stale_room_snapshot_retries_after_an_injected_interleaving() {
        let (_directory, store, project, validators, state_dir) = maintenance_fixture();
        let mut interleavings = 0;
        let mut before_recheck = || {
            interleavings += 1;
            if interleavings == 1 {
                let mut updated = project.clone();
                updated["path"] = json!(format!(
                    "{}/changed-during-filesystem-preparation",
                    project["path"].as_str().expect("project path is a string")
                ));
                store
                    .transaction(|transaction| {
                        ProjectsRepository::put_project(transaction, &updated)
                    })
                    .expect("injected project update commits");
            }
        };

        let response = pull_with_maintenance_interleaved(
            &store,
            &state_dir,
            &validators,
            0,
            100,
            false,
            0,
            false,
            None,
            0,
            &mut before_recheck,
        )
        .expect("pull retries against the updated room snapshot");

        assert!(response_value(&response)["documents"].is_array());
        assert_eq!(interleavings, 2, "the stale preparation is retried once");
    }

    #[tokio::test]
    async fn repeated_room_snapshot_interleavings_return_conflict_after_bounded_retries() {
        let (_directory, store, project, validators, state_dir) = maintenance_fixture();
        let mut interleavings = 0;
        let mut before_recheck = || {
            interleavings += 1;
            let mut updated = project.clone();
            updated["path"] = json!(format!(
                "{}/concurrent-{interleavings}",
                project["path"].as_str().expect("project path is a string")
            ));
            store
                .transaction(|transaction| ProjectsRepository::put_project(transaction, &updated))
                .expect("injected project update commits");
        };

        let error = match pull_with_maintenance_interleaved(
            &store,
            &state_dir,
            &validators,
            0,
            100,
            false,
            0,
            false,
            None,
            0,
            &mut before_recheck,
        ) {
            Err(error) => error,
            Ok(_) => panic!("persistent room updates exhaust the retry budget"),
        };
        let response = error.into_response();

        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("conflict response body is readable");
        let body: Value = serde_json::from_slice(&body).expect("conflict response is JSON");
        assert_eq!(body["error"], super::ENTITY_PROJECTION_PREPARATION_CONFLICT);
        assert_eq!(interleavings, ENTITY_PROJECTION_PREPARATION_MAX_ATTEMPTS);
    }

    #[test]
    fn entity_pull_reprojects_when_cached_vault_changes_without_a_store_write() {
        let directory = tempfile::tempdir().expect("isolated state directory");
        let store = RuntimeStore::open(Some(directory.path())).expect("runtime store opens");
        let source = "/workspace/Project One";
        let project_id = studio_runtime_store::sha256_hex(source.as_bytes());
        let project = json!({
            "id": source,
            "path": source,
            "projectAliases": [{
                "serverId": "fixture-server",
                "projectId": source,
                "name": "Project One",
            }],
        });
        let project_folder = store
            .root()
            .join("linux-vm/layr-projects")
            .join(&project_id);
        std::fs::create_dir_all(&project_folder).expect("vault folder is created");
        std::fs::write(
            project_folder.join("vault.json"),
            br#"{"configured":true,"prefix":"project"}"#,
        )
        .expect("vault record is written");
        std::fs::write(
            project_folder.join("vault-site.json"),
            br#"{"site":"https://before.example/"}"#,
        )
        .expect("site record is written");

        store
            .transaction(|transaction| {
                ProjectsRepository::put_project(transaction, &project)?;
                let mut ready_project = project.clone();
                ready_project["layrReadiness"] = json!({"state":"ready","projectId":project_id});
                ready_project["layrSync"] = json!({
                    "projectId":project_id,
                    "state":"synced",
                    "enabled":true,
                    "conflicts":[],
                    "choices":[],
                    "withAgent":[],
                    "deletes":0,
                });
                SyncDocumentsRepository::put_entity(
                    transaction,
                    "project",
                    source,
                    ready_project,
                    false,
                )?;
                Ok(())
            })
            .expect("project source and ready entity are stored");
        let validators = crate::validation::Registry::compile(&[(
            "GET".to_owned(),
            "/api/sync/pull".to_owned(),
        )])
        .expect("sync pull validators compile");
        let state_dir = store.root().to_string_lossy().into_owned();
        let pull = || {
            pull_with_maintenance(
                &store,
                &state_dir,
                &validators,
                0,
                100,
                false,
                0,
                false,
                None,
                0,
            )
            .expect("entity pull succeeds")
        };
        let first_pull = pull();
        assert!(response_value(&first_pull)["documents"].is_array());
        let first_payload: String = store
            .read()
            .expect("read store")
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='project' AND id=?1 AND deleted=0",
                [source],
                |row| row.get(0),
            )
            .expect("project projection exists");
        let first_payload: Value = serde_json::from_str(&first_payload).expect("JSON payload");
        assert_eq!(
            first_payload["value"]["vault"]["site"],
            "https://before.example/"
        );

        std::fs::write(
            project_folder.join("vault-site.json"),
            br#"{"site":"https://after.example/"}"#,
        )
        .expect("cached vault changes without a SQLite write");
        let second_pull = pull();
        assert!(response_value(&second_pull)["documents"].is_array());
        let second_payload: String = store
            .read()
            .expect("read store")
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='project' AND id=?1 AND deleted=0",
                [source],
                |row| row.get(0),
            )
            .expect("updated project projection exists");
        let second_payload: Value = serde_json::from_str(&second_payload).expect("JSON payload");
        assert_eq!(
            second_payload["value"]["vault"]["site"],
            "https://after.example/"
        );
        assert_ne!(first_payload, second_payload);
    }

    #[test]
    fn cold_entity_pull_reuses_complete_seed_without_scanning_source_rows() {
        const ENTITY_COUNT: usize = 2_000;
        let directory = tempfile::tempdir().expect("isolated state directory");
        let store = RuntimeStore::open(Some(directory.path())).expect("runtime store opens");
        store
            .transaction(|transaction| {
                for index in 0..ENTITY_COUNT {
                    let id = format!("agent-{index:04}");
                    transaction.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        rusqlite::params![id, json!({"id":id,"status":"idle"}).to_string()],
                    )?;
                    transaction.execute(
                        "INSERT INTO runtime_work(id,record) VALUES(?1,?2)",
                        rusqlite::params![
                            format!("work-{index:04}"),
                            json!({"id":format!("work-{index:04}"),"owner":id,"results":[]})
                                .to_string()
                        ],
                    )?;
                    SyncDocumentsRepository::put_entity(
                        transaction,
                        "agent",
                        &id,
                        json!({
                            "id":id,
                            "kind":"agent",
                            "source":"managed",
                            "canSend":true,
                            "launcherAlive":true,
                        }),
                        false,
                    )?;
                }
                Ok(())
            })
            .expect("synthetic Python-seeded state is stored");
        let seeded_generation = store.committed_change_generation();
        let validators = crate::validation::Registry::compile(&[(
            "GET".to_owned(),
            "/api/sync/pull".to_owned(),
        )])
        .expect("sync pull validators compile");
        let state_dir = store.root().to_string_lossy().into_owned();
        let started = std::time::Instant::now();
        let response = pull_with_maintenance(
            &store,
            &state_dir,
            &validators,
            0,
            100,
            false,
            0,
            false,
            None,
            0,
        )
        .expect("cold pull succeeds");
        let cold_elapsed = started.elapsed();
        let response = response_value(&response);
        assert_eq!(response["documents"].as_array().unwrap().len(), 100);
        assert_eq!(store.committed_change_generation(), seeded_generation);
        let reader = store.read().expect("store reader");
        assert!(sync_policy::has_complete_entity_seed(&reader).expect("seed marker reads"));
        drop(reader);
        let (_, vault_snapshot) =
            capture_project_vault_snapshot(&store, &state_dir).expect("vault snapshot");
        assert!(
            !store
                .entity_projections_need_maintenance(&vault_snapshot.signature())
                .expect("projection watermark reads")
        );
        let warm_started = std::time::Instant::now();
        let warm_response = pull_with_maintenance(
            &store,
            &state_dir,
            &validators,
            0,
            100,
            false,
            0,
            false,
            None,
            0,
        )
        .expect("warm pull succeeds");
        let warm_elapsed = warm_started.elapsed();
        let warm_response = response_value(&warm_response);
        assert_eq!(warm_response["documents"].as_array().unwrap().len(), 100);
        eprintln!(
            "synthetic sync pull: entities={ENTITY_COUNT}, source rows scanned=0, agent contexts computed=0, cold page rows={}, cold elapsed={cold_elapsed:?}, warm elapsed={warm_elapsed:?}",
            response["documents"].as_array().unwrap().len(),
        );
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "sync_documents",
        routes: Some(routes),
        dependencies: &["resource-policy/publish-drafts", "sync-policy/entity-seeding", "sync-policy/entity-window-maintenance", "sync-policy/runtime-capabilities"],
    }
}
