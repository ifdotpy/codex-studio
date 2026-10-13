//! HTTP adapter for the durable sync document operations.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use studio_runtime_services::sync_documents::{self, DraftWrite};
use studio_runtime_services::sync_policy::{self, PreferenceScope, SyncPolicyError};
use studio_runtime_store::SyncDocumentsRepository;

use crate::error::ApiError;
use crate::router::HttpState;

const MAX_MUTATION_SYNC_ENTITIES: usize = 500;
const SYNC_STREAM_BYTE_LIMIT: i64 = 1_048_576;

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
    let unix_socket = tokio::task::spawn_blocking(move || socket_path.exists())
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
        "protocolVersion": 3,
        "supportedVersions": [3],
        "capabilities": capabilities,
        "scopes": ["state:entities:v1", "transcript:<agent-id>", "drafts"],
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
        .unwrap_or("state:entities:v1")
        .to_owned();
    if scope != "drafts"
        && scope != "state:entities:v1"
        && !(scope.starts_with("transcript:") && scope.chars().count() < 300)
    {
        return Err(ApiError::validation(vec![
            Value::String("query".to_owned()),
            Value::String("scope".to_owned()),
        ]));
    }
    let after = query_integer(&query, "after", 0)?.max(0);
    let limit = sync_policy::effective_limit(&scope, query_integer(&query, "limit", 100)?);
    let fresh = query.typed_value("fresh").and_then(Value::as_str) == Some("1");
    let initial_high = query_integer(&query, "initialHigh", 0)?.max(0);
    let reset_support = query.typed_value("reset").and_then(Value::as_str) == Some("1");
    let priority_id = query
        .typed_value("priorityId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if scope.starts_with("transcript:") {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires transcript-service/pull",
        ));
    }
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let state_dir = state.state_dir.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let generation = store.generation().map_err(|_| ApiError::internal())?;
        if scope == "drafts" {
            let connection = store.read().map_err(|_| ApiError::internal())?;
            let workspace_id = SyncDocumentsRepository::workspace_id(&connection)
                .map_err(|_| ApiError::internal())?;
            let documents = SyncDocumentsRepository::pull_drafts(&connection, after, limit)
                .map_err(|_| ApiError::internal())?;
            for document in &documents {
                let payload: Value = serde_json::from_str(&document.payload).map_err(|_| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Invalid stored sync object drafts/{}", document.id),
                    )
                })?;
                if !validators.validate_stored("DraftPayload", &payload) {
                    return Err(ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Invalid stored sync object drafts/{}", document.id),
                    ));
                }
                sync_policy::validate_stored_draft(&document.id, &payload).map_err(|_| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Invalid stored sync object drafts/{}", document.id),
                    )
                })?;
            }
            let checkpoint = documents.last().map_or(after, |document| document.sequence);
            return Ok(json!({
                "workspaceId": workspace_id,
                "generation": generation,
                "documents": documents.iter().map(|document| document.response()).collect::<Vec<_>>(),
                "checkpoint": {"seq": checkpoint},
            }));
        }
        let page = store
            .transaction(|transaction| {
                sync_policy::maintain_entity_projections(transaction, &state_dir)?;
                let page = SyncDocumentsRepository::pull_entities(
                    transaction,
                    after,
                    limit,
                    fresh,
                    initial_high,
                    reset_support,
                    priority_id.as_deref(),
                )?;
                Ok(page)
            })
            .map_err(|_| ApiError::internal())?;
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let workspace_id = SyncDocumentsRepository::workspace_id(&connection)
            .map_err(|_| ApiError::internal())?;
        if page.reset {
            return Ok(json!({
                "workspaceId": workspace_id,
                "generation": generation,
                "reset": true,
                "floor": page.tombstone_floor,
                "maxSeq": page.max_sequence,
            }));
        }
        let mut documents = Vec::with_capacity(page.rows.len());
        for row in &page.rows {
            let raw = row
                .payload
                .as_deref()
                .ok_or_else(|| invalid_entity(&row.collection, &row.id))?;
            let payload: Value = serde_json::from_str(raw)
                .map_err(|_| invalid_entity(&row.collection, &row.id))?;
            validate_entity_envelope(&payload, &row.collection, &row.id, row.deleted)
                .map_err(|_| invalid_entity(&row.collection, &row.id))?;
            documents.push(json!({
                "id": format!("entity:{}:{}", row.collection, row.id),
                "payload": raw,
                "seq": row.sequence,
                "_deleted": row.deleted,
            }));
        }
        Ok(json!({
            "workspaceId": workspace_id,
            "generation": generation,
            "documents": documents,
            "checkpoint": {"seq": page.checkpoint},
            "maxSeq": page.max_sequence,
            "initialHigh": page.initial_high,
        }))
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
                let payload: Value = serde_json::from_str(&assumed.payload)
                    .map_err(|_| draft_validation_error(index, "assumedMasterState", "payload"))?;
                let canonical =
                    serde_json::to_string(&payload).map_err(|_| ApiError::internal())?;
                Ok::<_, ApiError>((canonical, assumed.deleted.unwrap_or(false)))
            })
            .transpose()?;
        writes.push(DraftWrite {
            id: row.new_document_state.id,
            payload: serde_json::to_string(&payload).map_err(|_| ApiError::internal())?,
            deleted: row.new_document_state.deleted.unwrap_or(false),
            assumed_payload: assumed.as_ref().map(|(payload, _)| payload.clone()),
            assumed_deleted: assumed.is_some_and(|(_, deleted)| deleted),
        });
    }
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let wake = state.event_wake.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result =
            sync_documents::push_drafts(&store, &writes).map_err(|_| ApiError::internal())?;
        if result.changed {
            wake.notify_one();
        }
        Ok::<_, ApiError>(result.conflicts)
    })
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
    let (merged, sync_entities, sync_after) =
        tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
            let before_connection = store.read().map_err(|_| ApiError::internal())?;
            let before = SyncDocumentsRepository::entity_sequence(&before_connection)
                .map_err(|_| ApiError::internal())?;
            drop(before_connection);
            let merged = sync_policy::push_preferences(&store, scope, &fields, move |field| {
                validators.validate_preference_field(field)
            })
            .map_err(|error| match error {
                SyncPolicyError::InvalidPreferences => {
                    ApiError::validation(vec![Value::String("body".to_owned())])
                }
                SyncPolicyError::InvalidStoredObject { .. } => {
                    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                }
                _ => ApiError::internal(),
            })?;
            let connection = store.read().map_err(|_| ApiError::internal())?;
            let rows = SyncDocumentsRepository::entities_after(
                &connection,
                before,
                i64::try_from(MAX_MUTATION_SYNC_ENTITIES + 1).unwrap_or(i64::MAX),
            )
            .map_err(|_| ApiError::internal())?;
            let floor = SyncDocumentsRepository::entity_tombstone_floor(&connection)
                .map_err(|_| ApiError::internal())?;
            let sync_after =
                (rows.len() <= MAX_MUTATION_SYNC_ENTITIES && floor <= before).then_some(before);
            Ok((merged, rows, sync_after))
        })
        .await
        .map_err(|_| ApiError::internal())??;
    let mut response = json!({"fields": merged});
    if !sync_entities.is_empty() {
        response["_syncEntities"] = Value::Array(
            sync_entities
                .iter()
                .take(MAX_MUTATION_SYNC_ENTITIES)
                .map(|row| {
                    json!({
                        "id": format!("entity:{}:{}", row.collection, row.id),
                        "seq": row.sequence,
                        "payload": row.payload,
                        "_deleted": row.deleted,
                    })
                })
                .collect(),
        );
        if let Some(sync_after) = sync_after {
            response["_syncEntitiesAfter"] = json!(sync_after);
        }
    }
    Ok(Json(super::generated_json::<
        crate::generated::PreferencePushResponse,
    >(response)?))
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
) -> Result<(), ()> {
    let Some(object) = payload.as_object() else {
        return Err(());
    };
    if object.len() != 3
        || object.get("collection").and_then(Value::as_str) != Some(collection)
        || object.get("id").and_then(Value::as_str) != Some(id)
        || !object.get("value").is_some_and(Value::is_object)
    {
        return Err(());
    }
    if deleted {
        return object
            .get("value")
            .and_then(Value::as_object)
            .filter(|value| value.is_empty())
            .map(|_| ())
            .ok_or(());
    }
    macro_rules! matches_schema {
        ($schema:ty) => {
            serde_json::from_value::<$schema>(payload.clone()).is_ok()
        };
    }
    let valid = match collection {
        "agent" => matches_schema!(crate::generated::SyncAgentEntityPayload),
        "chat" => matches_schema!(crate::generated::SyncChatEntityPayload),
        "complaint" => matches_schema!(crate::generated::SyncComplaintEntityPayload),
        "edge" => matches_schema!(crate::generated::SyncEdgeEntityPayload),
        "event" => matches_schema!(crate::generated::SyncEventEntityPayload),
        "monitor" => matches_schema!(crate::generated::SyncMonitorEntityPayload),
        "peerTeam" => matches_schema!(crate::generated::SyncPeerTeamEntityPayload),
        "project" => matches_schema!(crate::generated::SyncProjectEntityPayload),
        "request" => matches_schema!(crate::generated::SyncRequestEntityPayload),
        "room" => matches_schema!(crate::generated::SyncRoomEntityPayload),
        "rule" => matches_schema!(crate::generated::SyncRuleEntityPayload),
        "task" => matches_schema!(crate::generated::SyncTaskEntityPayload),
        "uiPreferences" => matches_schema!(crate::generated::SyncUiPreferencesEntityPayload),
        "work" => matches_schema!(crate::generated::SyncWorkEntityPayload),
        "workspace" => matches_schema!(crate::generated::SyncWorkspaceEntityPayload),
        _ => false,
    };
    if valid { Ok(()) } else { Err(()) }
}

fn invalid_entity(collection: &str, id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored sync object {collection}/{id}"),
    )
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
