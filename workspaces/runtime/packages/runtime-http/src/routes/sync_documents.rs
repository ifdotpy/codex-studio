//! HTTP adapter for the durable sync document operations.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use studio_runtime_schema::{SYNC_ENTITY_SCOPE, SYNC_PROTOCOL_VERSION};
use studio_runtime_services::sync_documents::{self, DraftWrite};
use studio_runtime_services::sync_entity_context::{RoomDirectory, RoomSources};
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
        #[allow(
            clippy::disallowed_methods,
            reason = "runs inside blocking_pool::spawn_blocking"
        )]
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
        "maxStreamDocuments": studio_runtime_schema::shared_values::SYNC_ENTITY_STREAM_MAX_DOCUMENTS,
        "maxStreamBytes": studio_runtime_schema::shared_values::SYNC_STREAM_EVENT_MAX_BYTES,
    });
    Ok(Json(super::generated_json::<
        crate::generated::SyncProtocolResponse,
    >(response)?))
}

pub(crate) async fn pull(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Response, ApiError> {
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
    if let Some(agent) = scope.strip_prefix("transcript:") {
        let mut response = transcript::pull_scope(store.clone(), &scope, agent, after)
            .await
            .map_err(super::transcript::map_transcript_error)?;
        // Python samples generation after transcript maintenance commits.
        let generation_store = store.clone();
        response["generation"] = json!(
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/sync_documents/transcript-generation",
                move || generation_store
                    .generation()
                    .map_err(|_| ApiError::internal()),
            )
            .await
            .map_err(|_| ApiError::internal())??
        );
        if !validators.validate_stored("SyncPullResponse", &response) {
            return Err(invalid_entity("sync_entities", "transcript-pull-response").into());
        }
        return Ok(Json(SyncPullResponseBody::Page(super::generated_json::<
            crate::generated::SyncPullResponse,
        >(response)?))
        .into_response());
    }
    let generation_before_vault_read = store.committed_change_generation();
    let vault_snapshot = if scope == "drafts" {
        sync_policy::ProjectVaultSnapshot::default()
    } else {
        sync_policy::ProjectVaultSnapshot::capture_for_sync(
            &store,
            std::path::Path::new(&state_dir),
        )
        .await
        .map_err(map_sync_store_error)?
    };
    let read_store = store.clone();
    let read_validators = validators.clone();
    let read_state_dir = state_dir.clone();
    let read_priority_id = priority_id.clone();
    let result = store
        .read_async(
            "runtime-http/routes/sync_documents",
            move |connection| -> Result<EntityPullResult, ApiError> {
                let store = read_store;
                let validators = read_validators;
                let generation = store.generation().map_err(|_| ApiError::internal())?;
                if scope == "drafts" {
                    let workspace_id = SyncDocumentsRepository::workspace_id(connection)
                        .map_err(|_| ApiError::internal())?;
                    let documents = SyncDocumentsRepository::pull_drafts(connection, after, limit)
                        .map_err(|_| ApiError::internal())?;
                    let checkpoint = documents.last().map_or(after, |document| document.sequence);
                    let response = crate::generated::SyncPullResponse {
                        checkpoint: crate::generated::SyncCheckpoint { seq: checkpoint },
                        documents: documents
                            .into_iter()
                            .map(|document| draft_document(document).map(crate::generated::SyncPullResponseDocumentsItemUnion::SyncDocument))
                            .collect::<Result<Vec<_>, _>>()?,
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
                    return Ok(EntityPullResult::Response(SyncPullResponseBody::Page(
                        response,
                    )));
                }
                if !vault_snapshot
                    .matches_sources(connection)
                    .map_err(map_sync_store_error)?
                {
                    return Ok(EntityPullResult::NeedsMaintenance(generation));
                }
                let vault_signature = vault_snapshot.signature();
                if generation_before_vault_read != store.committed_change_generation()
                    || store
                        .entity_projections_need_maintenance(&vault_signature)
                        .map_err(map_sync_store_error)?
                {
                    // Under steady writes the commit generation always differs from the last
                    // maintenance. The projections are still current when this snapshot's
                    // project payloads already carry the vault views just captured, so serve
                    // the page without taking the global maintenance lock. Nothing is marked
                    // here: the snapshot may predate the sampled generation.
                    let current = sync_policy::has_complete_entity_seed(connection)
                        .map_err(map_sync_store_error)?
                        && vault_snapshot
                            .matches_entity_projections(connection)
                            .map_err(map_sync_store_error)?;
                    if !current {
                        return Ok(EntityPullResult::NeedsMaintenance(generation));
                    }
                }
                // The page reads from the same snapshot as the vault check, so
                // commits that land meanwhile cannot discard it.
                match pull_entities(
                    connection,
                    &read_state_dir,
                    false,
                    None,
                    None,
                    None,
                    after,
                    limit,
                    fresh,
                    initial_high,
                    reset_support,
                    read_priority_id.as_deref(),
                    generation,
                    &validators,
                ) {
                    Ok(response) => Ok(EntityPullResult::Response(response)),
                    Err(StoreError::Sqlite(studio_runtime_store::SqliteError::SqliteFailure(
                        error,
                        _,
                    ))) if error.code == studio_runtime_store::ErrorCode::ReadOnly => {
                        Ok(EntityPullResult::NeedsMaintenance(generation))
                    }
                    Err(error) => Err(map_sync_store_error(error)),
                }
            },
        )
        .await?;
    match result {
        EntityPullResult::Response(response) => Ok(Json(response).into_response()),
        EntityPullResult::NeedsMaintenance(generation) => {
            let maintenance = async move {
                match pull_with_maintenance(
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
                    || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                        Box::pin(async {})
                    },
                )
                .await
                {
                    Ok(response) => Json(response).into_response(),
                    Err(error) => error.into_response(),
                }
            };
            Ok(state
                .run_tracked_mutation(
                    "GET /api/sync/pull/entity-maintenance".to_owned(),
                    None,
                    maintenance,
                )
                .await)
        }
    }
}

enum EntityMaintenancePreparation {
    Response(SyncPullResponseBody),
    ReadFallback {
        vault_signature: String,
        observed_generation: u64,
    },
    PrepareFilesystem,
}

#[allow(clippy::too_many_arguments)]
async fn pull_with_maintenance(
    store: Arc<RuntimeStore>,
    state_dir: String,
    validators: crate::validation::Registry,
    after: i64,
    limit: i64,
    fresh: bool,
    initial_high: i64,
    reset_support: bool,
    priority_id: Option<String>,
    generation: i64,
    mut before_room_recheck: impl FnMut() -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ()> + Send>,
    > + Send,
) -> Result<SyncPullResponseBody, ApiError> {
    let _maintenance_guard = store
        .lock_entity_projection_maintenance()
        .await
        .map_err(map_sync_store_error)?;
    for _ in 0..ENTITY_PROJECTION_PREPARATION_MAX_ATTEMPTS {
        let prepare_store = store.clone();
        let prepare_state_dir = state_dir.clone();
        let prepare_validators = validators.clone();
        let prepare_priority_id = priority_id.clone();
        let generation_before_vault_read = store.committed_change_generation();
        let vault_snapshot = sync_policy::ProjectVaultSnapshot::capture_for_sync(
            &store,
            std::path::Path::new(&state_dir),
        )
        .await
        .map_err(map_sync_store_error)?;
        let preparation = store
            .read_async(
                "runtime-http/routes/sync_documents/entity-maintenance-preparation",
                move |transaction| -> Result<EntityMaintenancePreparation, ApiError> {
                    if !vault_snapshot
                        .matches_sources(transaction)
                        .map_err(map_sync_store_error)?
                    {
                        return Ok(EntityMaintenancePreparation::PrepareFilesystem);
                    }
                    let vault_signature = vault_snapshot.signature();
                    let observed_generation = prepare_store.committed_change_generation();
                    let needs_maintenance = generation_before_vault_read != observed_generation
                        || prepare_store
                            .entity_projections_need_maintenance(&vault_signature)
                            .map_err(map_sync_store_error)?;
                    if !needs_maintenance {
                        return match pull_entities(
                            transaction,
                            &prepare_state_dir,
                            false,
                            None,
                            None,
                            None,
                            after,
                            limit,
                            fresh,
                            initial_high,
                            reset_support,
                            prepare_priority_id.as_deref(),
                            generation,
                            &prepare_validators,
                        ) {
                            Ok(response) => Ok(EntityMaintenancePreparation::Response(response)),
                            Err(StoreError::Sqlite(
                                studio_runtime_store::SqliteError::SqliteFailure(error, _),
                            )) if error.code == studio_runtime_store::ErrorCode::ReadOnly => {
                                Ok(EntityMaintenancePreparation::ReadFallback {
                                    vault_signature,
                                    observed_generation,
                                })
                            }
                            Err(error) => Err(map_sync_store_error(error)),
                        };
                    }

                    if sync_policy::has_complete_entity_seed(transaction)
                        .map_err(map_sync_store_error)?
                        && vault_snapshot
                            .matches_entity_projections(transaction)
                            .map_err(map_sync_store_error)?
                    {
                        let response = pull_entities(
                            transaction,
                            &prepare_state_dir,
                            false,
                            None,
                            None,
                            None,
                            after,
                            limit,
                            fresh,
                            initial_high,
                            reset_support,
                            prepare_priority_id.as_deref(),
                            generation,
                            &prepare_validators,
                        )
                        .map_err(map_sync_store_error)?;
                        // The sampled generation predates the snapshot, so a commit
                        // that lands during this read forces a later maintenance check.
                        prepare_store
                            .mark_entity_projections_maintained(
                                generation_before_vault_read,
                                vault_signature,
                            )
                            .map_err(map_sync_store_error)?;
                        return Ok(EntityMaintenancePreparation::Response(response));
                    }

                    Ok(EntityMaintenancePreparation::PrepareFilesystem)
                },
            )
            .await?;

        let (rooms, vault_snapshot, vault_signature, project_server_id) = match preparation {
            EntityMaintenancePreparation::Response(response) => return Ok(response),
            EntityMaintenancePreparation::ReadFallback {
                vault_signature,
                observed_generation,
            } => {
                let writer_store = store.clone();
                let writer_state_dir = state_dir.clone();
                let writer_validators = validators.clone();
                let writer_priority_id = priority_id.clone();
                let (response, _) = store
                    .transaction_with_commit_generation(move |transaction| {
                        if writer_store.committed_change_generation() != observed_generation
                            || writer_store.entity_projections_need_maintenance(&vault_signature)?
                        {
                            return Ok(None);
                        }
                        pull_entities(
                            transaction,
                            &writer_state_dir,
                            false,
                            None,
                            None,
                            None,
                            after,
                            limit,
                            fresh,
                            initial_high,
                            reset_support,
                            writer_priority_id.as_deref(),
                            generation,
                            &writer_validators,
                        )
                        .map(Some)
                    })
                    .await
                    .map_err(map_sync_store_error)?;
                if let Some(response) = response {
                    return Ok(response);
                }
                continue;
            }
            EntityMaintenancePreparation::PrepareFilesystem => {
                let (room_sources, projects) = store
                    .read_async(
                        "runtime-http/routes/sync_documents/entity-maintenance-sources",
                        |transaction| {
                            Ok((
                                RoomSources::read_for_sync(transaction)?,
                                sync_policy::ProjectVaultSnapshot::stored_projects(transaction)?,
                            ))
                        },
                    )
                    .await
                    .map_err(map_sync_store_error)?;
                let prepare_store = store.clone();
                let prepare_state_dir = state_dir.clone();
                studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-http/routes/sync_documents/entity-maintenance-filesystem",
                    move || -> Result<_, ApiError> {
                        // Path canonicalization runs after the read and outside
                        // a SQLite transaction; the writer rechecks these room inputs.
                        let vault_snapshot = sync_policy::ProjectVaultSnapshot::capture(
                            std::path::Path::new(&prepare_state_dir),
                            &projects,
                        );
                        let vault_signature = vault_snapshot.signature();
                        let project_server_id = prepare_store
                            .local_server_id()
                            .map_err(map_sync_store_error)?;
                        let rooms =
                            RoomDirectory::prepare(room_sources).map_err(map_sync_store_error)?;
                        Ok((rooms, vault_snapshot, vault_signature, project_server_id))
                    },
                )
                .await
                .map_err(|_| ApiError::internal())??
            }
        };
        before_room_recheck().await;
        let transaction_state_dir = state_dir.clone();
        let transaction_validators = validators.clone();
        let transaction_priority_id = priority_id.clone();
        let (response, committed_generation) = store
            .transaction_with_commit_generation(move |transaction| {
                if !rooms.matches_for_sync(transaction)? {
                    return Ok(None);
                }
                pull_entities(
                    transaction,
                    &transaction_state_dir,
                    true,
                    project_server_id.as_deref(),
                    Some(&vault_snapshot),
                    Some(&rooms),
                    after,
                    limit,
                    fresh,
                    initial_high,
                    reset_support,
                    transaction_priority_id.as_deref(),
                    generation,
                    &transaction_validators,
                )
                .map(Some)
            })
            .await
            .map_err(map_sync_store_error)?;
        let Some(response) = response else {
            continue;
        };
        store
            .mark_entity_projections_maintained(committed_generation, vault_signature)
            .map_err(map_sync_store_error)?;
        return Ok(response);
    }

    Err(ApiError::new(
        StatusCode::CONFLICT,
        ENTITY_PROJECTION_PREPARATION_CONFLICT,
    ))
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub(crate) enum SyncPullResponseBody {
    Reset(crate::generated::SyncPullResetResponse),
    Page(crate::generated::SyncPullResponse),
}

enum EntityPullResult {
    Response(SyncPullResponseBody),
    NeedsMaintenance(i64),
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
        validate_draft_value(&row.new_document_state.id, &payload)
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
                if validate_draft_value(&row.new_document_state.id, &payload).is_err() {
                    return Err(draft_validation_error(
                        index,
                        "assumedMasterState",
                        "payload",
                    ));
                }
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
    let result = sync_documents::push_drafts(&store, writes, move || wake.notify_one())
        .await
        .map_err(|error| match error {
            sync_documents::DraftPushError::InvalidStored { id } => invalid_stored_draft(&id),
            sync_documents::DraftPushError::Store(_) => ApiError::internal(),
        })?;
    Ok(Json(super::generated_json::<
        Vec<crate::generated::SyncDocument>,
    >(Value::Array(
        result
            .conflicts
            .iter()
            .map(|document| document.response())
            .collect(),
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
    let provider_versions = state.provider_versions.clone();
    let merged = sync_policy::push_preferences(
        &store,
        scope,
        fields,
        move |field| validators.validate_preference_field(field),
        move || {
            if let Some(monitor) = provider_versions {
                monitor.wake();
            }
        },
    )
    .await
    .map_err(|error| match error {
        SyncPolicyError::InvalidPreferences => {
            ApiError::validation(vec![Value::String("body".to_owned())])
        }
        SyncPolicyError::InvalidStoredObject { scope, id } => ApiError::invalid_stored(&scope, &id),
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
    rooms: Option<&RoomDirectory>,
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
        studio_runtime_services::diagnostics::record_sync_recovery(
            studio_runtime_services::diagnostics::SyncRecoveryKind::Reset,
        );
        if after > 0 && after < page.tombstone_floor {
            studio_runtime_services::diagnostics::record_sync_recovery(
                studio_runtime_services::diagnostics::SyncRecoveryKind::Gap,
            );
        }
        let response = crate::generated::SyncPullResetResponse {
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
            let Some(document) = entity_document(row) else {
                continue;
            };
            documents
                .push(crate::generated::SyncPullResponseDocumentsItemUnion::SyncEntity(document));
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

/// Shared conversion for pull and stream: normalize historical stored payloads once
/// through the schema owner, then carry the existing typed sync envelope.
pub(super) fn entity_document(
    row: &studio_runtime_store::EntityRow,
) -> Option<crate::generated::SyncEntity> {
    let mut payload = sync_documents::entity_payload(row)?;
    let object = payload.as_object_mut()?;
    let Value::String(collection) = object.remove("collection")? else {
        return None;
    };
    let Value::String(id) = object.remove("id")? else {
        return None;
    };
    Some(crate::generated::SyncEntity {
        collection,
        id,
        op: if row.deleted {
            crate::generated::SyncEntityOp::Delete
        } else {
            crate::generated::SyncEntityOp::Upsert
        },
        seq: row.sequence,
        value: if row.deleted {
            None
        } else {
            object.remove("value")
        },
    })
}

/// The typed response is built from the generated DTOs, which already carry the schema's
/// shape. The full JSON Schema walk (a second copy of a page of up to ~1 MB as a `Value`,
/// then the walk) runs in debug builds and tests only, not on every pull of a release server.
fn validate_generated<T: serde::Serialize>(
    schema: &str,
    response: &T,
    validators: &crate::validation::Registry,
) -> bool {
    !cfg!(debug_assertions)
        || serde_json::to_value(response)
            .is_ok_and(|value| validators.validate_stored(schema, &value))
}

fn validate_entity_envelope(payload: &Value, collection: &str, id: &str, deleted: bool) -> bool {
    studio_runtime_schema::validate_sync_entity_payload_strict(collection, id, deleted, payload)
        .unwrap_or(false)
}

fn map_sync_store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::InvalidSyncObject { scope, id } => ApiError::invalid_stored(&scope, &id),
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

fn invalid_entity(collection: &str, id: &str) -> StoreError {
    StoreError::InvalidSyncObject {
        scope: collection.to_owned(),
        id: id.to_owned(),
    }
}

/// Both pull and stream preserve the stored bytes used for conflict comparisons.
pub(super) fn draft_document(
    document: studio_runtime_store::SyncDocument,
) -> Result<crate::generated::SyncDocument, ApiError> {
    if sync_policy::contains_unrepresentable_integer(&document.payload) {
        return Err(invalid_stored_draft(&document.id));
    }
    let payload =
        serde_json::from_str(&document.payload).map_err(|_| invalid_stored_draft(&document.id))?;
    validate_draft_value(&document.id, &payload).map_err(|_| invalid_stored_draft(&document.id))?;
    Ok(crate::generated::SyncDocument {
        id: document.id,
        payload: document.payload,
        seq: document.sequence,
        deleted: document.deleted,
    })
}

fn invalid_stored_draft(id: &str) -> ApiError {
    ApiError::invalid_stored("drafts", id)
}

fn validate_draft_value(id: &str, payload: &Value) -> Result<(), ()> {
    studio_runtime_schema::drafts::DraftVersion::from_stored_value(id, payload.clone())
        .map(|_| ())
        .map_err(|_| ())
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
    #![allow(clippy::unwrap_used, clippy::disallowed_methods)]

    use super::sync_policy;
    use super::{
        ENTITY_PROJECTION_PREPARATION_MAX_ATTEMPTS, SyncPullResponseBody, pull_with_maintenance,
        validate_entity_envelope,
    };
    use axum::Json;
    use axum::response::IntoResponse;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use studio_runtime_store::{ProjectsRepository, RuntimeStore, SyncDocumentsRepository};

    fn response_value(response: &SyncPullResponseBody) -> Value {
        serde_json::to_value(response).expect("generated sync response serializes")
    }

    #[tokio::test]
    async fn stored_legacy_drafts_remain_readable_and_replaceable() {
        use axum::{
            body::{Body, to_bytes},
            extract::ConnectInfo,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
        let legacy = json!({"device":"phone","session":"chat","text":"x","updated":1.5,"legacyExtra":{"keep":true}});
        let raw = legacy.to_string();
        let original = raw.clone();
        store
            .transaction(move |tx| {
                SyncDocumentsRepository::put_draft(tx, "phone:chat", &raw, false).map(|_| ())
            })
            .await
            .unwrap();
        let app = crate::router::router(crate::router::HttpState::new(
            "token".into(),
            "workspace".into(),
            directory.path().display().to_string(),
            4321,
            None,
            Some(store.clone()),
            None,
        ))
        .unwrap();
        let request = |method: &str, uri: &str, body: Value| {
            let mut request = Request::builder()
                .method(method)
                .uri(uri)
                .header("host", "127.0.0.1:4321")
                .header("origin", "http://127.0.0.1:4321")
                .header("x-canvas-token", "token")
                .header("x-canvas-workspace", "workspace")
                .header("content-type", "application/json")
                .body(if method == "GET" {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap();
            request.extensions_mut().insert(ConnectInfo(
                "127.0.0.1:4321".parse::<std::net::SocketAddr>().unwrap(),
            ));
            request
        };
        let response = app
            .clone()
            .oneshot(request("GET", "/api/sync/pull?scope=drafts", Value::Null))
            .await
            .unwrap();
        let status = response.status();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["documents"][0]["payload"], original);
        let canonical =
            json!({"id":"phone:chat","device":"phone","session":"chat","text":"next","updated":2});
        let rows = json!({"rows":[{"newDocumentState":{"id":"phone:chat","payload":canonical.to_string(),"seq":0,"deleted":false},
            "assumedMasterState":{"id":"phone:chat","payload":original,"seq":1,"deleted":false}}]});
        let response = app
            .clone()
            .oneshot(request("POST", "/api/sync/drafts", rows))
            .await
            .unwrap();
        let status = response.status();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!([]));
        let stored = store
            .transaction(|tx| SyncDocumentsRepository::draft(tx, "phone:chat"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stored.payload).unwrap(),
            canonical
        );
        #[derive(serde::Deserialize)]
        struct DraftFixture {
            name: String,
            payload: Value,
        }
        let fixtures: Vec<DraftFixture> = serde_json::from_str(include_str!(
            "../../../../../client/contracts/draft-compatibility.json"
        ))
        .unwrap();
        let native = fixtures
            .into_iter()
            .find(|fixture| fixture.name == "extension")
            .expect("shared native legacy extension fixture");
        let rows = json!({"rows":[{
            "newDocumentState":{"id":"phone:chat","payload":native.payload.to_string(),"seq":0,"deleted":false},
            "assumedMasterState":{"id":"phone:chat","payload":stored.payload,"seq":stored.sequence,"deleted":false}
        }]});
        let mut committed_sequence = None;
        for attempt in ["initial native legacy push", "same-value retry"] {
            let response = app
                .clone()
                .oneshot(request("POST", "/api/sync/drafts", rows.clone()))
                .await
                .unwrap();
            let status = response.status();
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(status, StatusCode::OK, "{attempt}: {body}");
            assert_eq!(body, json!([]), "{attempt}");
            let current = store
                .transaction(|tx| SyncDocumentsRepository::draft(tx, "phone:chat"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&current.payload).unwrap(),
                native.payload,
                "{attempt}: preserve all historical fields"
            );
            if let Some(sequence) = committed_sequence {
                assert_eq!(current.sequence, sequence, "retry must not write twice");
            } else {
                assert!(current.sequence > stored.sequence);
                committed_sequence = Some(current.sequence);
            }
        }
    }

    #[test]
    fn projection_write_validation_rejects_extra_envelope_and_branch_fields() {
        let valid = json!({
            "collection":"task",
            "id":"task-1",
            "value":{"id":"task-1"}
        });
        assert!(validate_entity_envelope(&valid, "task", "task-1", false));
        let tombstone = json!({
            "collection":"task",
            "id":"task-1",
            "value":{}
        });
        assert!(validate_entity_envelope(&tombstone, "task", "task-1", true));

        let extra_envelope = json!({
            "collection":"task",
            "id":"task-1",
            "value":{"id":"task-1"},
            "legacyEnvelopeField":true
        });
        assert!(!validate_entity_envelope(
            &extra_envelope,
            "task",
            "task-1",
            false
        ));

        let extra_branch_field = json!({
            "collection":"task",
            "id":"task-1",
            "value":{"id":"task-1","legacyEntityField":true}
        });
        assert!(!validate_entity_envelope(
            &extra_branch_field,
            "task",
            "task-1",
            false
        ));
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
            checkpoint: crate::generated::SyncCheckpoint { seq: 8 },
            documents: vec![
                crate::generated::SyncPullResponseDocumentsItemUnion::SyncDocument(
                    crate::generated::SyncDocument {
                        id: "entity:agent:a".to_owned(),
                        payload: "{}".to_owned(),
                        seq: 8,
                        deleted: false,
                    },
                ),
            ],
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

    async fn maintenance_fixture() -> (
        tempfile::TempDir,
        Arc<RuntimeStore>,
        Value,
        crate::validation::Registry,
        String,
    ) {
        let directory = tempfile::tempdir().expect("isolated state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store opens"),
        );
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
        let stored_project = project.clone();
        store
            .transaction(move |transaction| {
                ProjectsRepository::put_project(transaction, &stored_project)
            })
            .await
            .expect("project source is stored");
        store
            .transaction(|transaction| {
                transaction.execute("DELETE FROM sync_entity_meta WHERE key='seeded'", [])?;
                Ok(())
            })
            .await
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

    #[tokio::test]
    async fn stale_room_snapshot_retries_after_an_injected_interleaving() {
        let (_directory, store, project, validators, state_dir) = maintenance_fixture().await;
        let interleavings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let before_recheck = {
            let interleavings = interleavings.clone();
            let store = store.clone();
            let project = project.clone();
            move || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                let interleavings = interleavings.clone();
                let store = store.clone();
                let project = project.clone();
                Box::pin(async move {
                    let current =
                        interleavings.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if current == 1 {
                        let mut updated = project.clone();
                        updated["path"] = json!(format!(
                            "{}/changed-during-filesystem-preparation",
                            project["path"].as_str().expect("project path is a string")
                        ));
                        store
                            .transaction(move |transaction| {
                                ProjectsRepository::put_project(transaction, &updated)
                            })
                            .await
                            .expect("injected project update commits");
                    }
                })
            }
        };

        let response = pull_with_maintenance(
            store.clone(),
            state_dir.clone(),
            validators.clone(),
            0,
            100,
            false,
            0,
            false,
            None,
            0,
            before_recheck,
        )
        .await
        .expect("pull retries against the updated room snapshot");

        assert!(response_value(&response)["documents"].is_array());
        assert_eq!(
            interleavings.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "the stale preparation is retried once"
        );
    }

    #[tokio::test]
    async fn repeated_room_snapshot_interleavings_return_conflict_after_bounded_retries() {
        let (_directory, store, project, validators, state_dir) = maintenance_fixture().await;
        let interleavings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let before_recheck = {
            let interleavings = interleavings.clone();
            let store = store.clone();
            let project = project.clone();
            move || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                let interleavings = interleavings.clone();
                let store = store.clone();
                let project = project.clone();
                Box::pin(async move {
                    let current =
                        interleavings.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    let mut updated = project.clone();
                    updated["path"] = json!(format!(
                        "{}/concurrent-{current}",
                        project["path"].as_str().expect("project path is a string")
                    ));
                    store
                        .transaction(move |transaction| {
                            ProjectsRepository::put_project(transaction, &updated)
                        })
                        .await
                        .expect("injected project update commits");
                })
            }
        };

        let error = match pull_with_maintenance(
            store.clone(),
            state_dir.clone(),
            validators.clone(),
            0,
            100,
            false,
            0,
            false,
            None,
            0,
            before_recheck,
        )
        .await
        {
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
        assert_eq!(
            interleavings.load(std::sync::atomic::Ordering::Relaxed),
            ENTITY_PROJECTION_PREPARATION_MAX_ATTEMPTS
        );
    }

    #[tokio::test]
    async fn canceled_maintenance_response_keeps_the_projection_lock_until_commit_and_mark() {
        let (_directory, store, _project, validators, state_dir) = maintenance_fixture().await;
        let state = Arc::new(crate::router::HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
            state_dir.clone(),
            4620,
            None,
            Some(store.clone()),
            None,
        ));
        let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
        let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel();
        let first_store = store.clone();
        let first_validators = validators.clone();
        let first_state_dir = state_dir.clone();
        let mut first_started_tx = Some(first_started_tx);
        let mut release_first_rx = Some(release_first_rx);
        let first_maintenance = async move {
            let result = pull_with_maintenance(
                first_store,
                first_state_dir,
                first_validators,
                0,
                100,
                false,
                0,
                false,
                None,
                0,
                move || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                    let started = first_started_tx.take();
                    let release = release_first_rx.take();
                    Box::pin(async move {
                        if let Some(started) = started {
                            started
                                .send(())
                                .expect("test waits for first maintenance start");
                        }
                        if let Some(release) = release {
                            release
                                .await
                                .expect("test retains first maintenance release sender");
                        }
                    })
                },
            )
            .await;
            match result {
                Ok(response) => Json(response).into_response(),
                Err(error) => error.into_response(),
            }
        };
        let waiting_response = {
            let state = state.clone();
            tokio::spawn(async move {
                state
                    .run_tracked_mutation(
                        "GET /api/sync/pull/entity-maintenance".to_owned(),
                        None,
                        first_maintenance,
                    )
                    .await
            })
        };
        first_started_rx
            .await
            .expect("first maintenance reaches its pre-commit hold");
        waiting_response.abort();
        assert!(matches!(
            waiting_response.await,
            Err(error) if error.is_cancelled()
        ));

        let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();
        let (second_reached_tx, second_reached_rx) = tokio::sync::oneshot::channel();
        let second_store = store.clone();
        let second_validators = validators.clone();
        let second_state_dir = state_dir.clone();
        let second = tokio::spawn(async move {
            second_started_tx
                .send(())
                .expect("test waits for second maintenance start");
            let mut second_reached_tx = Some(second_reached_tx);
            pull_with_maintenance(
                second_store,
                second_state_dir,
                second_validators,
                0,
                100,
                false,
                0,
                false,
                None,
                0,
                move || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                    let reached = second_reached_tx.take();
                    Box::pin(async move {
                        if let Some(reached) = reached {
                            reached
                                .send(())
                                .expect("test waits for the second preparation gate");
                        }
                    })
                },
            )
            .await
        });
        second_started_rx
            .await
            .expect("second maintenance starts while the first is retained");
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), second_reached_rx)
                .await
                .is_err(),
            "second preparation must not pass the lock while the first commit is pending"
        );
        release_first_tx
            .send(())
            .expect("first maintenance is still waiting");
        second
            .await
            .expect("second maintenance task completes")
            .expect("second pull succeeds after the first maintenance commits");
        state.shutdown_mutations().await;
    }

    #[tokio::test]
    async fn entity_pull_reprojects_when_cached_vault_changes_without_a_store_write() {
        let directory = tempfile::tempdir().expect("isolated state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store opens"),
        );
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

        let source_for_store = source.to_owned();
        store
            .transaction(move |transaction| {
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
                    &source_for_store,
                    ready_project,
                    false,
                )?;
                Ok(())
            })
            .await
            .expect("project source and ready entity are stored");
        let validators = crate::validation::Registry::compile(&[(
            "GET".to_owned(),
            "/api/sync/pull".to_owned(),
        )])
        .expect("sync pull validators compile");
        let state_dir = store.root().to_string_lossy().into_owned();
        let pull = || {
            pull_with_maintenance(
                store.clone(),
                state_dir.clone(),
                validators.clone(),
                0,
                100,
                false,
                0,
                false,
                None,
                0,
                || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                    Box::pin(async {})
                },
            )
        };
        let first_pull = pull().await.expect("entity pull succeeds");
        assert!(response_value(&first_pull)["documents"].is_array());
        let first_payload: String = store
            .test_reader()
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
        let second_pull = pull().await.expect("entity pull succeeds");
        assert!(response_value(&second_pull)["documents"].is_array());
        let second_payload: String = store
            .test_reader()
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

    /// Steady writes keep moving the commit generation. A pull whose projections still match
    /// its snapshot must be served while the global maintenance lock is held elsewhere.
    #[tokio::test]
    async fn entity_pull_with_current_projections_does_not_wait_for_the_maintenance_lock() {
        use axum::body::Body;
        use axum::extract::ConnectInfo;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;
        let directory = tempfile::tempdir().expect("isolated state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store opens"),
        );
        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        "agent-1",
                        json!({"id":"agent-1","status":"idle"}).to_string()
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("a source write advances the commit generation");
        let _maintenance = store
            .lock_entity_projection_maintenance()
            .await
            .expect("test holds the maintenance lock");
        let state = crate::router::HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
            directory.path().display().to_string(),
            4321,
            None,
            Some(Arc::clone(&store)),
            None,
        );
        let app = crate::router::router(state).expect("registered HTTP router");
        let mut request = Request::builder()
            .method("GET")
            .uri("/api/sync/pull")
            .header("host", "127.0.0.1:4321")
            .body(Body::empty())
            .expect("pull request");
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:4321"
                .parse::<std::net::SocketAddr>()
                .expect("loopback socket address"),
        ));
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(10), app.oneshot(request))
                .await
                .expect("pull is not queued behind the maintenance lock")
                .expect("HTTP response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cold_entity_pull_reuses_complete_seed_without_scanning_source_rows() {
        const ENTITY_COUNT: usize = 2_000;
        let directory = tempfile::tempdir().expect("isolated state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store opens"),
        );
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
            .await
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
            store.clone(),
            state_dir.clone(),
            validators.clone(),
            0,
            100,
            false,
            0,
            false,
            None,
            0,
            || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                Box::pin(async {})
            },
        )
        .await
        .expect("cold pull succeeds");
        let cold_elapsed = started.elapsed();
        let response = response_value(&response);
        assert_eq!(response["documents"].as_array().unwrap().len(), 100);
        assert_eq!(store.committed_change_generation(), seeded_generation);
        let reader = store.test_reader().expect("store reader");
        assert!(sync_policy::has_complete_entity_seed(&reader).expect("seed marker reads"));
        drop(reader);
        let vault_snapshot = sync_policy::ProjectVaultSnapshot::capture_for_sync(
            &store,
            std::path::Path::new(&state_dir),
        )
        .await
        .expect("vault snapshot");
        assert!(
            !store
                .entity_projections_need_maintenance(&vault_snapshot.signature())
                .expect("projection watermark reads")
        );
        let warm_started = std::time::Instant::now();
        let warm_response = pull_with_maintenance(
            store.clone(),
            state_dir.clone(),
            validators.clone(),
            0,
            100,
            false,
            0,
            false,
            None,
            0,
            || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                Box::pin(async {})
            },
        )
        .await
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
