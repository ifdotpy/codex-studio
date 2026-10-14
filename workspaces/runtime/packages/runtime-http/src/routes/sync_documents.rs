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

pub(crate) async fn protocol(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
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
) -> Result<Json<Value>, ApiError> {
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
    let result = studio_runtime_process::blocking_pool::spawn_blocking("runtime-http/routes/sync_documents", move || -> Result<Value, ApiError> {
        let generation = store.generation().map_err(|_| ApiError::internal())?;
        if let Some(agent) = scope.strip_prefix("transcript:") {
            let mut response = transcript::pull_scope(&store, &scope, agent, after)
                .map_err(super::transcript::map_transcript_error)?;
            // Python samples generation after transcript maintenance commits.
            response["generation"] = json!(store.generation().map_err(|_| ApiError::internal())?);
            if !validators.validate_stored("SyncPullResponse", &response)
                || serde_json::from_value::<crate::generated::SyncPullResponse>(response.clone())
                    .is_err()
            {
                return Err(invalid_entity("sync_entities", "transcript-pull-response").into());
            }
            return Ok(response);
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
                let payload: Value = serde_json::from_str(&document.payload).map_err(|_| {
                    invalid_stored_draft(&document.id)
                })?;
                validate_draft_value(&validators, &document.id, &payload)
                    .map_err(|_| invalid_stored_draft(&document.id))?;
            }
            let checkpoint = documents.last().map_or(after, |document| document.sequence);
            let response = json!({
                "workspaceId": workspace_id,
                "generation": generation,
                "documents": documents.iter().map(|document| document.response()).collect::<Vec<_>>(),
                "checkpoint": {"seq": checkpoint},
            });
            if !validators.validate_stored("SyncPullResponse", &response) {
                return Err(invalid_stored_draft("page"));
            }
            return Ok(response);
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
            Err(StoreError::Sqlite(
                studio_runtime_store::SqliteError::SqliteFailure(error, _),
            )) if error.code == studio_runtime_store::ErrorCode::ReadOnly =>
            {
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
    })
    .await
    .map_err(|_| ApiError::internal())??;
    let response = if result.get("reset").and_then(Value::as_bool) == Some(true) {
        super::generated_json::<crate::generated::SyncPullResetResponse>(result)?
    } else {
        super::generated_json::<crate::generated::SyncPullResponse>(result)?
    };
    Ok(Json(response))
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
) -> Result<Value, ApiError> {
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
        return pull_with_maintenance(
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
        );
    }

    let project_server_id = store.local_server_id().map_err(map_sync_store_error)?;
    let rooms = studio_runtime_services::sync_entity_context::RoomDirectory::prepare(&reader)
        .map_err(map_sync_store_error)?;
    let (response, committed_generation) = store
        .transaction_with_commit_generation(|transaction| {
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
        })
        .map_err(map_sync_store_error)?;
    store
        .mark_entity_projections_maintained(committed_generation, vault_signature)
        .map_err(map_sync_store_error)?;
    drop(maintenance_lock);
    Ok(response)
}

fn capture_project_vault_snapshot(
    store: &RuntimeStore,
    state_dir: &str,
) -> Result<
    (
        studio_runtime_store::Connection,
        sync_policy::ProjectVaultSnapshot,
    ),
    ApiError,
> {
    let reader = store.read().map_err(map_sync_store_error)?;
    let projects =
        studio_runtime_store::ProjectsRepository::list(&reader).map_err(map_sync_store_error)?;
    let snapshot =
        sync_policy::ProjectVaultSnapshot::capture(std::path::Path::new(state_dir), &projects);
    Ok((reader, snapshot))
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
) -> Result<Json<Value>, ApiError> {
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
) -> Result<Value, StoreError> {
    if maintain_projections {
        let vault_snapshot = vault_snapshot.ok_or(StoreError::RecordContract(
            "entity projection maintenance is missing its vault snapshot",
        ))?;
        let rooms = rooms.ok_or(StoreError::RecordContract(
            "entity projection maintenance is missing its room directory",
        ))?;
        if !rooms.matches(transaction)? {
            return Err(StoreError::RecordContract(
                "entity projection inputs changed after filesystem preparation",
            ));
        }
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
    let page = SyncDocumentsRepository::pull_entities(
        transaction,
        after,
        limit,
        fresh,
        initial_high,
        reset_support,
        priority_id,
    )?;
    let workspace_id = SyncDocumentsRepository::workspace_id(transaction)?;
    let response = if page.reset {
        json!({
            "workspaceId": workspace_id,
            "generation": generation,
            "reset": true,
            "floor": page.tombstone_floor,
            "maxSeq": page.max_sequence,
        })
    } else {
        let mut documents = Vec::with_capacity(page.rows.len());
        for row in &page.rows {
            let raw = row
                .payload
                .as_deref()
                .ok_or_else(|| invalid_entity(&row.collection, &row.id))?;
            let payload: Value =
                serde_json::from_str(raw).map_err(|_| invalid_entity(&row.collection, &row.id))?;
            if !validate_entity_envelope(
                &payload,
                &row.collection,
                &row.id,
                row.deleted,
                validators,
            ) {
                return Err(invalid_entity(&row.collection, &row.id));
            }
            documents.push(json!({
                "id": format!("entity:{}:{}", row.collection, row.id),
                "payload": raw,
                "seq": row.sequence,
                "_deleted": row.deleted,
            }));
        }
        json!({
            "workspaceId": workspace_id,
            "generation": generation,
            "documents": documents,
            "checkpoint": {"seq": page.checkpoint},
            "maxSeq": page.max_sequence,
            "initialHigh": page.initial_high,
        })
    };
    let schema = if page.reset {
        "SyncPullResetResponse"
    } else {
        "SyncPullResponse"
    };
    let generated_response_valid = if page.reset {
        serde_json::from_value::<crate::generated::SyncPullResetResponse>(response.clone()).is_ok()
    } else {
        serde_json::from_value::<crate::generated::SyncPullResponse>(response.clone()).is_ok()
    };
    if !validators.validate_stored(schema, &response) || !generated_response_valid {
        return Err(invalid_entity("sync_entities", "pull-response"));
    }
    Ok(response)
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
    use super::pull_with_maintenance;
    use serde_json::{Value, json};
    use studio_runtime_store::{ProjectsRepository, RuntimeStore, SyncDocumentsRepository};

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
        assert!(first_pull["documents"].is_array());
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
        assert!(second_pull["documents"].is_array());
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
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "sync_documents",
        routes: Some(routes),
        dependencies: &["resource-policy/publish-drafts", "sync-policy/entity-seeding", "sync-policy/entity-window-maintenance", "sync-policy/runtime-capabilities"],
    }
}
