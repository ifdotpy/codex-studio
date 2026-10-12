//! HTTP adapter for the durable sync document operations.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use studio_runtime_services::sync_policy::{self, PreferenceScope, SyncPolicyError};
use studio_runtime_store::SyncDocumentsRepository;

use crate::error::ApiError;
use crate::router::HttpState;

const MAX_MUTATION_SYNC_ENTITIES: usize = 500;
const PROTOCOL_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("sync-policy/runtime-capabilities")];
const DRAFT_DEPENDENCIES: [Dependency; 1] = [Dependency::new("resource-policy/publish-drafts")];
const PULL_DEPENDENCIES: [Dependency; 3] = [
    Dependency::new("sync-policy/entity-seeding"),
    Dependency::new("sync-policy/entity-window-maintenance"),
    Dependency::new("transcript-service/pull"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/sync/protocol",
            OperationStatus::partial(&PROTOCOL_DEPENDENCIES),
            protocol,
        ),
        get(
            "/api/sync/pull",
            OperationStatus::partial(&PULL_DEPENDENCIES),
            pull,
        ),
        post(
            "/api/sync/drafts",
            OperationStatus::partial(&DRAFT_DEPENDENCIES),
            push_drafts,
        ),
        post(
            "/api/sync/preferences",
            OperationStatus::Implemented,
            push_preferences,
        ),
    ])
}

pub(crate) async fn protocol() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "GET",
        "/api/sync/protocol",
        &PROTOCOL_DEPENDENCIES,
    ))
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
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        if scope == "state:entities:v1" {
            return Err(dependency_error(
                "GET",
                "/api/sync/pull",
                &PULL_DEPENDENCIES,
            ));
        }
        if scope.starts_with("transcript:") {
            return Err(dependency_error(
                "GET",
                "/api/sync/pull",
                &PULL_DEPENDENCIES,
            ));
        }
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let workspace_id = SyncDocumentsRepository::workspace_id(&connection)
            .map_err(|_| ApiError::internal())?;
        let generation = store.generation().map_err(|_| ApiError::internal())?;
        if scope == "drafts" {
            let documents = SyncDocumentsRepository::pull_drafts(&connection, after, limit)
                .map_err(|_| ApiError::internal())?;
            for document in &documents {
                let payload: Value = serde_json::from_str(&document.payload).map_err(|_| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Invalid stored sync object drafts/{}", document.id),
                    )
                })?;
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
        Err(dependency_error(
            "GET",
            "/api/sync/pull",
            &PULL_DEPENDENCIES,
        ))
    })
    .await
    .map_err(|_| ApiError::internal())??;
    let response = if result.get("reset").and_then(Value::as_bool) == Some(true) {
        generated_json::<crate::generated::SyncPullResetResponse>(result)?
    } else {
        generated_json::<crate::generated::SyncPullResponse>(result)?
    };
    Ok(Json(response))
}

#[derive(Debug, Deserialize)]
pub(crate) struct DraftPushRequest {
    rows: Vec<DraftPushRow>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PreferencePushRequest {
    scope: String,
    #[serde(default)]
    fields: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct DraftPushRow {
    #[serde(rename = "newDocumentState")]
    new_document_state: DraftDocumentInput,
    #[serde(rename = "assumedMasterState")]
    assumed_master_state: Option<DraftDocumentInput>,
}

#[derive(Debug, Deserialize)]
struct DraftDocumentInput {
    id: String,
    payload: String,
    #[serde(default, rename = "_deleted")]
    _deleted: bool,
    #[serde(default, rename = "seq")]
    _seq: Option<i64>,
}

pub(crate) async fn push_drafts(
    ApiJson(parsed): ApiJson<DraftPushRequest>,
) -> Result<Json<Value>, ApiError> {
    let has_rows = !parsed.rows.is_empty();
    for (index, row) in parsed.rows.into_iter().enumerate() {
        let payload: Value = serde_json::from_str(&row.new_document_state.payload)
            .map_err(|_| draft_validation_error(index, "newDocumentState"))?;
        sync_policy::validate_draft(&row.new_document_state.id, &payload)
            .map_err(|_| draft_validation_error(index, "newDocumentState"))?;
        if let Some(assumed) = row.assumed_master_state {
            let assumed_payload: Value = serde_json::from_str(&assumed.payload)
                .map_err(|_| draft_validation_error(index, "assumedMasterState"))?;
            sync_policy::validate_draft(&assumed.id, &assumed_payload)
                .map_err(|_| draft_validation_error(index, "assumedMasterState"))?;
        }
    }
    if !has_rows {
        return Ok(Json(generated_json::<Vec<crate::generated::SyncDocument>>(
            Value::Array(Vec::new()),
        )?));
    }
    Err(dependency_error(
        "POST",
        "/api/sync/drafts",
        &DRAFT_DEPENDENCIES,
    ))
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
    Ok(Json(generated_json::<
        crate::generated::PreferencePushResponse,
    >(response)?))
}

fn generated_json<T>(value: Value) -> Result<Value, ApiError>
where
    T: DeserializeOwned + Serialize,
{
    let dto = serde_json::from_value::<T>(value).map_err(|_| ApiError::internal())?;
    serde_json::to_value(dto).map_err(|_| ApiError::internal())
}

fn draft_validation_error(index: usize, document: &str) -> ApiError {
    ApiError::validation(vec![
        Value::String("body".to_owned()),
        Value::String("rows".to_owned()),
        Value::String(index.to_string()),
        Value::String(document.to_owned()),
    ])
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
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
