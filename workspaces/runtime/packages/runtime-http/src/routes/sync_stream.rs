//! HTTP adapter for the versioned sync event stream.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::convert::Infallible;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::generated::{
    DraftChangeBatch, EntityChangeBatch, ResourceChangeEvent, ResourceChangeEventReason,
    ResourceRef, ResourceRevisionEntry, TranscriptChangeBatch, TranscriptStreamDelta,
    TranscriptSyncPage,
};
use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, EventDataWriter, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::{
    MAX_SYNC_RESOURCE_COUNT, MAX_SYNC_RESOURCE_QUERY_BYTES, SYNC_DRAFT_STREAM_MAX_DOCUMENTS,
    SYNC_ENTITY_STREAM_MAX_DOCUMENTS, SYNC_RESOURCE_HEARTBEAT_SECONDS, SYNC_STREAM_EVENT_MAX_BYTES,
    SYNC_TRANSCRIPT_STREAM_MAX_ITEMS,
};
use studio_runtime_schema::{SYNC_PROTOCOL_VERSION, SYNC_PROTOCOL_VERSION_TEXT};
use studio_runtime_services::change_notifications::{
    ChangeSnapshot, MAX_SAFE_REVISION, SYNC_FRAME_MIN_INTERVAL,
};
use studio_runtime_services::resource_notify::{
    ResourceChanges, ResourceNotifications, ResourceNotifyError,
};
#[cfg(test)]
use studio_runtime_services::token_rate::TokenRateHub;
use studio_runtime_services::token_rate::TokenRateSnapshot as TokenRates;
use studio_runtime_store::{Connection, RuntimeStore, SyncDocumentsRepository};
use tokio::time::{Instant, Interval, interval_at};

use super::extractors::{ApiHeaders, ApiJson, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get, post};
use crate::error::ApiError;
use crate::router::HttpState;
use crate::schema::{API_SCHEMA_HASH_HEADER, api_schema_hash};

/// P18's catalog route calls this after the shared boundary authenticates and validates.
pub(crate) async fn notify_resources(
    state: HttpState,
    body: Value,
) -> Result<Json<crate::generated::ResourceNotifyAck>, ApiError> {
    let notifications = state.resource_notifications;
    let request_id = body["requestId"].clone();
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/sync_stream",
        move || notifications.publish_once(&body),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|error| match error {
        ResourceNotifyError::Conflict => ApiError::new(StatusCode::CONFLICT, error.to_string()),
        ResourceNotifyError::Invalid => ApiError::validation(vec![json!("body")]),
        ResourceNotifyError::Unavailable => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
        }
    })?;
    Ok(Json(super::generated_json::<
        crate::generated::ResourceNotifyAck,
    >(
        json!({"requestId": request_id, "accepted": true})
    )?))
}

/// Sessions share the same authenticated HTTP boundary as their streams. The
/// stream records no per-user identity: local and browser callers share HttpState.
#[derive(Default)]
pub(crate) struct SyncSessions(Mutex<BTreeMap<String, SyncSession>>);

struct SyncSession {
    resources: Vec<Value>,
    generation: i64,
    sender: tokio::sync::mpsc::Sender<SubscriptionChange>,
}

struct SubscriptionChange {
    generation: i64,
    resources: Vec<Value>,
    added: Vec<Value>,
    token_rates: Option<bool>,
}

struct LiveSubscription {
    id: String,
    sessions: Arc<SyncSessions>,
    receiver: tokio::sync::mpsc::Receiver<SubscriptionChange>,
}

impl Drop for LiveSubscription {
    fn drop(&mut self) {
        self.sessions
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/sync/stream",
            OperationStatus::Implemented,
            sync_stream,
        ),
        post(
            "/api/sync/subscriptions",
            OperationStatus::Implemented,
            sync_subscriptions,
        ),
    ])
}

async fn sync_subscriptions(
    State(state): State<HttpState>,
    ApiJson(request): ApiJson<crate::generated::SyncSubscriptionRequest>,
) -> Result<Json<crate::generated::SyncSubscriptionAck>, ApiError> {
    for resources in [&request.add, &request.remove] {
        if resources.len() > MAX_SYNC_RESOURCE_COUNT
            || serde_json::to_vec(resources)
                .map_err(|_| ApiError::internal())?
                .len()
                > MAX_SYNC_RESOURCE_QUERY_BYTES
        {
            return Err(ApiError::validation(vec![json!("body")]));
        }
    }
    let mut sessions = state
        .sync_sessions
        .0
        .lock()
        .map_err(|_| ApiError::internal())?;
    let session = sessions.get_mut(&request.session_id).ok_or_else(|| {
        ApiError::new(StatusCode::GONE, "Unknown sync session; reopen the stream")
    })?;
    if session.sender.is_closed() {
        return Err(ApiError::new(
            StatusCode::GONE,
            "Expired sync session; reopen the stream",
        ));
    }
    if request.r#gen <= session.generation {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Stale subscription generation; reopen the stream",
        ));
    }
    let remove = request
        .remove
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::internal())?;
    let mut resources = session
        .resources
        .iter()
        .filter(|resource| !remove.contains(resource))
        .cloned()
        .collect::<Vec<_>>();
    let mut added = Vec::new();
    for resource in request.add {
        let value = serde_json::to_value(resource).map_err(|_| ApiError::internal())?;
        if !resources.contains(&value) {
            added.push(value.clone());
            resources.push(value);
        }
    }
    if resources.len() > MAX_SYNC_RESOURCE_COUNT
        || serde_json::to_vec(&resources)
            .map_err(|_| ApiError::internal())?
            .len()
            > MAX_SYNC_RESOURCE_QUERY_BYTES
    {
        return Err(ApiError::validation(vec![json!("body"), json!("add")]));
    }
    session
        .sender
        .try_send(SubscriptionChange {
            generation: request.r#gen,
            resources: resources.clone(),
            added,
            token_rates: request.token_rates,
        })
        .map_err(|_| {
            ApiError::new(
                StatusCode::CONFLICT,
                "Subscription update pending; reopen the stream",
            )
        })?;
    session.resources = resources;
    session.generation = request.r#gen;
    Ok(Json(crate::generated::SyncSubscriptionAck {
        r#gen: request.r#gen,
    }))
}

async fn sync_stream(
    query: QueryParams,
    ApiHeaders(headers): ApiHeaders,
    request_headers: HeaderMap,
    State(state): State<HttpState>,
) -> Response {
    let protocol = query.typed_value("protocol").and_then(Value::as_str);
    let header_protocol = headers.get("x-codex-sync-protocol").and_then(Value::as_str);
    if protocol.is_some_and(|value| value != SYNC_PROTOCOL_VERSION_TEXT)
        || header_protocol.is_some_and(|value| value != SYNC_PROTOCOL_VERSION_TEXT)
        || (protocol != Some(SYNC_PROTOCOL_VERSION_TEXT)
            && header_protocol != Some(SYNC_PROTOCOL_VERSION_TEXT))
    {
        return (
            StatusCode::UPGRADE_REQUIRED,
            Json(json!({
                "error": "Unsupported sync protocol version",
                "supportedVersions": [SYNC_PROTOCOL_VERSION],
            })),
        )
            .into_response();
    }

    let server_hash = api_schema_hash();
    let renderer_hash = request_headers
        .get(API_SCHEMA_HASH_HEADER)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            query
                .typed_value(crate::schema::API_SCHEMA_HASH_PARAM)
                .and_then(Value::as_str)
        });
    if renderer_hash.is_some_and(|value| value != server_hash) {
        return encoded_event_response(
            stream::iter([Ok(sse_event(
                "api-schema",
                None,
                json!({"hash": server_hash, (crate::schema::API_SCHEMA_MISMATCH_FIELD): true}),
            ))]),
            &request_headers,
        );
    }

    let resources = match parse_resources(&query, &state) {
        Ok(resources) => resources,
        Err(error) => return error.into_response(),
    };
    let vault_readiness = match readiness_source(&state, &resources).await {
        Ok(source) => source,
        Err(error) => return error.into_response(),
    };
    let store = match state.store.clone() {
        Some(store) => store,
        None => return ApiError::internal().into_response(),
    };
    let change_hub = Arc::clone(&state.sync_changes);
    let mut changes = change_hub.subscribe();
    changes.borrow_and_update();
    let initial_entity_sequence = match read_entity_sequence(Some(store.clone()), &resources).await
    {
        Ok(sequence) => sequence,
        Err(error) => return error.into_response(),
    };
    let initial_draft_sequence =
        match read_draft_changes(Some(store.clone()), &resources, None).await {
            Ok((sequence, _)) => sequence,
            Err(error) => return error.into_response(),
        };
    let reconnect = headers.contains_key("last-event-id")
        || query
            .typed_value("reconnect")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let reason = if reconnect {
        studio_runtime_services::diagnostics::record_sync_recovery(
            studio_runtime_services::diagnostics::SyncRecoveryKind::Reconnect,
        );
        ResourceChangeEventReason::Reconnect
    } else {
        ResourceChangeEventReason::Initial
    };
    let view = match state
        .resource_notifications
        .changes_since_with_content(&resources, None, |resource| {
            resource_content(state.clone(), resource)
        })
        .await
    {
        Ok(view) if !view.cursor.closed => view,
        _ => return ApiError::internal().into_response(),
    };
    let mut voice_sequences = BTreeMap::new();
    let mut transcript_sequences = BTreeMap::new();
    let transcript_updates = read_transcript_changes(
        Some(store.clone()),
        &view.resources,
        &mut transcript_sequences,
        false,
    )
    .await;
    let subscription_snapshot = view.cursor.clone();
    let mut initial_event = match resource_changes_event(
        &state.workspace_id,
        &view,
        reason,
        initial_entity_sequence,
        None,
        initial_draft_sequence,
        None,
    ) {
        Ok(event) => event,
        Err(error) => return error.into_response(),
    };
    attach_transcript_changes(&mut initial_event, transcript_updates);
    attach_transient_snapshots(Some(&state), &mut initial_event, &mut voice_sequences).await;
    let mut initial = VecDeque::new();
    let session_id = uuid::Uuid::new_v4().to_string();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    match state.sync_sessions.0.lock() {
        Ok(mut sessions) => {
            sessions.insert(
                session_id.clone(),
                SyncSession {
                    resources: resources.clone(),
                    generation: 0,
                    sender,
                },
            );
        }
        Err(_) => return ApiError::internal().into_response(),
    }
    let subscription = LiveSubscription {
        id: session_id.clone(),
        sessions: Arc::clone(&state.sync_sessions),
        receiver,
    };
    initial.push_back(sse_event(
        "session",
        None,
        crate::generated::SyncSessionEvent { session_id },
    ));
    if renderer_hash.is_some() {
        initial.push_back(sse_event("api-schema", None, json!({"hash": server_hash})));
    }
    initial.push_back(resource_sse_event(
        subscription_snapshot.revision,
        initial_event,
    ));
    append_progress_frames(&state, &view, &mut initial).await;
    // OpenAPI owns the omitted-selector compatibility default; the HTTP witness
    // keeps this native/signed-client behavior in parity with that contract.
    let token_interest = match query
        .typed_value("tokenRates")
        .map(bool::deserialize)
        .transpose()
    {
        Ok(interest) => interest.unwrap_or(true),
        Err(_) => return ApiError::internal().into_response(),
    };
    let mut token_rates = token_interest.then(|| state.token_rates.subscribe());
    let mut last_token_rates = None;
    if let Some(receiver) = &mut token_rates {
        let (rates, cursor) = token_snapshot(receiver, || changes.borrow().clone());
        initial.push_back(token_rate_event(&state.workspace_id, &cursor, &rates));
        last_token_rates = Some(rates);
    }

    let stream_state = ResourceStream {
        http_state: Some(state.clone()),
        vault_readiness,
        subscription: Some(subscription),
        initial,
        workspace_id: state.workspace_id,
        epoch: subscription_snapshot.epoch,
        last_revision: subscription_snapshot.revision,
        resources,
        entity_sequence: initial_entity_sequence,
        transcript_sequences,
        voice_sequences,
        draft_sequence: initial_draft_sequence,
        store: Some(store),
        changes,
        notifications: state.resource_notifications,
        token_rates,
        last_token_rates,
        consecutive_token_frames: 0,
        next_token_frame_at: None,
        next_resources_frame_at: None,
        heartbeat: interval_at(
            Instant::now() + Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
            Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
        ),
    };
    encoded_event_response(stream::unfold(stream_state, next_frame), &request_headers)
}

/// Read the same projections as the resource endpoints. Unsupported resources
/// and failed reads retain broad invalidation; this path never starts providers,
/// local-cost scanners, terminal processes, or projection maintenance writes.
async fn resource_content(state: HttpState, resource: Value) -> Option<Vec<u8>> {
    use crate::generated::ResourceRef;
    let resource: ResourceRef = serde_json::from_value(resource).ok()?;
    match resource {
        ResourceRef::DraftsResource(_) => state
            .store?
            .read_async(
                "runtime-http/routes/sync_stream/draft-revision",
                |connection| {
                    let sequence = SyncDocumentsRepository::draft_sequence(connection)?;
                    serde_json::to_vec(&sequence).map_err(studio_runtime_store::StoreError::from)
                },
            )
            .await
            .ok(),
        ResourceRef::TranscriptResource(resource) => {
            // A scoped stream hint may precede the next stored transcript flush.
            let publication = state
                .resource_notifications
                .published_revision(&serde_json::to_value(&resource).ok()?)
                .ok()?;
            let store = state.store?;
            store
                .read_async(
                    "runtime-http/routes/sync_stream/transcript-revision",
                    move |connection| {
                        let revision = studio_runtime_store::TranscriptRepository::revision(
                            connection,
                            &resource.agent_id,
                        )?;
                        serde_json::to_vec(&(revision, publication))
                            .map_err(studio_runtime_store::StoreError::from)
                    },
                )
                .await
                .ok()
        }
        ResourceRef::AuthProgressResource(resource) => serde_json::to_vec(
            &state
                .claude_login?
                .progress(&resource.request_id)
                .await
                .ok()?,
        )
        .ok(),
        ResourceRef::VaultReadinessResource(resource) => {
            let settings = super::host_secrets::settings_dir(&state.state_dir);
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/vault-readiness",
                move || studio_runtime_services::vault::keys_ready(&settings, &resource.prefix),
            )
            .await
            .ok()?
            .ok()
            .and_then(|ready| serde_json::to_vec(&ready).ok())
        }
        ResourceRef::MultiServerResource(_) if cfg!(unix) => {
            serde_json::to_vec(&super::multi_server::read_snapshot(&state).await.ok()?).ok()
        }
        ResourceRef::VoiceResource(resource) => state
            .store?
            .read_async("runtime-http/voice-resource-revision", move |connection| {
                let cursor = studio_runtime_store::VoiceRepository::record_sequence(
                    connection,
                    &resource.agent_id,
                )?;
                let session = studio_runtime_store::VoiceRepository::latest_session(
                    connection,
                    &resource.agent_id,
                )?;
                serde_json::to_vec(&(cursor, session))
                    .map_err(studio_runtime_store::StoreError::from)
            })
            .await
            .ok(),
        ResourceRef::AccountsResource(_) => None,
        ResourceRef::LimitsResource(resource) => {
            let mut snapshot = if let Some(port) = &state.account_session {
                port.limits_snapshot(&resource.account_key).ok()?
            } else {
                state.account_policy.as_ref()?.limits(&resource.account_key)
            };
            // Observation receipts advance privately without changing rateLimit entities.
            // Fingerprint the same semantic content so unrelated commits cannot
            // turn a timestamp refresh into another provider read by the client.
            if let Some(fields) = snapshot.as_object_mut() {
                for field in studio_runtime_store::account_state::LIMITS_OBSERVATION_TIME_FIELDS {
                    fields.remove(*field);
                }
            }
            let snapshot: crate::generated::UsageLimitsResponse =
                serde_json::from_value(snapshot).ok()?;
            serde_json::to_vec(&snapshot).ok()
        }
        ResourceRef::DesktopResource(_) | ResourceRef::TerminalsResource(_) => None,
        ResourceRef::QueueResource(_) => None,
        ResourceRef::SessionCostResource(_) | ResourceRef::CostsResource(_) => None,
        _ => None,
    }
}

/// Only sessions with a live readiness selector own a filesystem source. Dropping
/// the stream or removing that selector drops its OS watcher as well.
async fn readiness_source(
    state: &HttpState,
    resources: &[Value],
) -> Result<Option<studio_runtime_services::vault::VaultReadinessWatcher>, ApiError> {
    if !resources
        .iter()
        .any(|resource| resource["kind"] == "vault-readiness")
    {
        return Ok(None);
    }
    let settings = super::host_secrets::settings_dir(&state.state_dir);
    let changes = Arc::clone(&state.sync_changes);
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/vault-readiness-source",
        move || {
            studio_runtime_services::vault::watch_readiness(
                &settings,
                Arc::new(move || changes.notify_one()),
            )
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map(Some)
    .map_err(|_| ApiError::internal())
}

/// Progress is deliberately separate from resources/entityChanges: OAuth links
/// must never enter SQLite, RxDB, entity replay, or durable checkpoints.
async fn append_progress_frames(
    state: &HttpState,
    view: &ResourceChanges,
    frames: &mut VecDeque<Event>,
) {
    for (resource, revision) in view.resources.iter().zip(&view.versions) {
        let value = match resource["kind"].as_str() {
            Some("auth-progress") => match (&state.claude_login, resource["requestId"].as_str()) {
                (Some(login), Some(id)) => login.progress(id).await.unwrap_or(Value::Null),
                _ => Value::Null,
            },
            Some("vault-readiness") => {
                let Some(bytes) = resource_content(state.clone(), resource.clone()).await else {
                    continue;
                };
                let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                value
            }
            _ => continue,
        };
        let payload = json!({
            "protocol": SYNC_PROTOCOL_VERSION,
            "workspaceId": state.workspace_id,
            "epoch": view.cursor.epoch,
            "revision": revision,
            "resource": resource,
            "value": value,
        });
        if state
            .validators
            .validate_component("ResourceProgressEvent", &payload)
        {
            frames.push_back(sse_event("resource-progress", None, payload));
        }
    }
}

fn parse_resources(query: &QueryParams, state: &HttpState) -> Result<Vec<Value>, ApiError> {
    let invalid = || {
        ApiError::validation(vec![
            Value::String("query".to_owned()),
            Value::String("resources".to_owned()),
        ])
    };
    let raw = query
        .typed_value("resources")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if raw.len() > MAX_SYNC_RESOURCE_QUERY_BYTES {
        return Err(invalid());
    }
    let parsed: Value = serde_json::from_str(raw).map_err(|_| invalid())?;
    let values = parsed.as_array().ok_or_else(invalid)?;
    if values.len() > MAX_SYNC_RESOURCE_COUNT
        || values
            .iter()
            .any(|resource| !state.validators.validate_component("ResourceRef", resource))
    {
        return Err(invalid());
    }
    let mut seen = BTreeSet::new();
    Ok(values
        .iter()
        .filter(|value| seen.insert(value.to_string()))
        .cloned()
        .collect())
}

async fn read_entity_sequence(
    store: Option<Arc<RuntimeStore>>,
    resources: &[Value],
) -> Result<i64, ApiError> {
    Ok(read_entity_changes(store, resources, None).await?.0)
}

/// One SQLite snapshot proves the advertised interval, including the tombstone floor.
/// A missing, oversized or unreadable interval carries only its revision for pull recovery.
async fn read_entity_changes(
    store: Option<Arc<RuntimeStore>>,
    resources: &[Value],
    after: Option<i64>,
) -> Result<(i64, Option<EntityChangeBatch>), ApiError> {
    let resources = resources
        .iter()
        .cloned()
        .map(serde_json::from_value::<ResourceRef>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::internal())?;
    if !resources
        .iter()
        .any(|resource| matches!(resource, ResourceRef::StateResource(_)))
    {
        return Ok((after.unwrap_or(0), None));
    }
    store
        .ok_or_else(ApiError::internal)?
        .read_async("runtime-http/routes/sync_stream", move |connection| {
            let floor = SyncDocumentsRepository::entity_tombstone_floor(connection)
                .map_err(|_| ApiError::internal())?;
            let sequence = SyncDocumentsRepository::entity_sequence(connection)
                .map_err(|_| ApiError::internal())?
                .max(floor);
            if !(0..=MAX_SAFE_REVISION).contains(&sequence) {
                return Err(ApiError::internal());
            }
            let batch = entity_change_batch(connection, after, floor, sequence);
            Ok((sequence, batch))
        })
        .await
}

/// Convert the complete record interval; final SSE serialization owns the byte limit.
fn entity_change_batch(
    connection: &Connection,
    after: Option<i64>,
    floor: i64,
    through: i64,
) -> Option<EntityChangeBatch> {
    let after = after?;
    if after < floor || after >= through {
        return None;
    }
    let rows = SyncDocumentsRepository::entities_after(
        connection,
        after,
        SYNC_ENTITY_STREAM_MAX_DOCUMENTS + 1,
    )
    .ok()?;
    if rows.len() as i64 > SYNC_ENTITY_STREAM_MAX_DOCUMENTS {
        return None;
    }
    let mut documents = Vec::with_capacity(rows.len());
    for row in &rows {
        documents.push(super::sync_documents::entity_document(row)?);
    }
    if documents.last()?.seq != through {
        return None;
    }
    let batch = EntityChangeBatch {
        after,
        through,
        documents,
    };
    Some(batch)
}

enum TranscriptStreamUpdate {
    Unchanged,
    Changed {
        revision: i64,
        batch: Option<Box<TranscriptChangeBatch>>,
    },
}

/// Reuse the pull projection owner only for transcript scopes hinted in this flush.
/// Its existing revision cache shares page computation across stream subscribers.
async fn read_transcript_changes(
    store: Option<Arc<RuntimeStore>>,
    resources: &[Value],
    sequences: &mut BTreeMap<String, i64>,
    complete: bool,
) -> BTreeMap<String, TranscriptStreamUpdate> {
    let mut updates: BTreeMap<String, TranscriptStreamUpdate> = BTreeMap::new();
    let mut inline_bytes = Some(SYNC_STREAM_EVENT_MAX_BYTES);
    let Some(store) = store else {
        return updates;
    };
    for resource in resources {
        let Ok(ResourceRef::TranscriptResource(resource)) =
            serde_json::from_value(resource.clone())
        else {
            continue;
        };
        let agent = resource.agent_id;
        let scope = format!("transcript:{agent}");
        let after = sequences.get(&agent).copied();
        let result = studio_runtime_services::transcript::pull_scope(
            store.clone(),
            &scope,
            &agent,
            after.unwrap_or(0),
        )
        .await;
        let response = match result
            .ok()
            .and_then(|value| serde_json::from_value::<TranscriptSyncPage>(value).ok())
        {
            Some(response) => response,
            None => {
                sequences.remove(&agent);
                continue;
            }
        };
        let through = response.checkpoint.seq;
        if !(0..=MAX_SAFE_REVISION).contains(&through) {
            sequences.remove(&agent);
            continue;
        }
        if complete && after == Some(through) && response.documents.is_empty() {
            updates.insert(agent, TranscriptStreamUpdate::Unchanged);
            continue;
        }
        let batch = after
            .filter(|after| complete && *after < through)
            .and_then(|after| {
                // The pull payload is already serialized. Its byte length bounds
                // retained inline data without serializing each batch again. The
                // final SSE writer accounts for the exact envelope and metadata.
                let remaining = inline_bytes?;
                let bytes = response
                    .documents
                    .iter()
                    .map(|document| document.payload.len())
                    .sum();
                inline_bytes = remaining.checked_sub(bytes);
                if inline_bytes.is_none() {
                    for update in updates.values_mut() {
                        if let TranscriptStreamUpdate::Changed { batch, .. } = update {
                            *batch = None;
                        }
                    }
                    return None;
                }
                transcript_change_batch(response, &scope, after)
            })
            .map(Box::new);
        sequences.insert(agent.clone(), through);
        updates.insert(
            agent,
            TranscriptStreamUpdate::Changed {
                revision: through,
                batch,
            },
        );
    }
    updates
}

fn transcript_change_batch(
    response: TranscriptSyncPage,
    scope: &str,
    after: i64,
) -> Option<TranscriptChangeBatch> {
    let through = response.checkpoint.seq;
    if !(0..=MAX_SAFE_REVISION).contains(&after)
        || !(after + 1..=MAX_SAFE_REVISION).contains(&through)
        || response.documents.len() != 1
    {
        return None;
    }
    let document = response.documents.into_iter().next()?;
    if document.id != scope || document.seq != through || document.deleted {
        return None;
    }
    let payload: Value = serde_json::from_str(&document.payload).ok()?;
    if !studio_runtime_schema::validate("TranscriptStreamDelta", &payload).ok()? {
        return None;
    }
    let delta: TranscriptStreamDelta = serde_json::from_value(payload).ok()?;
    if delta.items.len() + delta.removed.len() > SYNC_TRANSCRIPT_STREAM_MAX_ITEMS {
        return None;
    }
    let batch = TranscriptChangeBatch {
        after,
        through,
        delta,
    };
    Some(batch)
}

/// Read the scoped watermark and its complete interval in the same SQLite snapshot.
async fn read_draft_changes(
    store: Option<Arc<RuntimeStore>>,
    resources: &[Value],
    after: Option<i64>,
) -> Result<(i64, Option<DraftChangeBatch>), ApiError> {
    let resources = resources
        .iter()
        .cloned()
        .map(serde_json::from_value::<ResourceRef>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::internal())?;
    if !resources
        .iter()
        .any(|resource| matches!(resource, ResourceRef::DraftsResource(_)))
    {
        return Ok((after.unwrap_or(0), None));
    }
    store
        .ok_or_else(ApiError::internal)?
        .read_async(
            "runtime-http/routes/sync_stream/drafts",
            move |connection| {
                let through = SyncDocumentsRepository::draft_sequence(connection)
                    .map_err(|_| ApiError::internal())?;
                if !(0..=MAX_SAFE_REVISION).contains(&through) {
                    return Err(ApiError::internal());
                }
                Ok((through, draft_change_batch(connection, after, through)))
            },
        )
        .await
}

fn draft_change_batch(
    connection: &Connection,
    after: Option<i64>,
    through: i64,
) -> Option<DraftChangeBatch> {
    let after = after?;
    if after < 0 || after >= through {
        return None;
    }
    let rows = SyncDocumentsRepository::drafts_after(
        connection,
        after,
        SYNC_DRAFT_STREAM_MAX_DOCUMENTS + 1,
    )
    .ok()?;
    if rows.len() as i64 > SYNC_DRAFT_STREAM_MAX_DOCUMENTS {
        return None;
    }
    let mut documents = Vec::with_capacity(rows.len());
    for row in rows {
        documents.push(super::sync_documents::draft_document(row).ok()?);
    }
    if documents.last()?.seq != through {
        return None;
    }
    let batch = DraftChangeBatch {
        after,
        through,
        documents,
    };
    Some(batch)
}

fn attach_transcript_changes(
    event: &mut ResourceChangeEvent,
    mut updates: BTreeMap<String, TranscriptStreamUpdate>,
) {
    let resources = std::mem::take(&mut event.resources);
    let entries = std::mem::take(&mut event.resource_versions);
    for (resource, mut entry) in resources.into_iter().zip(entries) {
        if let ResourceRef::TranscriptResource(transcript) = &resource {
            match updates.remove(&transcript.agent_id) {
                Some(TranscriptStreamUpdate::Unchanged) => continue,
                Some(TranscriptStreamUpdate::Changed { revision, batch }) => {
                    entry.revision = revision;
                    entry.transcript_changes = batch.map(|batch| Some(*batch));
                }
                None => {}
            }
        }
        event.resources.push(resource);
        event.resource_versions.push(entry);
    }
}

/// Access metadata and live voice signaling remain in memory, outside persisted entities.
/// Missing or oversized intervals deliberately retain the resource's HTTP recovery hint.
async fn attach_transient_snapshots(
    state: Option<&HttpState>,
    event: &mut ResourceChangeEvent,
    voice_sequences: &mut BTreeMap<String, i64>,
) {
    let Some(state) = state else { return };
    for (resource, entry) in event.resources.iter().zip(&mut event.resource_versions) {
        match resource {
            ResourceRef::MultiServerResource(_) if cfg!(unix) => {
                entry.discovery_snapshot = super::multi_server::read_snapshot(state)
                    .await
                    .ok()
                    .and_then(|snapshot| serde_json::from_value(snapshot).ok())
                    .map(Some);
            }
            ResourceRef::VoiceResource(resource) => {
                let Some(store) = &state.store else { continue };
                let after = voice_sequences
                    .get(&resource.agent_id)
                    .copied()
                    .unwrap_or_default();
                if let Ok((cursor, payload)) =
                    studio_runtime_services::voice::stream_records(store, &resource.agent_id, after)
                        .await
                {
                    voice_sequences.insert(resource.agent_id.clone(), cursor);
                    entry.voice_records = payload
                        .and_then(|payload| serde_json::from_value(payload).ok())
                        .map(Some);
                }
            }
            _ => {}
        }
    }
}

struct ResourceStream {
    subscription: Option<LiveSubscription>,
    http_state: Option<HttpState>,
    vault_readiness: Option<studio_runtime_services::vault::VaultReadinessWatcher>,
    initial: VecDeque<Event>,
    workspace_id: String,
    epoch: String,
    last_revision: i64,
    resources: Vec<Value>,
    entity_sequence: i64,
    transcript_sequences: BTreeMap<String, i64>,
    voice_sequences: BTreeMap<String, i64>,
    draft_sequence: i64,
    store: Option<Arc<RuntimeStore>>,
    changes: tokio::sync::watch::Receiver<ChangeSnapshot>,
    notifications: Arc<ResourceNotifications>,
    token_rates: Option<tokio::sync::watch::Receiver<Arc<TokenRates>>>,
    /// Last snapshot sent by this stream; clearing it makes the next frame a full snapshot.
    last_token_rates: Option<Arc<TokenRates>>,
    consecutive_token_frames: usize,
    /// Earliest instant of the next token-rate frame, from `SYNC_FRAME_MIN_INTERVAL`.
    next_token_frame_at: Option<Instant>,
    /// Earliest instant of the next resources frame, from `SYNC_FRAME_MIN_INTERVAL`.
    next_resources_frame_at: Option<Instant>,
    heartbeat: Interval,
}

async fn next_subscription_change(
    subscription: Option<&mut LiveSubscription>,
) -> Option<SubscriptionChange> {
    match subscription {
        Some(subscription) => subscription.receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn subscription_frame(state: &mut ResourceStream, update: SubscriptionChange) -> Option<()> {
    state.resources = update.resources;
    let readiness_interest = state
        .resources
        .iter()
        .any(|resource| resource["kind"] == "vault-readiness");
    if readiness_interest != state.vault_readiness.is_some()
        && let Some(http) = &state.http_state
    {
        state.vault_readiness = readiness_source(http, &state.resources).await.ok()?;
    }
    state.voice_sequences.retain(|id, _| {
        state
            .resources
            .iter()
            .any(|resource| resource["kind"] == "voice" && resource["agentId"] == *id)
    });
    state.transcript_sequences.retain(|id, _| {
        state
            .resources
            .iter()
            .any(|resource| resource["kind"] == "transcript" && resource["agentId"] == *id)
    });
    state.initial.push_back(sse_event(
        "subscriptions",
        None,
        crate::generated::SyncSubscriptionAck {
            r#gen: update.generation,
        },
    ));
    if !update.added.is_empty() {
        let http_state = state.http_state.as_ref()?;
        let view = state
            .notifications
            .changes_since_with_content(&update.added, None, |resource| {
                resource_content(http_state.clone(), resource)
            })
            .await
            .ok()?;
        if view.cursor.closed {
            return None;
        }
        if update
            .added
            .iter()
            .any(|resource| resource["kind"] == "state")
        {
            state.entity_sequence = read_entity_sequence(state.store.clone(), &update.added)
                .await
                .ok()?;
        }
        if update
            .added
            .iter()
            .any(|resource| resource["kind"] == "drafts")
        {
            state.draft_sequence = read_draft_changes(state.store.clone(), &update.added, None)
                .await
                .ok()?
                .0;
        }
        let updates = read_transcript_changes(
            state.store.clone(),
            &view.resources,
            &mut state.transcript_sequences,
            false,
        )
        .await;
        let mut event = resource_changes_event(
            &state.workspace_id,
            &view,
            ResourceChangeEventReason::Initial,
            state.entity_sequence,
            None,
            state.draft_sequence,
            None,
        )
        .ok()?;
        attach_transcript_changes(&mut event, updates);
        attach_transient_snapshots(
            state.http_state.as_ref(),
            &mut event,
            &mut state.voice_sequences,
        )
        .await;
        state
            .initial
            .push_back(resource_sse_event(view.cursor.revision, event));
        append_progress_frames(http_state, &view, &mut state.initial).await;
    }
    if let Some(interest) = update.token_rates {
        if interest && state.token_rates.is_none() {
            state.token_rates = Some(state.http_state.as_ref()?.token_rates.subscribe());
            // A new interest starts from a full snapshot, not a delta of an older baseline.
            state.last_token_rates = None;
            let frame = token_rate_sse_frame(state)?;
            state.initial.push_back(frame);
        } else if !interest {
            state.token_rates = None;
            state.last_token_rates = None;
        }
    }
    Some(())
}

async fn next_frame(
    mut state: ResourceStream,
) -> Option<(Result<Event, Infallible>, ResourceStream)> {
    if let Some(event) = state.initial.pop_front() {
        return Some((Ok(event), state));
    }

    loop {
        // Match Python's token-first behavior when both lanes are pending,
        // while bounding token bursts so resource invalidations still progress.
        let prefer_resource = state.consecutive_token_frames >= 8;
        let heartbeat = if prefer_resource {
            tokio::select! {
                biased;
                update = next_subscription_change(state.subscription.as_mut()) => {
                    subscription_frame(&mut state, update?).await?;
                    return Some((Ok(state.initial.pop_front()?), state));
                },
                changed = changed_not_before(Some(&mut state.changes), state.next_resources_frame_at) => {
                    if changed.is_err() { return None; }
                    state.changes.borrow_and_update();
                    false
                },
                changed = changed_not_before(state.token_rates.as_mut(), state.next_token_frame_at) => {
                    if changed.is_err() { return None; }
                    state.consecutive_token_frames = state.consecutive_token_frames.saturating_add(1);
                    let frame = token_rate_sse_frame(&mut state)?;
                    state.next_token_frame_at = Some(Instant::now() + SYNC_FRAME_MIN_INTERVAL);
                    return Some((Ok(frame), state));
                },
                _ = state.heartbeat.tick() => true,
            }
        } else {
            tokio::select! {
                biased;
                update = next_subscription_change(state.subscription.as_mut()) => {
                    subscription_frame(&mut state, update?).await?;
                    return Some((Ok(state.initial.pop_front()?), state));
                },
                changed = changed_not_before(state.token_rates.as_mut(), state.next_token_frame_at) => {
                    if changed.is_err() { return None; }
                    state.consecutive_token_frames = state.consecutive_token_frames.saturating_add(1);
                    let frame = token_rate_sse_frame(&mut state)?;
                    state.next_token_frame_at = Some(Instant::now() + SYNC_FRAME_MIN_INTERVAL);
                    return Some((Ok(frame), state));
                },
                changed = changed_not_before(Some(&mut state.changes), state.next_resources_frame_at) => {
                    if changed.is_err() { return None; }
                    state.changes.borrow_and_update();
                    false
                },
                _ = state.heartbeat.tick() => true,
            }
        };
        let mut view = match state
            .notifications
            .changes_since_with_content(&state.resources, Some(state.last_revision), |resource| {
                let http_state = state.http_state.clone();
                async move { resource_content(http_state?, resource).await }
            })
            .await
        {
            Ok(view) if !view.cursor.closed => view,
            _ => return None,
        };
        state.last_revision = view.cursor.revision;
        if !view.resources.is_empty() {
            state.consecutive_token_frames = 0;
            let (sequence, batch) = match read_entity_changes(
                state.store.clone(),
                &view.resources,
                (!view.overflow && !view.entity_reset).then_some(state.entity_sequence),
            )
            .await
            {
                Ok(update) => update,
                Err(_) => {
                    view.entity_reset = true;
                    (state.entity_sequence, None)
                }
            };
            state.entity_sequence = sequence;
            let (draft_sequence, draft_changes) = read_draft_changes(
                state.store.clone(),
                &view.resources,
                (!view.overflow).then_some(state.draft_sequence),
            )
            .await
            .ok()?;
            state.draft_sequence = draft_sequence;
            let reason = if view.overflow {
                studio_runtime_services::diagnostics::record_sync_recovery(
                    studio_runtime_services::diagnostics::SyncRecoveryKind::Overflow,
                );
                ResourceChangeEventReason::Overflow
            } else {
                ResourceChangeEventReason::Change
            };
            let transcript_updates = read_transcript_changes(
                state.store.clone(),
                &view.resources,
                &mut state.transcript_sequences,
                !view.overflow,
            )
            .await;
            let mut event = resource_changes_event(
                &state.workspace_id,
                &view,
                reason,
                sequence,
                batch,
                draft_sequence,
                draft_changes,
            )
            .ok()?;
            attach_transcript_changes(&mut event, transcript_updates);
            attach_transient_snapshots(
                state.http_state.as_ref(),
                &mut event,
                &mut state.voice_sequences,
            )
            .await;
            if event.resources.is_empty() {
                continue;
            }
            state.next_resources_frame_at = Some(Instant::now() + SYNC_FRAME_MIN_INTERVAL);
            if let Some(http) = &state.http_state {
                append_progress_frames(http, &view, &mut state.initial).await;
            }
            return Some((Ok(resource_sse_event(view.cursor.revision, event)), state));
        }
        if heartbeat {
            let event = json!({
                "protocol": SYNC_PROTOCOL_VERSION,
                "workspaceId": state.workspace_id,
                "epoch": state.epoch,
                "revision": view.cursor.revision,
            });
            return Some((
                Ok(sse_event("heartbeat", Some(view.cursor.revision), event)),
                state,
            ));
        }
    }
}

/// A change notification that is not acted on before `not_before`: the first change after
/// a quiet period passes at once, and a burst collapses into one frame at the window's end
/// because the watch keeps only the newest value. The window is waited out before
/// `changed()`, which consumes the notification, so dropping this future loses nothing.
async fn changed_not_before<T>(
    receiver: Option<&mut tokio::sync::watch::Receiver<T>>,
    not_before: Option<Instant>,
) -> Result<(), tokio::sync::watch::error::RecvError> {
    let Some(receiver) = receiver else {
        return std::future::pending().await;
    };
    if let Some(not_before) = not_before {
        tokio::time::sleep_until(not_before).await;
    }
    receiver.changed().await
}

fn token_rate_sse_frame(state: &mut ResourceStream) -> Option<Event> {
    let (rates, cursor) = token_snapshot(state.token_rates.as_mut()?, || {
        state.changes.borrow().clone()
    });
    if cursor.closed {
        return None;
    }
    let event = match &state.last_token_rates {
        Some(previous) => {
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct DeltaFrame<'a> {
                protocol: u32,
                workspace_id: &'a str,
                epoch: &'a str,
                revision: i64,
                snapshot: bool,
                #[serde(flatten)]
                delta: studio_runtime_services::token_rate::TokenRateDelta<'a>,
            }
            sse_event(
                "token-rates",
                Some(cursor.revision),
                DeltaFrame {
                    protocol: SYNC_PROTOCOL_VERSION,
                    workspace_id: &state.workspace_id,
                    epoch: &cursor.epoch,
                    revision: cursor.revision,
                    snapshot: false,
                    delta: rates.delta_since(previous),
                },
            )
        }
        None => token_rate_event(&state.workspace_id, &cursor, &rates),
    };
    state.last_token_rates = Some(rates);
    Some(event)
}

// Keep the rate read guard until its cursor is captured: publishers take these locks in this order.
fn token_snapshot(
    rates: &mut tokio::sync::watch::Receiver<Arc<TokenRates>>,
    cursor: impl FnOnce() -> ChangeSnapshot,
) -> (Arc<TokenRates>, ChangeSnapshot) {
    let rates = rates.borrow_and_update();
    (Arc::clone(&rates), cursor())
}

fn resource_changes_event(
    workspace: &str,
    view: &ResourceChanges,
    reason: ResourceChangeEventReason,
    sequence: i64,
    mut changes: Option<EntityChangeBatch>,
    draft_sequence: i64,
    mut draft_changes: Option<DraftChangeBatch>,
) -> Result<ResourceChangeEvent, ApiError> {
    let resources = view
        .resources
        .iter()
        .cloned()
        .map(serde_json::from_value::<ResourceRef>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::internal())?;
    let resource_versions = resources
        .iter()
        .zip(&view.versions)
        .map(|(resource, revision)| {
            let state = matches!(resource, ResourceRef::StateResource(_));
            let drafts = matches!(resource, ResourceRef::DraftsResource(_));
            ResourceRevisionEntry {
                discovery_snapshot: None,
                voice_records: None,
                revision: if state {
                    sequence
                } else if drafts {
                    draft_sequence
                } else {
                    *revision
                },
                draft_changes: drafts.then(|| draft_changes.take()).flatten().map(Some),
                entity_changes: state.then(|| changes.take()).flatten().map(Some),
                entity_sequence_reset: state.then_some(Some(view.entity_reset)),
                entity_sequences: None,
                transcript_changes: None,
                resource: None,
            }
        })
        .collect();
    Ok(ResourceChangeEvent {
        protocol: i64::from(SYNC_PROTOCOL_VERSION),
        workspace_id: workspace.into(),
        epoch: view.cursor.epoch.clone(),
        revision: view.cursor.revision,
        reason,
        resources,
        resource_versions,
    })
}

/// The token-rates frame. The record list is the snapshot's shared,
/// once-serialized text; only the small cursor members are written per subscriber.
fn token_rate_event(workspace_id: &str, snapshot: &ChangeSnapshot, rates: &TokenRates) -> Event {
    let wire = rates.wire();
    let text = |value: &str| serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned());
    let data = format!(
        r#"{{"protocol":{},"workspaceId":{},"epoch":{},"revision":{},"snapshot":true,"removed":[],"records":{}}}"#,
        SYNC_PROTOCOL_VERSION,
        text(workspace_id),
        text(&snapshot.epoch),
        snapshot.revision,
        wire.records,
    );
    Event::default()
        .event("token-rates")
        .data(data)
        .id(snapshot.revision.to_string())
}

#[cfg(test)]
fn resource_event(
    workspace_id: &str,
    epoch: &str,
    revision: i64,
    reason: &str,
    resources: &[Value],
    entity_sequence: i64,
    entity_reset: bool,
) -> Value {
    serde_json::to_value(
        resource_changes_event(
            workspace_id,
            &ResourceChanges {
                cursor: ChangeSnapshot {
                    epoch: epoch.into(),
                    revision,
                    resource_revision: revision,
                    closed: false,
                },
                resources: resources.to_vec(),
                versions: vec![revision; resources.len()],
                entity_reset,
                overflow: false,
            },
            serde_json::from_value(json!(reason)).expect("fixture reason"),
            entity_sequence,
            None,
            0,
            None,
        )
        .expect("typed fixture event"),
    )
    .expect("event serializes")
}

fn sse_event(name: &str, revision: Option<i64>, payload: impl Serialize) -> Event {
    // These stream DTOs contain only JSON-compatible fields and string map keys.
    let event = Event::default()
        .event(name)
        .json_data(payload)
        .expect("stream DTO serializes");
    match revision {
        Some(revision) => event.id(revision.to_string()),
        None => event,
    }
}

/// Write JSON once into Axum's event buffer, counting the complete UTF-8 envelope.
/// Only an oversized interval is serialized again, after dropping its inline data.
fn resource_sse_event(revision: i64, mut payload: ResourceChangeEvent) -> Event {
    let id = revision.to_string();
    // Axum adds a space after each colon and a final blank line. The byte-boundary
    // witness compares this accounting with the actual HTTP body at T-1/T/T+1.
    let envelope_bytes = b"event: resources\nid: \ndata: \n\n".len() + id.len();
    let mut writer = Event::default()
        .event("resources")
        .id(&id)
        .into_data_writer();
    let mut bounded = BoundedEventWriter {
        writer: &mut writer,
        remaining: SYNC_STREAM_EVENT_MAX_BYTES - envelope_bytes,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut bounded, &payload);
    if !bounded.exceeded {
        // The generated DTO has only JSON-compatible fields and string map keys.
        result.expect("resource stream DTO serializes");
        return writer.into_event();
    }
    studio_runtime_services::diagnostics::record_sync_recovery(
        studio_runtime_services::diagnostics::SyncRecoveryKind::Oversize,
    );
    for entry in &mut payload.resource_versions {
        entry.entity_changes = None;
        entry.transcript_changes = None;
        entry.draft_changes = None;
        entry.discovery_snapshot = None;
        entry.voice_records = None;
    }
    sse_event("resources", Some(revision), payload)
}

struct BoundedEventWriter<'a> {
    writer: &'a mut EventDataWriter,
    remaining: usize,
    exceeded: bool,
}

impl Write for BoundedEventWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            self.exceeded = true;
            return Err(io::Error::other("resource event exceeds stream byte limit"));
        }
        // serde_json emits complete UTF-8 string fragments (and ASCII syntax).
        let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
        std::fmt::Write::write_str(self.writer, text).map_err(io::Error::other)?;
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Compression has its own CPU/window contract, independent of the HTTP JSON
/// threshold. One encoder retains its dictionary until the response is dropped.
const STREAM_BROTLI_QUALITY: u32 = 3;
/// Base-2 logarithm of the Brotli history window size in bytes.
const STREAM_BROTLI_LGWIN: u32 = 20;
/// Scratch buffer size, in bytes, used by the Brotli streaming writer.
const STREAM_BROTLI_BUFFER_BYTES: usize = 4096;

fn accepts_brotli(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .any(|header| {
            header.to_str().is_ok_and(|header| {
                header.split(',').any(|coding| {
                    let mut parts = coding.trim().split(';');
                    if !parts
                        .next()
                        .is_some_and(|name| name.trim().eq_ignore_ascii_case("br"))
                    {
                        return false;
                    }
                    parts.all(|parameter| {
                        let Some((key, value)) = parameter.trim().split_once('=') else {
                            return false;
                        };
                        key.trim().eq_ignore_ascii_case("q")
                            && value
                                .trim()
                                .parse::<f32>()
                                .is_ok_and(|q| q > 0.0 && q <= 1.0)
                    })
                })
            })
        })
}

fn encoded_event_response<S>(stream: S, headers: &HeaderMap) -> Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    let mut response = event_response(stream);
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if !accepts_brotli(headers) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    parts
        .headers
        .insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
    let encoder = brotli::CompressorWriter::new(
        Vec::new(),
        STREAM_BROTLI_BUFFER_BYTES,
        STREAM_BROTLI_QUALITY,
        STREAM_BROTLI_LGWIN,
    );
    let compressed = stream::try_unfold(
        (body.into_data_stream(), Some(encoder)),
        |(mut input, encoder)| async move {
            let Some(mut encoder) = encoder else {
                return Ok::<_, io::Error>(None);
            };
            let bytes = match input.next().await {
                Some(chunk) => {
                    encoder.write_all(&chunk.map_err(io::Error::other)?)?;
                    // Sse emits one body chunk per event, including heartbeats.
                    // FLUSH emits all pending bytes without ending the dictionary.
                    encoder.flush()?;
                    Bytes::from(std::mem::take(encoder.get_mut()))
                }
                None => return Ok(Some((Bytes::from(encoder.into_inner()), (input, None)))),
            };
            Ok(Some((bytes, (input, Some(encoder)))))
        },
    );
    Response::from_parts(parts, Body::from_stream(compressed))
}

fn event_response<S>(stream: S) -> Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    let mut response = Sse::new(stream).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_runtime_services::change_notifications::ChangeNotificationHub;

    #[tokio::test]
    async fn state_stream_progress_frames_are_transient_and_scoped() {
        use studio_runtime_services::claude_login::{
            LoginError,
            ports::*,
            receipts::FileReceipts,
            service::{LoginConfig, LoginService},
        };
        struct Accounts;
        impl LoginAccounts for Accounts {
            fn profile(&self, _: &str) -> Result<Value, LoginError> {
                Err(LoginError::Unavailable)
            }
            fn revalidate(&self, _: &str) -> Result<Value, LoginError> {
                Err(LoginError::Unavailable)
            }
            fn find_config(&self, _: &str) -> Result<Option<Value>, LoginError> {
                Ok(None)
            }
            fn register(
                &self,
                _: serde_json::Map<String, Value>,
                _: &str,
                _: &std::path::Path,
                _: &studio_runtime_services::account_driver::AuthMetadata,
            ) -> Result<Value, LoginError> {
                Err(LoginError::Unavailable)
            }
            fn refresh(
                &self,
                _: &str,
                _: &Value,
                _: &studio_runtime_services::account_driver::AuthMetadata,
            ) -> Result<Value, LoginError> {
                Err(LoginError::Unavailable)
            }
        }
        struct Chats;
        impl LoginChats for Chats {
            fn refresh<'a>(&'a self, _: &'a str) -> FutureResult<'a, ()> {
                Box::pin(async { Ok(()) })
            }
        }
        struct Child(tokio::sync::mpsc::Receiver<LoginEvent>);
        impl LoginChild for Child {
            fn next_event(&mut self) -> FutureResult<'_, Option<LoginEvent>> {
                Box::pin(async { Ok(self.0.recv().await) })
            }
            fn write(&self, _: Vec<u8>) -> FutureResult<'_, ()> {
                Box::pin(async { Ok(()) })
            }
            fn close(self: Box<Self>) -> FutureResult<'static, ()> {
                Box::pin(async { Ok(()) })
            }
        }
        struct Launcher(tokio::sync::Mutex<Option<tokio::sync::mpsc::Receiver<LoginEvent>>>);
        impl LoginLauncher for Launcher {
            fn spawn(&self, _: LoginCommand) -> FutureResult<'_, Box<dyn LoginChild>> {
                Box::pin(async {
                    Ok(Box::new(Child(
                        self.0.lock().await.take().ok_or(LoginError::Unavailable)?,
                    )) as Box<dyn LoginChild>)
                })
            }
        }
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let mut state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store.clone()),
            None,
        );
        let directory = root.path().join("logins");
        let receipt_directory = directory.clone();
        let receipts = studio_runtime_process::blocking_pool::spawn_blocking(
            "test/login-receipts",
            move || FileReceipts::new(receipt_directory),
        )
        .await
        .expect("blocking task")
        .expect("receipts");
        let (output, input) = tokio::sync::mpsc::channel(2);
        let changes = state.sync_changes.clone();
        let login = LoginService::open(
            LoginConfig {
                directory: directory.clone(),
                program: "/fixture/claude".into(),
                cwd: root.path().into(),
                environment: BTreeMap::from([("HOME".into(), root.path().as_os_str().to_owned())]),
            },
            Arc::new(Accounts),
            Arc::new(Launcher(tokio::sync::Mutex::new(Some(input)))),
            Arc::new(receipts),
            Arc::new(Chats),
            Arc::new(move || changes.notify_one()),
        )
        .await
        .expect("login service");
        state.validators = crate::validation::Registry::compile(&[]).expect("router validators");
        state.claude_login = Some(login.clone());
        let id = "e0166868-592a-4dca-8994-04d34a730af5";
        let url = "https://claude.com/cai/oauth/authorize?secret=transient-fixture";
        let mut changed = state.sync_changes.subscribe();
        let entity_sequence_before = store
            .read_async("test/before-progress", |connection| {
                SyncDocumentsRepository::entity_sequence(connection)
            })
            .await
            .expect("initial entity sequence");
        login
            .start(id.into(), None, None, None)
            .await
            .expect("start once");
        output
            .send(LoginEvent::Output(format!("{url}\n").into_bytes()))
            .await
            .expect("scripted URL");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if login.progress(id).await.expect("progress")["verificationUrl"] == url {
                    break;
                }
                changed.changed().await.expect("progress producer");
            }
        })
        .await
        .expect("URL notification");
        let resource = json!({"kind":"auth-progress","requestId":id});
        let view = ResourceChanges {
            cursor: state.sync_changes.snapshot(),
            resources: vec![resource.clone()],
            versions: vec![1],
            entity_reset: false,
            overflow: false,
        };
        let mut frames = VecDeque::new();
        append_progress_frames(&state, &view, &mut frames).await;
        let frame = frame_bytes(frames.pop_front().expect("current progress on subscribe")).await;
        let text = std::str::from_utf8(&frame).expect("UTF-8 frame");
        assert!(text.starts_with("event: resource-progress\n"));
        assert!(
            text.contains(url),
            "the subscribed client receives its transient URL"
        );
        assert_eq!(
            store
                .read_async("test/progress-is-transient", |connection| {
                    SyncDocumentsRepository::entity_sequence(connection)
                })
                .await
                .expect("sequence"),
            entity_sequence_before,
            "progress never writes a SQLite entity"
        );
        let receipt = tokio::fs::read_to_string(directory.join(format!("{id}.json")))
            .await
            .expect("durable receipt");
        assert!(!receipt.contains(url), "the OAuth URL never reaches disk");
        let unselected = ResourceChanges {
            resources: vec![json!({"kind":"state"})],
            ..view
        };
        append_progress_frames(&state, &unselected, &mut frames).await;
        assert!(
            frames.is_empty(),
            "only subscribed progress selectors receive data"
        );
        login.shutdown().await.expect("owned child closed");
    }

    #[test]
    fn state_stream_vault_prefix_contract_matches_its_runtime_owner() {
        let root = tempfile::tempdir().expect("isolated state");
        let mut state = HttpState::new(
            "".into(),
            "".into(),
            root.path().display().to_string(),
            0,
            None,
            None,
            None,
        );
        state.validators = crate::validation::Registry::compile(&[]).expect("router validators");
        for prefix in [
            "".to_owned(),
            "a".into(),
            "Project_1".into(),
            "../other".into(),
            "-bad".into(),
            "a".repeat(44),
            "a".repeat(45),
        ] {
            assert_eq!(
                state.validators.validate_component(
                    "ResourceRef",
                    &json!({"kind":"vault-readiness","prefix":prefix})
                ),
                studio_runtime_services::vault::valid_prefix(&prefix),
                "vault::valid_prefix owns the prefix contract: {prefix}"
            );
        }
    }

    #[test]
    fn state_stream_moves_owned_batch_allocations() {
        let changes: EntityChangeBatch = serde_json::from_value(json!({
            "after": 1, "through": 2,
            "documents": [{"op":"delete", "collection":"agent", "id":"agent", "seq":2}]
        }))
        .expect("entity batch");
        let drafts: DraftChangeBatch = serde_json::from_value(json!({
            "after": 1, "through": 2,
            "documents": [{"id":"draft", "payload":"{}", "seq":2, "_deleted":true}]
        }))
        .expect("draft batch");
        let entities_ptr = changes.documents.as_ptr();
        let drafts_ptr = drafts.documents.as_ptr();
        let view = ResourceChanges {
            cursor: ChangeSnapshot {
                epoch: "epoch".into(),
                revision: 2,
                resource_revision: 2,
                closed: false,
            },
            resources: vec![json!({"kind":"state"}), json!({"kind":"drafts"})],
            versions: vec![2, 2],
            entity_reset: false,
            overflow: false,
        };
        let event = resource_changes_event(
            "workspace",
            &view,
            ResourceChangeEventReason::Change,
            2,
            Some(changes),
            2,
            Some(drafts),
        )
        .expect("event");
        let entities = event.resource_versions[0]
            .entity_changes
            .as_ref()
            .and_then(Option::as_ref)
            .expect("entity batch");
        let drafts = event.resource_versions[1]
            .draft_changes
            .as_ref()
            .and_then(Option::as_ref)
            .expect("draft batch");
        assert_eq!(entities.documents.as_ptr(), entities_ptr);
        assert_eq!(drafts.documents.as_ptr(), drafts_ptr);
    }

    async fn frame_bytes(event: Event) -> axum::body::Bytes {
        axum::body::to_bytes(
            Sse::new(stream::iter([Ok::<_, Infallible>(event)]))
                .into_response()
                .into_body(),
            usize::MAX,
        )
        .await
        .expect("SSE bytes")
    }

    fn combined_transcript_event() -> ResourceChangeEvent {
        let mut value = resource_event(
            "workspace",
            "epoch",
            99999,
            "change",
            &[
                json!({"kind":"transcript","agentId":"a"}),
                json!({"kind":"transcript","agentId":"b"}),
            ],
            0,
            false,
        );
        for entry in value["resourceVersions"].as_array_mut().expect("versions") {
            entry["transcriptChanges"] = json!({"after":1,"through":99999,"delta":{
                "delta":true,"items":[{"id":"answer","role":"assistant","text":""}],
                "removed":[],"itemRevisions":{},"truncated":false
            }});
        }
        serde_json::from_value(value).expect("combined fixture")
    }

    #[tokio::test]
    async fn brotli_resource_event_limit_counts_utf8_envelope_and_combined_resources() {
        let limit = SYNC_STREAM_EVENT_MAX_BYTES;
        let empty = combined_transcript_event();
        let overhead = frame_bytes(sse_event("resources", Some(99999), &empty))
            .await
            .len();
        for target in [limit - 1, limit, limit + 1] {
            let mut value = serde_json::to_value(&empty).expect("fixture");
            let added = target - overhead;
            for (index, entry) in value["resourceVersions"]
                .as_array_mut()
                .expect("versions")
                .iter_mut()
                .enumerate()
            {
                let count = if index == 0 {
                    added / 2
                } else {
                    added - added / 2
                };
                entry["transcriptChanges"]["delta"]["items"][0]["text"] = json!(format!(
                    "{}{}",
                    "é".repeat(count / 2),
                    "x".repeat(count % 2)
                ));
            }
            let payload: ResourceChangeEvent = serde_json::from_value(value).expect("typed event");
            assert_eq!(
                frame_bytes(sse_event("resources", Some(99999), &payload))
                    .await
                    .len(),
                target
            );
            let bytes = frame_bytes(resource_sse_event(99999, payload)).await;
            assert!(
                bytes.len() <= limit,
                "complete SSE event is {} bytes; limit {limit}",
                bytes.len()
            );
            let wire = String::from_utf8(bytes.to_vec()).expect("UTF8");
            let body: Value = serde_json::from_str(
                wire.lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .expect("data"),
            )
            .expect("JSON");
            assert_eq!(body["revision"], 99999);
            assert_eq!(
                body["resourceVersions"].as_array().expect("versions").len(),
                2
            );
            for entry in body["resourceVersions"].as_array().expect("versions") {
                assert_eq!(entry["revision"], 99999);
                assert_eq!(entry.get("transcriptChanges").is_some(), target <= limit);
            }
        }
    }

    #[tokio::test]
    async fn brotli_resource_event_fallback_drops_all_inline_batches_and_preserves_revisions() {
        let mut event = combined_transcript_event();
        let entry = &mut event.resource_versions[0];
        entry.entity_changes = Some(Some(EntityChangeBatch {
            after: 1,
            through: 99999,
            documents: Vec::new(),
        }));
        entry.draft_changes = Some(Some(DraftChangeBatch {
            after: 1,
            through: 99999,
            documents: Vec::new(),
        }));
        entry.entity_sequence_reset = Some(Some(true));
        let mut value = serde_json::to_value(event).expect("event");
        value["resourceVersions"][0]["transcriptChanges"]["delta"]["items"][0]["text"] =
            json!("x".repeat(SYNC_STREAM_EVENT_MAX_BYTES));
        let event = serde_json::from_value(value).expect("event");
        let document = event_document(Ok(resource_sse_event(99999, event))).await;
        let entry = &document["resourceVersions"][0];
        assert_eq!(entry["revision"], 99999);
        assert_eq!(entry["entitySequenceReset"], true);
        for field in ["entityChanges", "transcriptChanges", "draftChanges"] {
            assert!(entry.get(field).is_none(), "{field} must use pull recovery");
        }
    }

    #[tokio::test]
    async fn brotli_stream_route_negotiates_encoding_and_flushes_the_initial_event() {
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use tower::ServiceExt;
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store),
            None,
        );
        let app = crate::router(state).expect("router");
        for (accept, compressed) in [
            ("br", true),
            ("gzip, BR;q=0.5", true),
            ("gzip", false),
            ("br;q=0", false),
        ] {
            let response = app.clone().oneshot(Request::builder()
                .uri(format!("/api/sync/stream?protocol={SYNC_PROTOCOL_VERSION_TEXT}&resources=%5B%7B%22kind%22%3A%22drafts%22%7D%5D&tokenRates=false"))
                .header("host", "127.0.0.1:4321").header("x-canvas-token", "token")
                .header(header::ACCEPT_ENCODING, accept)
                .extension(ConnectInfo(std::net::SocketAddr::from(([127,0,0,1],12345))))
                .body(Body::empty()).expect("request")).await.expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().contains_key(header::CONTENT_ENCODING),
                compressed,
                "{accept}"
            );
            assert!(
                response
                    .headers()
                    .get_all(header::VARY)
                    .iter()
                    .any(|value| value
                        .to_str()
                        .is_ok_and(|value| value.contains("Accept-Encoding")))
            );
            let mut body = response.into_body().into_data_stream();
            let chunk = tokio::time::timeout(Duration::from_secs(2), body.next())
                .await
                .expect("initial flush")
                .expect("chunk")
                .expect("body");
            let bytes = if compressed {
                let mut decoder = brotli::DecompressorWriter::new(Vec::new(), 4096);
                decoder
                    .write_all(&chunk)
                    .expect("decode initial event without EOF");
                decoder.get_ref().clone()
            } else {
                chunk.to_vec()
            };
            let text = std::str::from_utf8(&bytes).expect("UTF8");
            assert!(text.starts_with("event: session\n"), "{text}");
            assert!(text.ends_with("\n\n"));
        }
    }

    #[tokio::test]
    async fn brotli_stream_flushes_each_event_and_heartbeat_and_cancels_on_drop() {
        use futures_util::StreamExt;
        use std::io::Write;
        let (send, recv) = tokio::sync::mpsc::channel::<Event>(1);
        let source = stream::unfold(recv, |mut recv| async move {
            recv.recv().await.map(|event| (Ok(event), recv))
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, br"),
        );
        let response = encoded_event_response(source, &headers);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("br")
        );
        let mut body = response.into_body().into_data_stream();
        let mut decoder = brotli::DecompressorWriter::new(Vec::new(), 4096);
        for (name, revision) in [("resources", 1), ("heartbeat", 2), ("resources", 3)] {
            let event = sse_event(name, Some(revision), json!({"revision":revision}));
            let expected = frame_bytes(event.clone()).await;
            send.send(event).await.expect("produce only this event");
            let start = decoder.get_ref().len();
            tokio::time::timeout(Duration::from_secs(2), async {
                while decoder.get_ref().len() - start < expected.len() {
                    let chunk = body.next().await.expect("compressed chunk").expect("body");
                    decoder.write_all(&chunk).expect("incremental decode");
                }
            })
            .await
            .expect("event must arrive before next event is produced");
            assert_eq!(&decoder.get_ref()[start..], expected.as_ref());
        }
        drop(body);
        tokio::time::timeout(Duration::from_secs(2), send.closed())
            .await
            .expect("disconnect drops upstream");
    }

    #[tokio::test]
    async fn brotli_stream_stays_plain_without_negotiation() {
        for accept in [
            None,
            Some("gzip"),
            Some("br;q=0"),
            Some("br;q=0, gzip"),
            Some("xbr"),
            Some("br;q=invalid"),
        ] {
            let mut headers = HeaderMap::new();
            if let Some(accept) = accept {
                headers.insert(
                    header::ACCEPT_ENCODING,
                    HeaderValue::from_str(accept).expect("header"),
                );
            }
            let event = sse_event("heartbeat", Some(1), json!({"revision":1}));
            let expected = frame_bytes(event.clone()).await;
            let response = encoded_event_response(stream::iter([Ok(event)]), &headers);
            assert!(
                response.headers().get(header::CONTENT_ENCODING).is_none(),
                "{accept:?}"
            );
            let actual = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("plain body");
            assert_eq!(actual, expected);
        }
    }

    #[tokio::test]
    async fn state_stream_live_subscriptions_ack_before_initial_and_expire_on_drop() {
        use axum::body::{Body, to_bytes};
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use futures_util::StreamExt;
        use tower::ServiceExt;

        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store),
            None,
        );
        let app = crate::router(state).expect("router");
        let request = |method: &str, uri: &str, body: Body| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header("host", "127.0.0.1:4321")
                .header("x-canvas-token", "token")
                .header("content-type", "application/json")
                .extension(ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    12345,
                ))))
                .body(body)
                .expect("request")
        };
        let token_only = app.clone().oneshot(request("GET", &format!("/api/sync/stream?protocol={SYNC_PROTOCOL_VERSION_TEXT}&resources=%5B%5D&tokenRates=true"), Body::empty())).await.expect("token-only recovery");
        assert_eq!(
            token_only.status(),
            StatusCode::OK,
            "a full reconnect can retain token interest after removing the last resource"
        );
        let mut token_only = token_only.into_body().into_data_stream();
        token_only.next().await.expect("session").expect("bytes");
        token_only
            .next()
            .await
            .expect("empty baseline")
            .expect("bytes");
        let token = token_only
            .next()
            .await
            .expect("token baseline")
            .expect("bytes");
        assert!(String::from_utf8_lossy(&token).contains("event: token-rates"));
        drop(token_only);
        let response = app.clone().oneshot(request("GET", &format!("/api/sync/stream?protocol={SYNC_PROTOCOL_VERSION_TEXT}&resources=%5B%7B%22kind%22%3A%22drafts%22%7D%5D&tokenRates=false"), Body::empty())).await.expect("stream");
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let frame = stream.next().await.expect("session").expect("bytes");
        let frame = String::from_utf8_lossy(&frame);
        assert!(frame.starts_with("event: session\n"), "{frame}");
        let session: Value = serde_json::from_str(
            frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("session JSON"),
        )
        .expect("session DTO");
        assert_eq!(session["sessionId"].as_str().expect("id").len(), 36);
        let initial = stream.next().await.expect("initial").expect("bytes");
        assert!(String::from_utf8_lossy(&initial).contains("event: resources"));
        let update = json!({"sessionId":session["sessionId"],"gen":1,"add":[{"kind":"queue","agentId":"chat"}],"remove":[{"kind":"drafts"}]});
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/sync/subscriptions",
                Body::from(update.to_string()),
            ))
            .await
            .expect("post");
        assert_eq!(response.status(), StatusCode::OK);
        let replay = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/sync/subscriptions",
                Body::from(update.to_string()),
            ))
            .await
            .expect("replay");
        assert_eq!(replay.status(), StatusCode::CONFLICT);
        let mut invalid = update.clone();
        invalid["gen"] = json!(2);
        invalid["add"] = json!([{"kind":"transcript","agentId":""}]);
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/sync/subscriptions",
                Body::from(invalid.to_string()),
            ))
            .await
            .expect("invalid selector");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let mut oversized = update.clone();
        oversized["gen"] = json!(2);
        oversized["add"] = json!(
            (0..=MAX_SYNC_RESOURCE_COUNT)
                .map(|index| json!({"kind":"queue","agentId":index.to_string()}))
                .collect::<Vec<_>>()
        );
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/sync/subscriptions",
                Body::from(oversized.to_string()),
            ))
            .await
            .expect("oversized selector");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let ack = stream.next().await.expect("ack").expect("bytes");
        assert!(String::from_utf8_lossy(&ack).contains("event: subscriptions"));
        let frame = stream.next().await.expect("added initial").expect("bytes");
        let frame = String::from_utf8_lossy(&frame);
        assert!(
            frame.contains("event: resources")
                && frame.contains("\"reason\":\"initial\"")
                && frame.contains("chat")
                && !frame.contains("drafts"),
            "{frame}"
        );
        let bad_auth = Request::builder()
            .method("POST")
            .uri("/api/sync/subscriptions")
            .header("host", "127.0.0.1:4321")
            .header("content-type", "application/json")
            .body(Body::from(update.to_string()))
            .expect("unauthenticated");
        assert_eq!(
            app.clone().oneshot(bad_auth).await.expect("auth").status(),
            StatusCode::FORBIDDEN
        );
        drop(stream);
        let response = app
            .oneshot(request(
                "POST",
                "/api/sync/subscriptions",
                Body::from(update.to_string()),
            ))
            .await
            .expect("expired");
        assert_eq!(response.status(), StatusCode::GONE);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error");
        assert!(String::from_utf8_lossy(&body).contains("session"));
    }

    #[tokio::test(start_paused = true)]
    async fn state_stream_token_interest_suppresses_tokens_but_preserves_resources_and_heartbeats()
    {
        use axum::body::Body;
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use futures_util::StreamExt;
        use tower::ServiceExt;

        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store.clone()),
            None,
        );
        let app = crate::router(state.clone()).expect("router");
        let request = |selector: &str| {
            Request::builder()
            .uri(format!("/api/sync/stream?protocol={SYNC_PROTOCOL_VERSION_TEXT}&resources=%5B%7B%22kind%22%3A%22drafts%22%7D%5D{selector}"))
            .header("host", "127.0.0.1:4321")
            .header("x-canvas-token", "token")
            .extension(ConnectInfo(std::net::SocketAddr::from(([127,0,0,1],12345))))
            .body(Body::empty()).expect("request")
        };
        let response = app
            .clone()
            .oneshot(request("&tokenRates=false"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        body.next().await.expect("session").expect("bytes");
        let first = body.next().await.expect("initial").expect("bytes");
        assert!(String::from_utf8_lossy(&first).contains("event: resources"));
        state
            .token_rates
            .publish(test_rates(1))
            .expect("token publication");
        let frame = body.next().await.expect("heartbeat").expect("bytes");
        assert!(
            String::from_utf8_lossy(&frame).contains("event: heartbeat"),
            "uninterested streams must omit initial and subsequent tokens: {}",
            String::from_utf8_lossy(&frame)
        );
        store
            .transaction(|tx| {
                SyncDocumentsRepository::put_draft(
                    tx,
                    "phone:chat",
                    r#"{"device":"phone","session":"chat","text":"draft","updated":1}"#,
                    false,
                )?;
                Ok(())
            })
            .await
            .expect("draft change");
        state
            .resource_notifications
            .publish_once(&json!({"requestId":"interest-draft","resources":[{"kind":"drafts"}]}))
            .expect("draft hint");
        let frame = body.next().await.expect("resource change").expect("bytes");
        assert!(String::from_utf8_lossy(&frame).contains("event: resources"));
        drop(body);
        // Read only the selector default from the OpenAPI owner, through typed serde.
        #[derive(Deserialize)]
        struct Api {
            paths: Paths,
        }
        #[derive(Deserialize)]
        struct Paths {
            #[serde(rename = "/api/sync/stream")]
            stream: Stream,
        }
        #[derive(Deserialize)]
        struct Stream {
            get: Operation,
        }
        #[derive(Deserialize)]
        struct Operation {
            parameters: Vec<Parameter>,
        }
        #[derive(Deserialize)]
        struct Parameter {
            name: String,
            schema: ParameterSchema,
        }
        #[derive(Deserialize)]
        struct ParameterSchema {
            default: Option<bool>,
        }
        let api: Api =
            serde_json::from_str(include_str!("../../openapi.json")).expect("OpenAPI owner");
        let selector = api
            .paths
            .stream
            .get
            .parameters
            .iter()
            .find(|parameter| parameter.name == "tokenRates")
            .expect("selector owner");
        assert_eq!(
            selector.schema.default,
            Some(true),
            "omission below must match the schema owner"
        );
        for selector in ["", "&tokenRates=true"] {
            let response = app
                .clone()
                .oneshot(request(selector))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            let mut body = response.into_body().into_data_stream();
            body.next().await.expect("session").expect("bytes");
            body.next().await.expect("initial").expect("bytes");
            let frame = body.next().await.expect("current snapshot").expect("bytes");
            let frame = String::from_utf8_lossy(&frame);
            assert!(frame.contains("event: token-rates"));
            assert!(
                frame.contains("\"rate\":1"),
                "reconfiguration immediately gets the current snapshot: {frame}"
            );
        }
        for selector in [
            "&tokenRates=invalid",
            "&tokenRates=1",
            "&tokenRates=",
            "&tokenRates=true&tokenRates=false",
        ] {
            let response = app
                .clone()
                .oneshot(request(selector))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{selector}");
        }
    }

    #[tokio::test]
    async fn state_stream_subscription_post_token_interest_reenable_starts_full() {
        fn document_bytes(bytes: &[u8]) -> Value {
            let text = std::str::from_utf8(bytes).expect("UTF8");
            serde_json::from_str(
                text.lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .expect("data"),
            )
            .expect("JSON")
        }
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use tower::ServiceExt;
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let http = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store),
            None,
        );
        let quiet = json!({"agentId":"quiet","rootId":"root","turnId":"turn","active":false,"estimated":false,"rate":0.0,"outputTokens":50.0});
        let mut rates = floating_test_rates(1);
        rates["records"]
            .as_array_mut()
            .expect("records")
            .push(quiet.clone());
        http.token_rates
            .publish(rates.clone())
            .expect("initial rates");
        let app = crate::router(http.clone()).expect("router");
        let request = |method: &str, uri: &str, body: Body| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header("host", "127.0.0.1:4321")
                .header("x-canvas-token", "token")
                .header("content-type", "application/json")
                .extension(ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    12345,
                ))))
                .body(body)
                .expect("request")
        };
        let response = app.clone().oneshot(request("GET", &format!("/api/sync/stream?protocol={SYNC_PROTOCOL_VERSION_TEXT}&resources=%5B%5D&tokenRates=true"), Body::empty())).await.expect("stream");
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let session = document_bytes(&body.next().await.expect("session").expect("bytes"));
        body.next().await.expect("resources").expect("bytes");
        let initial = document_bytes(&body.next().await.expect("tokens").expect("bytes"));
        assert_eq!(initial["snapshot"], true);
        assert_eq!(initial["records"], rates["records"]);
        for (generation, interest) in [(1, false), (2, true)] {
            let update = json!({"sessionId":session["sessionId"],"gen":generation,"add":[],"remove":[],"tokenRates":interest});
            let response = app
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/sync/subscriptions",
                    Body::from(update.to_string()),
                ))
                .await
                .expect("subscription POST");
            assert_eq!(response.status(), StatusCode::OK);
            let ack = body.next().await.expect("ack").expect("bytes");
            assert!(String::from_utf8_lossy(&ack).starts_with("event: subscriptions\n"));
            assert_eq!(document_bytes(&ack)["gen"], generation);
            if !interest {
                rates = floating_test_rates(2);
                rates["records"]
                    .as_array_mut()
                    .expect("records")
                    .push(quiet.clone());
                http.token_rates
                    .publish(rates.clone())
                    .expect("rates change while disabled");
            }
        }
        let restored = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .expect("reenabled token frame")
            .expect("tokens")
            .expect("bytes");
        let restored = document_bytes(&restored);
        assert_eq!(restored["snapshot"], true);
        assert_eq!(restored["removed"], json!([]));
        assert_eq!(
            restored["records"], rates["records"],
            "full replay includes quiet agents"
        );
    }

    #[tokio::test]
    async fn state_stream_subscription_added_oversized_initial_data_stays_revisions_only() {
        fn document_bytes(bytes: &[u8]) -> Value {
            let text = std::str::from_utf8(bytes).expect("UTF8");
            serde_json::from_str(
                text.lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .expect("data"),
            )
            .expect("JSON")
        }
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use tower::ServiceExt;
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('large',?1)", [json!({"id":"large","rootId":"large","epoch":0,"status":"running","threadId":"large","accountKey":"default","turnId":"turn","inFlight":true,"activeTools":[],"events":0}).to_string()])?;
            for (id, text) in [("large:large", "x".repeat(SYNC_STREAM_EVENT_MAX_BYTES + 1)), ("large:small", "before".to_owned())] {
                tx.execute("INSERT INTO runtime_items(id,agent,record,created) VALUES(?1,'large',?2,1)", rusqlite::params![id, json!({"id":id,"role":"assistant","text":text}).to_string()])?;
            }
            Ok(())
        }).await.expect("large conversation");
        let http = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store.clone()),
            None,
        );
        let app = crate::router(http).expect("router");
        let request = |method: &str, uri: &str, body: Body| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header("host", "127.0.0.1:4321")
                .header("x-canvas-token", "token")
                .header("content-type", "application/json")
                .extension(ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    12345,
                ))))
                .body(body)
                .expect("request")
        };
        let response = app.clone().oneshot(request("GET", &format!("/api/sync/stream?protocol={SYNC_PROTOCOL_VERSION_TEXT}&resources=%5B%7B%22kind%22%3A%22transcript%22%2C%22agentId%22%3A%22large%22%7D%5D&tokenRates=false"), Body::empty())).await.expect("stream");
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let session = document_bytes(&body.next().await.expect("session").expect("bytes"));
        body.next().await.expect("initial").expect("bytes");
        let full = studio_runtime_services::transcript::pull_scope(
            store.clone(),
            "transcript:large",
            "large",
            0,
        )
        .await
        .expect("source data");
        assert!(
            serde_json::to_vec(&full).expect("source JSON").len() > SYNC_STREAM_EVENT_MAX_BYTES
        );
        let before = full["checkpoint"]["seq"].as_i64().expect("baseline cursor");
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_items SET record=json_set(record,'$.text','after') WHERE id='large:small'", [])?;
            Ok(())
        }).await.expect("small change inside large transcript");
        // Removing and adding in one POST requires a new baseline even when the
        // final resource set retains the selector and its previous stream cursor.
        let resource = json!({"kind":"transcript","agentId":"large"});
        let update =
            json!({"sessionId":session["sessionId"],"gen":1,"add":[resource],"remove":[resource]});
        let response = app
            .oneshot(request(
                "POST",
                "/api/sync/subscriptions",
                Body::from(update.to_string()),
            ))
            .await
            .expect("subscription POST");
        assert_eq!(response.status(), StatusCode::OK);
        let ack = body.next().await.expect("ack").expect("bytes");
        assert!(String::from_utf8_lossy(&ack).starts_with("event: subscriptions\n"));
        let frame = body
            .next()
            .await
            .expect("added resource initial")
            .expect("bytes");
        assert!(frame.len() <= SYNC_STREAM_EVENT_MAX_BYTES);
        let event = document_bytes(&frame);
        assert_eq!(event["reason"], "initial");
        assert_eq!(event["resources"], json!([resource]));
        let entry = &event["resourceVersions"][0];
        assert!(entry["revision"].as_i64().expect("revision") > before);
        for field in ["transcriptChanges", "entityChanges", "draftChanges"] {
            assert!(
                entry.get(field).is_none(),
                "initial data must use pull recovery: {field}"
            );
        }
    }

    #[tokio::test]
    async fn state_stream_transcript_delta_uses_the_existing_pull_cursor() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('open',?1)",
                [json!({"id":"open","rootId":"open","epoch":0,"status":"running","threadId":"thread","accountKey":"default","turnId":"turn","inFlight":true,"activeTools":[],"events":0}).to_string()])?;
            tx.execute("INSERT INTO runtime_items(id,agent,record,created) VALUES('open:answer','open',?1,1)",
                [json!({"id":"open:answer","role":"assistant","text":"before"}).to_string()])?;
            Ok(())
        }).await.expect("conversation");
        let http = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store.clone()),
            None,
        );
        let resources = vec![json!({"kind":"transcript","agentId":"open"})];
        let mut state = test_stream(
            &http.sync_changes,
            &http.token_rates,
            http.resource_notifications.clone(),
            resources,
        );
        state.http_state = Some(http.clone());
        state.store = Some(store.clone());
        http.sync_changes.notify_one();
        let (_, mut state) = next_frame(state).await.expect("baseline hint");
        let baseline = studio_runtime_services::transcript::pull_scope(
            store.clone(),
            "transcript:open",
            "open",
            0,
        )
        .await
        .expect("client baseline");
        let after = baseline["checkpoint"]["seq"].as_i64().expect("cursor");
        use studio_runtime_services::notification_dispatch::{
            NotificationStreamBuffer, ProviderDeliveryIdentity, PublicationContext,
            dispatch_notification_with_effects, flush_notification_streams,
        };
        let streams = store.transaction_with(|tx| {
            let mut decoder = studio_runtime_providers::LineDecoder::new();
            let notification = decoder.push(b"{\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"thread\",\"turnId\":\"turn\",\"itemId\":\"answer\",\"delta\":\" streamed text\"}}\n")
                .expect("real provider frame").remove(0);
            let result = dispatch_notification_with_effects(tx, "open", &PublicationContext::default(),
                &NotificationStreamBuffer::default(), &ProviderDeliveryIdentity { provider_handle: "account:default".into(), generation: 1, cursor: 1 },
                &notification, 2.0, &studio_runtime_services::notification_runtime_effects::RuntimeNotificationEffects)?;
            Ok::<_, studio_runtime_services::notification_dispatch::NotificationDispatchError>(result.stream_update.expect("buffered delta"))
        }).await.expect("provider delta commits");
        store
            .transaction_with(move |tx| {
                flush_notification_streams(
                    tx,
                    "open",
                    &PublicationContext::default(),
                    &streams,
                    3.0,
                )?;
                Ok::<_, studio_runtime_services::notification_dispatch::NotificationDispatchError>(
                    (),
                )
            })
            .await
            .expect("provider flush commits runtime_items and publishes the store change");
        // Throttle spacing is covered by the paused stream scheduling tests.
        state.next_resources_frame_at = None;
        let (event, mut state) = next_frame(state).await.expect("transcript frame");
        let event = event_document(event).await;
        let batch = &event["resourceVersions"][0]["transcriptChanges"];
        assert_eq!(
            batch["after"], after,
            "stream carries the entire pull interval"
        );
        assert_eq!(batch["delta"]["items"][0]["text"], "before streamed text");
        assert_eq!(batch["through"], event["resourceVersions"][0]["revision"]);
        state.next_resources_frame_at = None;
        http.resource_notifications.publish_once(&json!({"requestId":"preflush-empty","resources":[{"kind":"transcript","agentId":"open"}]})).expect("scoped preflush hint");
        let mut pending = Box::pin(next_frame(state));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), pending.as_mut())
                .await
                .is_err(),
            "a proven unchanged transcript must emit no pull hint"
        );
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_items SET record=json_set(record,'$.text','flushed after hint') WHERE id='open:answer'", [])?;
            Ok(())
        }).await.expect("later stored flush");
        http.sync_changes.notify_one();
        let (event, _) = pending.await.expect("later changed transcript");
        assert_eq!(
            event_document(event).await["resourceVersions"][0]["transcriptChanges"]["delta"]["items"]
                [0]["text"],
            "flushed after hint"
        );
    }

    #[tokio::test]
    async fn state_stream_transcript_accumulation_drops_inline_data_at_shared_budget() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        store.transaction(|tx| {
            for agent in ["first", "second", "last"] {
                tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)", rusqlite::params![agent, json!({"id":agent,"rootId":agent,"epoch":0,"status":"running","threadId":agent,"accountKey":"default","turnId":"turn","inFlight":true,"activeTools":[],"events":0}).to_string()])?;
                tx.execute("INSERT INTO runtime_items(id,agent,record,created) VALUES(?1,?2,?3,1)", rusqlite::params![format!("{agent}:answer"), agent, json!({"id":format!("{agent}:answer"),"role":"assistant","text":"before"}).to_string()])?;
            }
            Ok(())
        }).await.expect("conversations");
        let resources: Vec<_> = ["first", "second", "last"]
            .into_iter()
            .map(|agent| json!({"kind":"transcript","agentId":agent}))
            .collect();
        let mut sequences = BTreeMap::new();
        read_transcript_changes(Some(store.clone()), &resources, &mut sequences, false).await;
        let before = sequences.clone();
        store.transaction(|tx| {
            for agent in ["first", "second", "last"] {
                let text = if agent == "last" { "small".to_owned() } else { "x".repeat(SYNC_STREAM_EVENT_MAX_BYTES / 2 + 1) };
                tx.execute("UPDATE runtime_items SET record=json_set(record,'$.text',?1) WHERE agent=?2", rusqlite::params![text, agent])?;
            }
            Ok(())
        }).await.expect("changed transcripts");
        // Prove the first candidate fits by itself; crossing the aggregate budget
        // must discard it and prevent the later small candidate from being retained.
        let mut single = before.clone();
        let first =
            read_transcript_changes(Some(store.clone()), &resources[..1], &mut single, true).await;
        assert!(matches!(
            first["first"],
            TranscriptStreamUpdate::Changed { batch: Some(_), .. }
        ));
        let updates = read_transcript_changes(Some(store), &resources, &mut sequences, true).await;
        assert_eq!(updates.len(), 3);
        for agent in ["first", "second", "last"] {
            assert!(sequences[agent] > before[agent]);
            assert!(
                matches!(updates[agent], TranscriptStreamUpdate::Changed { revision, batch: None } if revision == sequences[agent]),
                "{agent}: only revisions may remain after the budget is crossed"
            );
        }
    }

    #[tokio::test]
    async fn state_stream_transcript_subscribers_share_one_projection_build_per_revision() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('open',?1)",
                [json!({"id":"open","rootId":"open","epoch":0,"status":"idle"}).to_string()])?;
            tx.execute("INSERT INTO runtime_items(id,agent,record,created) VALUES('answer','open',?1,1)",
                [json!({"id":"answer","role":"assistant","text":"before"}).to_string()])?;
            tx.execute_batch("CREATE TABLE transcript_build_witness(count INTEGER NOT NULL);
                INSERT INTO transcript_build_witness VALUES(0);
                CREATE TRIGGER count_transcript_builds AFTER UPDATE OF payload ON sync_entities
                WHEN NEW.collection='transcript:open' AND NEW.id='@revision' AND NEW.payload IS NOT NULL
                BEGIN UPDATE transcript_build_witness SET count=count+1; END;")?;
            Ok(())
        }).await.expect("conversation and rebuild counter");
        let resources = vec![json!({"kind":"transcript","agentId":"open"})];
        let mut first = BTreeMap::new();
        let mut second = BTreeMap::new();
        let (left, right) = tokio::join!(
            read_transcript_changes(Some(store.clone()), &resources, &mut first, false),
            read_transcript_changes(Some(store.clone()), &resources, &mut second, false),
        );
        assert_eq!(first, second);
        assert!(matches!(
            left["open"],
            TranscriptStreamUpdate::Changed { batch: None, .. }
        ));
        assert!(matches!(
            right["open"],
            TranscriptStreamUpdate::Changed { batch: None, .. }
        ));
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_items SET record=json_set(record,'$.text','after') WHERE id='answer'", [])?;
            Ok(())
        }).await.expect("streamed update");
        let (left, right) = tokio::join!(
            read_transcript_changes(Some(store.clone()), &resources, &mut first, true),
            read_transcript_changes(Some(store.clone()), &resources, &mut second, true),
        );
        let TranscriptStreamUpdate::Changed {
            batch: Some(left), ..
        } = &left["open"]
        else {
            panic!("complete left delta");
        };
        let TranscriptStreamUpdate::Changed {
            batch: Some(right), ..
        } = &right["open"]
        else {
            panic!("complete right delta");
        };
        assert_eq!(
            serde_json::to_value(left).expect("left"),
            serde_json::to_value(right).expect("right")
        );
        let unrelated = read_transcript_changes(
            Some(store.clone()),
            &[json!({"kind":"accounts"})],
            &mut first,
            true,
        )
        .await;
        assert!(
            unrelated.is_empty(),
            "idle transcripts are never read for unrelated hints"
        );
        let builds = store
            .read_async(
                "transcript-build-count",
                |tx| -> Result<i64, studio_runtime_store::StoreError> {
                    Ok(
                        tx.query_row("SELECT count FROM transcript_build_witness", [], |row| {
                            row.get(0)
                        })?,
                    )
                },
            )
            .await
            .expect("projection build count");
        assert_eq!(
            builds, 2,
            "one page computation for baseline and one for the changed revision, shared by both subscribers"
        );
    }

    #[test]
    fn state_stream_transcript_controls_use_bounded_schema_owners() {
        let validator = studio_runtime_schema::compiled_validator("TranscriptStreamDelta")
            .expect("stream delta validator");
        let delta = json!({"delta":true,"items":[{"id":"answer","role":"assistant","text":"text"}],
            "order":["answer"],"removed":[],"itemRevisions":{"answer":"0".repeat(64)},"truncated":false,
            "nextCursor":"answer","nextAfterCursor":"answer","historyVersion":"thread:checkpoint","tail":"","unavailable":null});
        assert!(validator.is_valid(&delta));
        for scenario in [
            "unavailable",
            "order_count",
            "removed_id",
            "order_id",
            "revision_key",
            "revision_value",
            "cursor_empty",
            "cursor_long",
            "version_long",
            "tail_long",
        ] {
            let mut payload = delta.clone();
            match scenario {
                "unavailable" => payload["unavailable"] = json!("arbitrary-reason"),
                "order_count" => {
                    payload["order"] = json!(vec!["answer"; SYNC_TRANSCRIPT_STREAM_MAX_ITEMS + 1])
                }
                "removed_id" => payload["removed"] = json!([""]),
                "order_id" => payload["order"] = json!([""]),
                "revision_key" => payload["itemRevisions"] = json!({"":"0".repeat(64)}),
                "revision_value" => {
                    payload["itemRevisions"]["answer"] = json!("not-a-sha256-digest")
                }
                "cursor_empty" => payload["nextCursor"] = json!(""),
                "cursor_long" => payload["nextAfterCursor"] = json!("x".repeat(4097)),
                "version_long" => payload["historyVersion"] = json!("x".repeat(8194)),
                "tail_long" => payload["tail"] = json!("x".repeat(301)),
                _ => unreachable!(),
            }
            assert!(
                !validator.is_valid(&payload),
                "{scenario} must be rejected by the stream schema"
            );
            let response: TranscriptSyncPage = serde_json::from_value(json!({"workspaceId":"workspace", "checkpoint":{"seq":2}, "documents":[{"id":"transcript:open","seq":2,"payload":payload.to_string(),"_deleted":false}]})).expect("projection response");
            assert!(
                transcript_change_batch(response, "transcript:open", 1).is_none(),
                "{scenario} must preserve pull recovery"
            );
        }
    }

    #[test]
    fn state_stream_transcript_caps_and_incomplete_snapshots_stay_hint_only() {
        let delta = json!({"delta":true,"items":[{"id":"answer","role":"assistant","text":"after"}],
            "removed":[],"itemRevisions":{"answer":studio_runtime_store::sha256_hex(b"after")},"truncated":false});
        for scenario in [
            "complete", "items", "snapshot", "deleted", "cursor", "unknown",
        ] {
            let mut payload = delta.clone();
            match scenario {
                "items" => {
                    payload["items"] = json!(
                    (0..=SYNC_TRANSCRIPT_STREAM_MAX_ITEMS)
                        .map(|index| json!({"id":index.to_string(),"role":"assistant","text":"x"}))
                        .collect::<Vec<_>>()
                )
                }
                "snapshot" => {
                    payload.as_object_mut().expect("object").remove("delta");
                }
                "unknown" => payload["unexpected"] = json!(true),
                _ => {}
            }
            let response: TranscriptSyncPage = serde_json::from_value(json!({
                "workspaceId":"workspace","checkpoint":{"seq":2},"documents":[{
                    "id":"transcript:open","seq":if scenario=="cursor" {3} else {2},
                    "payload":payload.to_string(),"_deleted":scenario=="deleted",
                }],
            }))
            .expect("typed projection response");
            assert_eq!(
                transcript_change_batch(response, "transcript:open", 1).is_some(),
                scenario == "complete",
                "{scenario}"
            );
        }
    }

    #[tokio::test]
    async fn state_stream_draft_batches_keep_limits_and_recovery() {
        for scenario in ["maximum", "documents", "payload"] {
            let root = tempfile::tempdir().expect("isolated state");
            let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
            store.transaction(move |tx| {
                let count = match scenario {
                    "maximum" => SYNC_DRAFT_STREAM_MAX_DOCUMENTS,
                    "documents" => SYNC_DRAFT_STREAM_MAX_DOCUMENTS + 1,
                    _ => 1,
                };
                for index in 0..count {
                    let session = format!("chat-{index}");
                    let payload = if scenario == "payload" { "broken".to_owned() } else {
                        json!({"device":"phone", "session":session, "text": "draft", "updated":1}).to_string()
                    };
                    SyncDocumentsRepository::put_draft(tx, &format!("phone:{session}"), &payload, false)?;
                }
                Ok(())
            }).await.expect("draft interval");
            let resources = [json!({"kind":"drafts"})];
            let (through, batch) = read_draft_changes(Some(store.clone()), &resources, Some(0))
                .await
                .expect("batch");
            if scenario == "maximum" {
                assert_eq!(
                    batch.expect("maximum fits").documents.len() as i64,
                    SYNC_DRAFT_STREAM_MAX_DOCUMENTS
                );
            } else {
                assert!(batch.is_none(), "{scenario} needs pull recovery");
            }
            assert!(
                read_draft_changes(Some(store.clone()), &resources, None)
                    .await
                    .expect("initial")
                    .1
                    .is_none()
            );
            assert!(
                read_draft_changes(Some(store), &resources, Some(through + 1))
                    .await
                    .expect("future checkpoint")
                    .1
                    .is_none()
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn state_stream_delivers_draft_edits_and_tombstones_without_pull() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let http = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store.clone()),
            None,
        );
        let resources = vec![json!({"kind":"drafts"})];
        http.resource_notifications
            .changes_since_with_content(&resources, None, |resource| {
                resource_content(http.clone(), resource)
            })
            .await
            .expect("baseline");
        let mut state = test_stream(
            &http.sync_changes,
            &http.token_rates,
            http.resource_notifications.clone(),
            resources,
        );
        state.store = Some(store.clone());
        state.http_state = Some(http);
        let raw = r#"{"device":"phone","session":"chat","text":"legacy bytes","updated":1.5}"#;
        store.transaction(move |tx| {
            SyncDocumentsRepository::put_draft(tx, "phone:chat", raw, false)?;
            SyncDocumentsRepository::put_draft(tx, "phone:removed", r#"{"id":"phone:removed","device":"phone","session":"removed","text":"","updated":2}"#, true)?;
            Ok(())
        }).await.expect("draft changes");
        let (event, _) = next_frame(state).await.expect("draft frame");
        let event = event_document(event).await;
        let batch = &event["resourceVersions"][0]["draftChanges"];
        assert_eq!(
            batch["after"], 0,
            "draft batches prove their starting checkpoint"
        );
        assert_eq!(batch["through"], event["resourceVersions"][0]["revision"]);
        assert_eq!(
            batch["documents"][0]["payload"], raw,
            "legacy payload bytes stay unchanged"
        );
        assert_eq!(batch["documents"][1]["_deleted"], true);
    }

    #[tokio::test]
    async fn unchanged_drafts_ignore_unrelated_commits_and_publications() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store.clone()),
            None,
        );
        let resources = [json!({"kind":"drafts"})];
        let baseline = state
            .resource_notifications
            .changes_since_with_content(&resources, None, |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("baseline");
        assert_eq!(baseline.resources, resources);
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('other',?1)",
                    [json!({"id":"other","status":"idle"}).to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("unrelated broad commit");
        state
            .resource_notifications
            .publish_once(&json!({
                "requestId":"unchanged-drafts", "resources":resources
            }))
            .expect("redundant external hint");
        let unchanged = state
            .resource_notifications
            .changes_since_with_content(&resources, Some(baseline.cursor.revision), |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("unchanged drafts");
        assert!(
            unchanged.resources.is_empty(),
            "unchanged drafts must cause zero pulls"
        );
        store
            .transaction(|tx| {
                SyncDocumentsRepository::put_draft(tx, "phone:chat", "draft", false)?;
                Ok(())
            })
            .await
            .expect("draft edit");
        let edited = state
            .resource_notifications
            .changes_since_with_content(&resources, Some(unchanged.cursor.revision), |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("edited draft");
        assert_eq!(edited.resources, resources, "real draft edits still notify");
        store
            .transaction(|tx| {
                SyncDocumentsRepository::put_draft(tx, "phone:chat", "draft", true)?;
                Ok(())
            })
            .await
            .expect("draft deletion");
        let deleted = state
            .resource_notifications
            .changes_since_with_content(&resources, Some(edited.cursor.revision), |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("deleted draft");
        assert_eq!(
            deleted.resources, resources,
            "draft tombstones still notify"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn state_stream_delivers_complete_coalesced_records_and_tombstones() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let hub = Arc::new(ChangeNotificationHub::new());
        let notifications = Arc::new(ResourceNotifications::new(hub.clone()));
        let tokens = TokenRateHub::new();
        let mut state = test_stream(&hub, &tokens, notifications, vec![json!({"kind":"state"})]);
        state.store = Some(store.clone());
        state.entity_sequence = read_entity_sequence(Some(store.clone()), &state.resources)
            .await
            .expect("baseline sequence");
        let baseline = state.entity_sequence;
        store
            .transaction(|tx| {
                SyncDocumentsRepository::put_entity(
                    tx,
                    "agent",
                    "first",
                    json!({"id":"first","name":"superseded","status":"idle","source":"managed"}),
                    false,
                )?;
                SyncDocumentsRepository::put_entity(
                    tx,
                    "agent",
                    "first",
                    json!({"id":"first","name":"First","status":"idle","source":"managed"}),
                    false,
                )?;
                SyncDocumentsRepository::put_entity(tx, "agent", "removed", json!({}), true)?;
                Ok(())
            })
            .await
            .expect("records");
        hub.notify_one();
        hub.notify_one();
        let (event, _) = next_frame(state).await.expect("state frame");
        let event = event_document(event).await;
        assert_eq!(
            event["reason"], "change",
            "a complete coalesced range is ordinary data"
        );
        let batch = &event["resourceVersions"][0]["entityChanges"];
        assert_eq!(batch["after"], baseline);
        assert_eq!(batch["through"], baseline + 3);
        assert_eq!(batch["documents"][0]["value"]["name"], "First");
        assert_eq!(
            batch["documents"].as_array().expect("full records").len(),
            2
        );
        assert_eq!(batch["documents"][1]["op"], "delete");
        assert!(event["resourceVersions"][0]["revision"] == batch["through"]);
    }

    #[tokio::test]
    async fn state_stream_accepts_the_full_contract_document_limit() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let resources = vec![json!({"kind":"state"})];
        let baseline = read_entity_sequence(Some(store.clone()), &resources)
            .await
            .expect("baseline");
        store
            .transaction(|tx| {
                for index in 0..SYNC_ENTITY_STREAM_MAX_DOCUMENTS {
                    SyncDocumentsRepository::put_entity(
                        tx,
                        "agent",
                        &format!("deleted-{index}"),
                        json!({}),
                        true,
                    )?;
                }
                Ok(())
            })
            .await
            .expect("maximum batch");
        let (sequence, batch) = read_entity_changes(Some(store), &resources, Some(baseline))
            .await
            .expect("batch read");
        let batch = batch.expect("all documents fit");
        assert_eq!(
            batch.documents.len() as i64,
            SYNC_ENTITY_STREAM_MAX_DOCUMENTS
        );
        assert_eq!(batch.through, sequence);
    }

    #[tokio::test]
    async fn state_stream_falls_back_for_oversize_pruned_or_unreadable_intervals() {
        for scenario in ["documents", "bytes", "pruned", "payload"] {
            let root = tempfile::tempdir().expect("isolated state");
            let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
            let hub = Arc::new(ChangeNotificationHub::new());
            let tokens = TokenRateHub::new();
            let notifications = Arc::new(ResourceNotifications::new(hub.clone()));
            let mut state =
                test_stream(&hub, &tokens, notifications, vec![json!({"kind":"state"})]);
            state.store = Some(store.clone());
            store.transaction(move |tx| {
                let count = if scenario == "documents" { 513 } else { 1 };
                for index in 0..count {
                    let id = format!("agent-{index}");
                    let name = if scenario == "bytes" { "é".repeat(262144) } else { "Agent".into() };
                    SyncDocumentsRepository::put_entity(tx, "agent", &id, json!({"id":id,"name":name,"status":"idle","source":"managed"}), false)?;
                }
                if scenario == "pruned" {
                    tx.execute("INSERT OR REPLACE INTO sync_entity_meta(key,value) VALUES('entity_tombstone_floor','1')", [])?;
                }
                if scenario == "payload" {
                    tx.execute("UPDATE sync_entities SET payload='broken'", [])?;
                }
                Ok(())
            }).await.expect("interval");
            hub.notify_one();
            let (event, _) = next_frame(state).await.expect("revision fallback");
            let event = event_document(event).await;
            assert!(
                event["resourceVersions"][0].get("entityChanges").is_none(),
                "{scenario}"
            );
            assert!(
                event["resourceVersions"][0]["revision"]
                    .as_i64()
                    .expect("revision")
                    > 0
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn state_stream_merges_changes_inside_the_200_ms_flush_window() {
        use futures_util::FutureExt;
        let hub = Arc::new(ChangeNotificationHub::new());
        let tokens = TokenRateHub::new();
        let notifications = Arc::new(ResourceNotifications::new(hub.clone()));
        let state = test_stream(
            &hub,
            &tokens,
            notifications,
            vec![json!({"kind":"accounts"})],
        );
        hub.notify_one();
        let (_, state) = next_frame(state).await.expect("first frame");
        let mut next = Box::pin(next_frame(state));
        for _ in 0..5 {
            hub.notify_one();
            tokio::time::advance(Duration::from_millis(30)).await;
            assert!(
                next.as_mut().now_or_never().is_none(),
                "no intermediate frame inside 200 ms"
            );
        }
        tokio::time::advance(Duration::from_millis(50)).await;
        let (event, _) = next.await.expect("merged frame");
        assert_eq!(event_document(event).await["revision"], 6);
    }

    #[tokio::test]
    async fn state_stream_streaming_delta_in_open_chat_invalidates_its_transcript() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store.clone()),
            None,
        );
        let resources = vec![json!({"kind":"transcript","agentId":"open"})];
        let baseline = state
            .resource_notifications
            .changes_since_with_content(&resources, None, |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("baseline");
        let before = store
            .read_async("streaming-delta-witness", |connection| {
                studio_runtime_store::TranscriptRepository::revision(connection, "open")
            })
            .await
            .expect("stored conversation revision");
        // A scoped external stream publication can precede the next stored item flush.
        let publication = json!({"requestId":"stream-delta-1","resources":resources});
        assert!(
            state
                .resource_notifications
                .publish_once(&publication)
                .expect("delta publication")
        );
        let after = store
            .read_async("streaming-delta-witness", |connection| {
                studio_runtime_store::TranscriptRepository::revision(connection, "open")
            })
            .await
            .expect("same stored conversation revision");
        let changed = state
            .resource_notifications
            .changes_since_with_content(&resources, Some(baseline.cursor.revision), |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("streaming delta");
        assert_eq!(
            changed.resources, resources,
            "an open chat must pull its streamed delta even before its stored revision changes"
        );
        assert_eq!(
            before, after,
            "the witness keeps the stored conversation revision unchanged"
        );
        assert!(
            !state
                .resource_notifications
                .publish_once(&publication)
                .expect("exact retry")
        );
        let replay = state
            .resource_notifications
            .changes_since_with_content(&resources, Some(changed.cursor.revision), |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("replay");
        assert!(
            replay.resources.is_empty(),
            "a retried publication must not cause a repeated pull"
        );
        state.resource_notifications.publish_once(&json!({"requestId":"other-chat-delta","resources":[{"kind":"transcript","agentId":"other"}]})).expect("other stream");
        let unrelated = state
            .resource_notifications
            .changes_since_with_content(&resources, Some(replay.cursor.revision), |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("other chat");
        assert!(
            unrelated.resources.is_empty(),
            "another chat's stream must not pull the open transcript"
        );
    }

    #[tokio::test]
    async fn unchanged_open_chat_ignores_unrelated_agent_commits() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store.clone()),
            None,
        );
        let resources = vec![json!({"kind":"transcript","agentId":"open"})];
        let baseline = state
            .resource_notifications
            .changes_since_with_content(&resources, None, |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("baseline");
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('other',?1)",
                    [json!({"id":"other","status":"idle"}).to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("unrelated commit");
        state.sync_changes.notify_one();
        let unchanged = state
            .resource_notifications
            .changes_since_with_content(&resources, Some(baseline.cursor.revision), |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("unchanged conversation");
        assert!(
            unchanged.resources.is_empty(),
            "an unrelated agent must cause zero open-chat pulls"
        );
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('open',?1)",
                    [json!({"id":"open","status":"running"}).to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("active conversation");
        state.sync_changes.notify_one();
        let active = state
            .resource_notifications
            .changes_since_with_content(&resources, Some(unchanged.cursor.revision), |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("active conversation revision");
        assert_eq!(active.resources, resources);
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_items(id,agent,record,created) VALUES('text','open',?1,1)",
                    [json!({"id":"text","text":"Live transcript text"}).to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("active text changes");
        state.sync_changes.notify_one();
        let text = state
            .resource_notifications
            .changes_since_with_content(&resources, Some(active.cursor.revision), |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("active text revision");
        assert_eq!(
            text.resources, resources,
            "active transcript text still requests a pull"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn sync_stream_manifest_is_implemented_on_hosts_without_the_windows_file_identity_gap() {
        let operation = crate::routes::manifest::operation_manifest()
            .expect("route manifest validates")
            .into_iter()
            .find(|operation| operation.key() == ("GET".to_owned(), "/api/sync/stream".to_owned()))
            .expect("sync stream operation is registered");
        assert_eq!(
            serde_json::to_value(operation).expect("operation serializes")["status"],
            "implemented"
        );
    }

    mod token_rate_difference {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../runtime-services/tests/support/token_rate_difference.rs"
        ));
    }

    #[tokio::test]
    async fn limits_content_reads_only_cached_account_snapshots() {
        use std::sync::Mutex;
        use studio_runtime_services::account_policy::AccountPolicyState;
        use studio_runtime_services::account_session_runtime::{
            AccountSessionError, AccountSessionFuture, AccountSessionPort,
        };

        struct CachedLimits(Mutex<Value>);
        impl AccountSessionPort for CachedLimits {
            fn limits_snapshot(&self, account: &str) -> Result<Value, AccountSessionError> {
                Ok(if account == "first" {
                    self.0.lock().expect("snapshot").clone()
                } else {
                    json!({"accountKey":account,"data":null,"error":null,"at":null})
                })
            }

            fn read_limits<'a>(&'a self, _: &'a str) -> AccountSessionFuture<'a, Value> {
                panic!("content filtering must never read the provider, even with a cold cache")
            }

            fn refresh_limits<'a>(&'a self, _: &'a str) -> AccountSessionFuture<'a, ()> {
                panic!("content filtering must never enqueue a native refresh")
            }
        }

        let root = tempfile::tempdir().expect("isolated state");
        let policy = AccountPolicyState::from_startup(
            Some(br#"{"version":1,"defaultAccountKey":"first","accounts":{},"logins":{}}"#),
            [],
        )
        .expect("policy");
        let mut state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            None,
            Some(Arc::new(policy)),
        );
        let resources = [
            json!({"kind":"limits","accountKey":"first"}),
            json!({"kind":"limits","accountKey":"second"}),
        ];
        let cold = resource_content(state.clone(), resources[0].clone())
            .await
            .expect("policy-only cold snapshot is filterable");
        let port = Arc::new(CachedLimits(Mutex::new(
            json!({"accountKey":"first","data":null,"error":null,"at":null}),
        )));
        state.account_session = Some(port.clone());
        assert_eq!(
            cold,
            resource_content(state.clone(), resources[0].clone())
                .await
                .expect("session cold snapshot is filterable")
        );
        let notifications = &state.resource_notifications;
        let initial = notifications
            .changes_since_with_content(&resources, None, |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("baseline");
        assert_eq!(initial.resources, resources);
        notifications.notify_store_commit(studio_runtime_store::ResourceChange::All);
        let unchanged = notifications
            .changes_since_with_content(&resources, Some(initial.cursor.revision), |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("unrelated commit");
        assert!(
            unchanged.resources.is_empty(),
            "cold limits do not cause client reads"
        );
        *port.0.lock().expect("snapshot") =
            json!({"accountKey":"first","data":null,"error":"Unavailable","checkedAt":1.0});
        notifications.notify_store_commit(studio_runtime_store::ResourceChange::All);
        let changed = notifications
            .changes_since_with_content(&resources, Some(unchanged.cursor.revision), |resource| {
                resource_content(state.clone(), resource)
            })
            .await
            .expect("changed cached snapshot");
        assert_eq!(changed.resources, vec![resources[0].clone()]);
        assert_eq!(changed.versions, vec![changed.cursor.revision]);

        *port.0.lock().expect("snapshot") = json!({
            "accountKey":"first", "data":{"accountId":"native-first"},
            "at":1.0, "readAt":1.0, "checkedAt":1.0, "processedAt":1.0
        });
        let before = resource_content(state.clone(), resources[0].clone())
            .await
            .expect("semantic limits");
        for field in studio_runtime_store::account_state::LIMITS_OBSERVATION_TIME_FIELDS {
            port.0.lock().expect("snapshot")[*field] = json!(120.0);
        }
        assert_eq!(
            before,
            resource_content(state.clone(), resources[0].clone())
                .await
                .expect("reobserved limits"),
            "private observation times must not invalidate streamed limits"
        );
        port.0.lock().expect("snapshot")["data"]["accountId"] = json!("replacement");
        assert_ne!(
            before,
            resource_content(state, resources[0].clone())
                .await
                .expect("changed account limits")
        );
    }

    #[tokio::test]
    async fn bounded_entities_never_reconstruct_resource_content() {
        let root = tempfile::tempdir().expect("isolated state");
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            None,
            None,
        );
        for resource in [
            json!({"kind":"accounts"}),
            json!({"kind":"costs"}),
            json!({"kind":"desktop"}),
            json!({"kind":"terminals"}),
            json!({"kind":"queue","agentId":"agent"}),
            json!({"kind":"session-cost","agentId":"agent"}),
        ] {
            assert!(resource_content(state.clone(), resource).await.is_none());
        }
    }

    #[tokio::test]
    async fn stream_documents_match_frozen_resource_hub() {
        let rate = json!({"agentId":"agent","rootId":"lead","turnId":"turn", "active":false,"estimated":true,"rate":1.25,"outputTokens":400});
        let cases = json!([
            {"resources":[{"kind":"accounts"},{"kind":"session-cost","agentId":"agent"}],"sequence":0,"reconnect":false,"rates":{"records":[]}},
            {"resources":[{"kind":"state"}],"sequence":42,"reconnect":true,"rates":{"records":[rate]}}
        ]);
        let expected: Vec<Value> =
            serde_json::from_str(include_str!("fixtures/sync_stream_documents.json"))
                .expect("frozen reference documents");
        assert_eq!(expected.len(), cases.as_array().expect("cases").len());
        for (case, expected) in cases.as_array().expect("cases").iter().zip(expected) {
            let reason = if case["reconnect"] == true {
                "reconnect"
            } else {
                "initial"
            };
            let resources = resource_event(
                "workspace",
                "epoch",
                0,
                reason,
                case["resources"].as_array().expect("resources"),
                case["sequence"].as_i64().expect("sequence"),
                false,
            );
            assert_eq!(resources, expected["resources"]);
            let event = token_rate_event(
                "workspace",
                &ChangeSnapshot {
                    epoch: "epoch".into(),
                    revision: 0,
                    resource_revision: 0,
                    closed: false,
                },
                &TokenRates::from_value(case["rates"].clone()).expect("rates"),
            );
            let body = axum::body::to_bytes(
                Sse::new(stream::iter([Ok::<_, Infallible>(event)]))
                    .into_response()
                    .into_body(),
                4096,
            )
            .await
            .expect("frame");
            let frame = String::from_utf8(body.to_vec()).expect("UTF8");
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("data");
            token_rate_difference::assert_token_rate_documents_equal(
                &expected["rates"],
                &serde_json::from_str::<Value>(data).expect("JSON"),
                &[],
                "frozen stream document",
            );
        }
    }

    #[tokio::test]
    async fn coalesced_revisions_keep_the_latest_ordinary_resource_event() {
        let hub = ChangeNotificationHub::new();
        let tokens = TokenRateHub::new();
        let mut changes = hub.subscribe();
        let baseline = changes.borrow_and_update().clone();
        let state = ResourceStream {
            http_state: None,
            vault_readiness: None,
            subscription: None,
            initial: VecDeque::new(),
            workspace_id: "workspace".to_owned(),
            epoch: baseline.epoch,
            last_revision: baseline.revision,
            resources: vec![json!({"kind": "queue", "agentId": "agent-a"})],
            entity_sequence: 0,
            transcript_sequences: BTreeMap::new(),
            voice_sequences: BTreeMap::new(),
            draft_sequence: 0,
            store: None,
            changes,
            notifications: Arc::new(ResourceNotifications::new(Arc::new(hub.clone()))),
            token_rates: Some(tokens.subscribe()),
            last_token_rates: None,
            consecutive_token_frames: 0,
            next_token_frame_at: None,
            next_resources_frame_at: None,
            heartbeat: interval_at(
                Instant::now() + Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
                Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
            ),
        };
        hub.notify_one();
        hub.notify_one();
        hub.notify_one();

        let (event, state) = next_frame(state).await.expect("change frame");
        assert_eq!(state.last_revision, 3);
        let response = Sse::new(stream::iter([event])).into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("SSE body");
        let frame = String::from_utf8(body.to_vec()).expect("UTF-8 SSE frame");
        assert!(frame.contains("id: 3"), "{frame}");
        assert!(frame.contains("\"reason\":\"change\""), "{frame}");
        assert!(frame.contains("\"revision\":3"), "{frame}");
    }

    #[tokio::test]
    async fn token_rate_burst_keeps_only_latest_snapshot_and_monotone_cursor() {
        let hub = Arc::new(ChangeNotificationHub::new());
        let notifications = Arc::new(ResourceNotifications::new(hub.clone()));
        let token_hub = TokenRateHub::with_change_hub(hub.clone());
        let changes = hub.subscribe();
        let baseline = changes.borrow().clone();
        let state = ResourceStream {
            http_state: None,
            vault_readiness: None,
            subscription: None,
            initial: VecDeque::new(),
            workspace_id: "workspace".to_owned(),
            epoch: baseline.epoch,
            last_revision: baseline.revision,
            resources: vec![json!({"kind":"accounts"})],
            entity_sequence: 0,
            transcript_sequences: BTreeMap::new(),
            voice_sequences: BTreeMap::new(),
            draft_sequence: 0,
            store: None,
            changes,
            notifications: notifications.clone(),
            token_rates: Some(token_hub.subscribe()),
            last_token_rates: None,
            consecutive_token_frames: 0,
            next_token_frame_at: None,
            next_resources_frame_at: None,
            heartbeat: interval_at(
                Instant::now() + Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
                Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
            ),
        };
        const BURST_PUBLICATIONS: usize = 3;
        for index in 0..BURST_PUBLICATIONS {
            token_hub.publish(json!({
                "records": [{"agentId":"agent","rootId":"agent","turnId":"turn", "active":true, "estimated":false, "rate":index, "outputTokens":index}]
            })).expect("publish");
        }
        let (event, state) = next_frame(state).await.expect("token frame");
        assert!(state.initial.is_empty());
        let body = axum::body::to_bytes(
            Sse::new(stream::iter([event])).into_response().into_body(),
            4096,
        )
        .await
        .expect("frame");
        let frame = String::from_utf8(body.to_vec()).expect("UTF8");
        assert!(frame.contains("event: token-rates"), "{frame}");
        assert!(
            frame.contains(&format!("id: {BURST_PUBLICATIONS}")),
            "{frame}"
        );
        assert!(
            frame.contains(&format!("\"rate\":{}", BURST_PUBLICATIONS - 1)),
            "{frame}"
        );
        drop(state); // Disconnect drops the only receiver; later publications remain bounded.
        assert!(
            token_hub
                .publish(json!({"records":[]}))
                .expect("publish disconnected")
        );
    }

    /// Token frames are spaced by `SYNC_FRAME_MIN_INTERVAL`, so a pending resource change is
    /// sent right after the first token frame instead of waiting behind a token burst, and
    /// the burst collapses into the newest snapshot once the window has passed.
    #[tokio::test(start_paused = true)]
    async fn pending_resource_change_goes_out_before_the_next_spaced_token_frame() {
        let hub = Arc::new(ChangeNotificationHub::new());
        let notifications = Arc::new(ResourceNotifications::new(hub.clone()));
        let token_hub = TokenRateHub::with_change_hub(hub.clone());
        let state = test_stream(
            &hub,
            &token_hub,
            notifications,
            vec![json!({"kind":"accounts"})],
        );
        hub.notify_one();
        token_hub.publish(test_rates(1)).expect("token rate");
        let (token, state) = next_frame(state).await.expect("first token event at once");
        let token = event_document(token).await;
        assert_eq!(token["records"][0]["rate"].as_f64(), Some(1.0));

        for rate in 2..=8 {
            token_hub.publish(test_rates(rate)).expect("token burst");
        }
        let started = Instant::now();
        let (resource, state) = next_frame(state).await.expect("resource event");
        let resource = event_document(resource).await;
        assert_eq!(resource["resources"], json!([{"kind":"accounts"}]));
        assert!(started.elapsed() < SYNC_FRAME_MIN_INTERVAL);

        let (token, _state) = next_frame(state).await.expect("latest token snapshot");
        assert!(started.elapsed() >= SYNC_FRAME_MIN_INTERVAL - Duration::from_millis(1));
        let token = event_document(token).await;
        assert_eq!(token["records"][0]["rate"].as_f64(), Some(8.0));
    }

    #[test]
    fn state_reset_frame_preserves_position_and_sequence() {
        let event = resource_event(
            "workspace",
            "epoch",
            4,
            "change",
            &[json!({"kind":"accounts"}), json!({"kind":"state"})],
            2,
            true,
        );
        assert_eq!(
            event["resourceVersions"],
            json!([{"revision":4}, {"revision":2,"entitySequenceReset":true}])
        );
    }

    fn test_rates(rate: u64) -> Value {
        json!({"records":[{"agentId":"agent","rootId":"agent","turnId":"turn","active":true,"estimated":false,"rate":rate,"outputTokens":rate}]})
    }

    fn floating_test_rates(rate: u64) -> Value {
        json!({"records":[{"agentId":"agent","rootId":"agent","turnId":"turn","active":true,"estimated":false,"rate":rate as f64,"outputTokens":rate as f64}]})
    }

    #[tokio::test]
    async fn state_stream_discovery_delivers_current_access_snapshot_and_later_changes() {
        let root = tempfile::tempdir().expect("isolated discovery state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let http = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            Some("https://owner.example".into()),
            Some(store.clone()),
            None,
        );
        let resource = json!({"kind":"multi-server"});
        let mut state = test_stream(
            &http.sync_changes,
            &http.token_rates,
            http.resource_notifications.clone(),
            vec![],
        );
        state.http_state = Some(http.clone());
        state.store = Some(store.clone());
        subscription_frame(
            &mut state,
            SubscriptionChange {
                generation: 1,
                resources: vec![resource.clone()],
                added: vec![resource.clone()],
                token_rates: None,
            },
        )
        .await
        .expect("subscription");
        let ack = event_document(Ok(state
            .initial
            .pop_front()
            .expect("subscription acknowledgement")))
        .await;
        assert_eq!(ack["gen"], 1);
        let event = event_document(Ok(state.initial.pop_front().expect("baseline"))).await;
        let snapshot = &event["resourceVersions"][0]["discoverySnapshot"];
        assert_eq!(snapshot["protocol"], 1);
        assert!(snapshot["identity"]["serverId"].as_str().is_some());
        store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_access_settings(id,auto_pair) VALUES(1,0) ON CONFLICT(id) DO UPDATE SET auto_pair=0", [])?;
            Ok(())
        }).await.expect("settings change");
        http.resource_notifications.publish_once(&json!({"requestId":"discovery-settings-change","resources":[{"kind":"multi-server"}]})).expect("committed discovery hint");
        let (frame, _) = next_frame(state).await.expect("discovery update");
        let event = event_document(frame).await;
        assert_eq!(
            event["resourceVersions"][0]["discoverySnapshot"]["settings"]["autoPair"],
            false
        );
    }

    #[tokio::test]
    async fn state_stream_oversized_transient_voice_payload_retains_only_a_recovery_hint() {
        let mut value = resource_event(
            "workspace",
            "epoch",
            1,
            "change",
            &[json!({"kind":"voice","agentId":"chat"})],
            0,
            false,
        );
        value["resourceVersions"][0]["voiceRecords"] = json!({"after":0,"cursor":1,"session":null,"records":[{
            "id":"large","agent":"chat","session":"session","kind":"user","seq":1,"text":"x".repeat(SYNC_STREAM_EVENT_MAX_BYTES),"item_id":"","previous_item_id":"","payload":"{}","created":1.0
        }]});
        let event = serde_json::from_value(value).expect("typed transient payload");
        let bytes = frame_bytes(resource_sse_event(1, event)).await;
        assert!(bytes.len() <= SYNC_STREAM_EVENT_MAX_BYTES);
        let event: Value = serde_json::from_str(
            std::str::from_utf8(&bytes)
                .expect("UTF8")
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("data"),
        )
        .expect("JSON");
        assert_eq!(
            event["resources"],
            json!([{"kind":"voice","agentId":"chat"}])
        );
        assert!(event["resourceVersions"][0].get("voiceRecords").is_none());
    }

    #[tokio::test]
    async fn state_stream_voice_delivers_records_and_session_without_a_followup_read() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('voice-chat',?1)",
                [json!({"id":"voice-chat","rootId":"voice-chat","epoch":0,"status":"idle","isLead":true}).to_string()])?;
            tx.execute("INSERT INTO voice_sessions(id,agent,created,state) VALUES('session','voice-chat',1,'ready')", [])?;
            tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('first','voice-chat','session','user','hello','','','{}',1)", [])?;
            Ok(())
        }).await.expect("voice fixture");
        let http = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store.clone()),
            None,
        );
        let mut state = test_stream(
            &http.sync_changes,
            &http.token_rates,
            http.resource_notifications.clone(),
            vec![json!({"kind":"voice","agentId":"voice-chat"})],
        );
        state.http_state = Some(http.clone());
        state.store = Some(store.clone());
        http.sync_changes.notify_one();
        let (frame, state) = next_frame(state).await.expect("voice event");
        let event = event_document(frame).await;
        let batch = &event["resourceVersions"][0]["voiceRecords"];
        assert_eq!(batch["records"][0]["id"], "first");
        assert_eq!(batch["session"]["session_id"], "session");
        let cursor = batch["cursor"].as_i64().expect("cursor");
        store.transaction(|tx| {
            tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('second','voice-chat','session','assistant','answer','','','{}',2)", [])?;
            Ok(())
        }).await.expect("voice update");
        http.sync_changes.notify_one();
        let (frame, state) = next_frame(state).await.expect("voice delta");
        let event = event_document(frame).await;
        let batch = &event["resourceVersions"][0]["voiceRecords"];
        assert_eq!(batch["after"], cursor);
        assert_eq!(batch["records"].as_array().expect("records").len(), 1);
        assert_eq!(batch["records"][0]["id"], "second");
        store.transaction(|tx| {
            for index in 0..=studio_runtime_schema::shared_values::SYNC_VOICE_STREAM_MAX_RECORDS {
                tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES(?1,'voice-chat','session','user','burst','','','{}',3)", [format!("burst-{index}")])?;
            }
            Ok(())
        }).await.expect("bounded history fixture");
        http.sync_changes.notify_one();
        let (frame, state) = next_frame(state).await.expect("large interval hint");
        let event = event_document(frame).await;
        assert!(
            event["resourceVersions"][0].get("voiceRecords").is_none(),
            "large intervals require HTTP recovery"
        );
        let after_recovery = state.voice_sequences["voice-chat"];
        store.transaction(|tx| {
            tx.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('after-recovery','voice-chat','session','user','next','','','{}',4)", [])?;
            Ok(())
        }).await.expect("later record");
        http.sync_changes.notify_one();
        let (frame, _) = next_frame(state).await.expect("resume inline delivery");
        let event = event_document(frame).await;
        let batch = &event["resourceVersions"][0]["voiceRecords"];
        assert_eq!(batch["after"], after_recovery);
        assert_eq!(batch["records"][0]["id"], "after-recovery");
    }

    fn test_stream(
        hub: &Arc<ChangeNotificationHub>,
        tokens: &TokenRateHub,
        notifications: Arc<ResourceNotifications>,
        resources: Vec<Value>,
    ) -> ResourceStream {
        let changes = hub.subscribe();
        let baseline = changes.borrow().clone();
        ResourceStream {
            http_state: None,
            vault_readiness: None,
            subscription: None,
            initial: VecDeque::new(),
            workspace_id: "workspace".into(),
            epoch: baseline.epoch,
            last_revision: baseline.revision,
            resources,
            entity_sequence: 0,
            transcript_sequences: BTreeMap::new(),
            voice_sequences: BTreeMap::new(),
            draft_sequence: 0,
            store: None,
            changes,
            notifications,
            token_rates: Some(tokens.subscribe()),
            last_token_rates: None,
            consecutive_token_frames: 0,
            next_token_frame_at: None,
            next_resources_frame_at: None,
            heartbeat: interval_at(
                Instant::now() + Duration::from_secs(15),
                Duration::from_secs(15),
            ),
        }
    }

    async fn event_document(event: Result<Event, Infallible>) -> Value {
        let body = axum::body::to_bytes(
            Sse::new(stream::iter([event])).into_response().into_body(),
            262144,
        )
        .await
        .expect("frame");
        let frame = String::from_utf8(body.to_vec()).expect("UTF8");
        serde_json::from_str(
            frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("data"),
        )
        .expect("document")
    }

    #[test]
    fn token_snapshot_keeps_cursor_guarded_during_a_competing_publication() {
        use std::sync::mpsc;
        let hub = Arc::new(ChangeNotificationHub::new());
        let tokens = Arc::new(TokenRateHub::with_change_hub(hub.clone()));
        tokens.publish(floating_test_rates(1)).expect("first");
        let mut reader = tokens.subscribe();
        let (captured, capture_gate) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let reading = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                token_snapshot(&mut reader, || {
                    captured.send(()).expect("rate read gate");
                    released.recv().expect("cursor read gate");
                    hub.snapshot()
                })
            })
        };
        capture_gate.recv().expect("reader holds rate guard");
        let (attempting, attempted) = mpsc::channel();
        let publishing = {
            let tokens = tokens.clone();
            std::thread::spawn(move || {
                attempting.send(()).expect("publication gate");
                tokens.publish(floating_test_rates(2)).expect("second")
            })
        };
        attempted
            .recv()
            .expect("publication admitted while pair read is gated");
        release
            .send(())
            .expect("read cursor before releasing rate guard");
        let (rates, cursor) = reading.join().expect("reader");
        assert_eq!(rates.to_value(), floating_test_rates(1));
        assert_eq!(cursor.revision, 1);
        assert!(publishing.join().expect("publisher"));
        let (rates, cursor) = token_snapshot(&mut tokens.subscribe(), || hub.snapshot());
        assert_eq!(rates.to_value(), floating_test_rates(2));
        assert_eq!(cursor.revision, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn gated_publications_between_reader_polls_cannot_starve_tokens_or_invalidate_resources()
    {
        use futures_util::StreamExt;
        let hub = Arc::new(ChangeNotificationHub::new());
        let tokens = Arc::new(TokenRateHub::with_change_hub(hub.clone()));
        let notifications = Arc::new(ResourceNotifications::new(hub.clone()));
        let mut first = Box::pin(stream::unfold(
            test_stream(
                &hub,
                &tokens,
                notifications.clone(),
                vec![json!({"kind":"accounts"})],
            ),
            next_frame,
        ));
        let mut second = Box::pin(stream::unfold(
            test_stream(
                &hub,
                &tokens,
                notifications.clone(),
                vec![json!({"kind":"accounts"})],
            ),
            next_frame,
        ));
        for rate in 1..=4 {
            let (publish, gate) = tokio::sync::oneshot::channel();
            let producer = {
                let tokens = tokens.clone();
                tokio::spawn(async move {
                    gate.await.expect("reader gate");
                    tokens.publish(floating_test_rates(rate)).expect("publish")
                })
            };
            publish
                .send(())
                .expect("release one publication between polls");
            assert!(producer.await.expect("producer"));
            let document = event_document(first.next().await.expect("token frame")).await;
            assert_eq!(document["records"], floating_test_rates(rate)["records"]);
            assert_eq!(document["revision"], rate);
            assert!(document.get("resources").is_none());
        }
        let latest = event_document(second.next().await.expect("slow reader latest")).await;
        assert_eq!(latest["records"], floating_test_rates(4)["records"]);
        let mut pending = first.next();
        assert!(
            futures_util::poll!(&mut pending).is_pending(),
            "token-only changes produced an unrelated frame"
        );
        notifications
            .publish_once(&json!({"requestId":"account-change","resources":[{"kind":"accounts"}]}))
            .expect("resource publication");
        let document = event_document(pending.await.expect("resource frame")).await;
        assert_eq!(document["resources"], json!([{"kind":"accounts"}]));
        assert_eq!(document["resourceVersions"], json!([{"revision":5}]));
    }

    #[tokio::test(start_paused = true)]
    async fn post_publication_documents_match_frozen_interleaved_consumers() {
        use futures_util::{FutureExt, StreamExt};
        let cases = json!([
            {"resources":[{"kind":"accounts"},{"kind":"queue","agentId":"agent"}],"steps":[
                {"op":"notify","resources":[{"kind":"models"}]},{"op":"consume"},
                {"op":"rates","body":test_rates(1)},{"op":"consume"},
                {"op":"notify","resources":[{"kind":"accounts"}]},{"op":"consume"},
                {"op":"notify","resources":[{"kind":"queue","agentId":"agent"}]},
                {"op":"rates","body":test_rates(2)},{"op":"consume"},{"op":"consume"},{"op":"consume"}
            ]},
            {"resources":[{"kind":"accounts"}],"steps":[
                {"op":"rates","body":test_rates(1)},
                {"op":"notify","resources":[{"kind":"accounts"}]},
                {"op":"rates","body":test_rates(2)},{"op":"rates","body":test_rates(3)},
                {"op":"consume"},{"op":"consume"},
                {"op":"rates","body":test_rates(3)},{"op":"consume"}
            ]}
        ]);
        let expected: Vec<Vec<Value>> =
            serde_json::from_str(include_str!("fixtures/sync_stream_stream.json"))
                .expect("frozen reference documents");
        assert_eq!(expected.len(), cases.as_array().expect("cases").len());
        for (case, expected) in cases.as_array().expect("cases").iter().zip(expected) {
            let hub = Arc::new(ChangeNotificationHub::new());
            let tokens = Arc::new(TokenRateHub::with_change_hub(hub.clone()));
            let notifications = Arc::new(ResourceNotifications::new(hub.clone()));
            let state = test_stream(
                &hub,
                &tokens,
                notifications.clone(),
                case["resources"].as_array().expect("resources").clone(),
            );
            let mut events = Box::pin(stream::unfold(state, next_frame));
            let mut expected = expected.into_iter();
            for (index, step) in case["steps"].as_array().expect("steps").iter().enumerate() {
                match step["op"].as_str().expect("operation") {
                    "rates" => {
                        tokens.publish(step["body"].clone()).expect("rates");
                    }
                    "notify" => {
                        notifications.publish_once(&json!({"requestId":index.to_string(),"resources":step["resources"]})).expect("notify");
                    }
                    _ => {
                        let expected = expected.next().expect("fixture consumption");
                        if expected.is_null() {
                            assert!(
                                events.next().now_or_never().is_none(),
                                "unexpected frame at step {index}"
                            );
                        } else {
                            let mut actual =
                                event_document(events.next().await.expect("frame")).await;
                            actual["epoch"] = json!("epoch");
                            token_rate_difference::assert_token_rate_documents_equal(
                                &expected,
                                &actual,
                                &[],
                                &format!("consumer step {index}"),
                            );
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn shutdown_closes_a_watch_receiver() {
        let hub = ChangeNotificationHub::new();
        let changes = hub.subscribe();
        let baseline = changes.borrow().clone();
        let state = ResourceStream {
            http_state: None,
            vault_readiness: None,
            subscription: None,
            initial: VecDeque::new(),
            workspace_id: "workspace".to_owned(),
            epoch: baseline.epoch,
            last_revision: baseline.revision,
            resources: Vec::new(),
            entity_sequence: 0,
            transcript_sequences: BTreeMap::new(),
            voice_sequences: BTreeMap::new(),
            draft_sequence: 0,
            store: None,
            changes,
            notifications: Arc::new(ResourceNotifications::new(Arc::new(hub.clone()))),
            token_rates: Some(TokenRateHub::new().subscribe()),
            last_token_rates: None,
            consecutive_token_frames: 0,
            next_token_frame_at: None,
            next_resources_frame_at: None,
            heartbeat: interval_at(
                Instant::now() + Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
                Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
            ),
        };
        let pending = next_frame(state);
        hub.close();
        assert!(pending.await.is_none());
    }

    #[tokio::test]
    async fn state_stream_token_delta_tracks_changes_removals_roots_and_reconnect() {
        let hub = Arc::new(ChangeNotificationHub::new());
        let tokens = TokenRateHub::with_change_hub(hub.clone());
        let notifications = Arc::new(ResourceNotifications::new(hub.clone()));
        let mut state = test_stream(&hub, &tokens, notifications.clone(), vec![]);
        let record = |id: &str, root: &str, rate: f64| {
            json!({
                "agentId":id,"rootId":root,"turnId":"turn","active":true,
                "estimated":false,"rate":rate,"outputTokens":100.0
            })
        };
        let idle = record("idle", "root", 0.0);
        tokens
            .publish(json!({"records":[record("active", "root", 1.0), idle.clone()]}))
            .expect("publish");
        let first = event_document(Ok(token_rate_sse_frame(&mut state).expect("initial"))).await;
        assert_eq!(first["snapshot"], true);
        assert_eq!(first["records"].as_array().expect("records").len(), 2);
        assert_eq!(first["removed"], json!([]));

        // A slow stream skips an intermediate publication, diffing from its own last send.
        tokens
            .publish(json!({"records":[record("active", "root", 2.0), idle]}))
            .expect("publish");
        let moved = record("active", "other-root", 3.0);
        tokens
            .publish(json!({"records":[moved.clone()]}))
            .expect("publish");
        let delta = event_document(Ok(token_rate_sse_frame(&mut state).expect("delta"))).await;
        assert_eq!(delta["snapshot"], false);
        assert_eq!(delta["records"], json!([moved.clone()]));
        assert_eq!(delta["removed"], json!(["idle"]));
        let unchanged =
            event_document(Ok(token_rate_sse_frame(&mut state).expect("unchanged"))).await;
        assert_eq!(unchanged["records"], json!([]));
        assert_eq!(unchanged["removed"], json!([]));

        let mut reconnect = test_stream(&hub, &tokens, notifications, vec![]);
        let full =
            event_document(Ok(token_rate_sse_frame(&mut reconnect).expect("reconnect"))).await;
        assert_eq!(full["snapshot"], true);
        assert_eq!(full["records"], json!([moved]));
        tokens.publish(json!({"records":[]})).expect("remove last");
        let removed = event_document(Ok(token_rate_sse_frame(&mut state).expect("removed"))).await;
        assert_eq!(removed["records"], json!([]));
        assert_eq!(removed["removed"], json!(["active"]));
        // The borrowed frame must remain aligned with the generated OpenAPI DTO.
        for frame in [first, delta, unchanged, full, removed] {
            serde_json::from_value::<crate::generated::ResourceTokenRatesEvent>(frame)
                .expect("generated token-rate frame contract");
        }
    }

    #[tokio::test]
    async fn state_stream_token_delta_bytes_for_fifty_agents_with_two_active() {
        async fn bytes(event: Event) -> Vec<u8> {
            axum::body::to_bytes(
                Sse::new(stream::iter([Ok::<_, Infallible>(event)]))
                    .into_response()
                    .into_body(),
                262144,
            )
            .await
            .expect("frame")
            .to_vec()
        }
        let hub = Arc::new(ChangeNotificationHub::new());
        let tokens = TokenRateHub::with_change_hub(hub.clone());
        let notifications = Arc::new(ResourceNotifications::new(hub.clone()));
        let mut state = test_stream(&hub, &tokens, notifications, vec![]);
        let mut records: Vec<Value> = (0..50).map(|id| json!({
            "agentId":format!("agent-{id:02}"),"rootId":"root","turnId":"turn",
            "active":id < 2,"estimated":false,"rate":if id < 2 {10.0} else {0.0},"outputTokens":100.0
        })).collect();
        tokens
            .publish(json!({"records":records}))
            .expect("initial publication");
        let initial = bytes(token_rate_sse_frame(&mut state).expect("initial")).await;
        for record in &mut records[..2] {
            record["rate"] = json!(20.0);
            record["outputTokens"] = json!(120.0);
        }
        tokens
            .publish(json!({"records":records}))
            .expect("changed publication");
        let rates = tokens.snapshot();
        let cursor = hub.subscribe().borrow().clone();
        // The previous wire protocol sent this complete once-serialized list every time.
        let old = Event::default().event("token-rates").id(cursor.revision.to_string()).data(format!(
            r#"{{"protocol":{},"workspaceId":"workspace","epoch":{},"revision":{},"records":{}}}"#,
            SYNC_PROTOCOL_VERSION, serde_json::to_string(&cursor.epoch).expect("epoch"), cursor.revision, rates.wire().records));
        let old = bytes(old).await;
        let delta = bytes(token_rate_sse_frame(&mut state).expect("delta")).await;
        assert!(delta.len() * 10 < old.len());
        println!(
            "token-rate SSE bytes: v3_full={}, v4_initial={}, v4_delta={}, reduction={:.2}%",
            old.len(),
            initial.len(),
            delta.len(),
            100.0 * (1.0 - delta.len() as f64 / old.len() as f64)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn state_stream_token_delta_http_starts_full_and_reconnects() {
        use axum::body::Body;
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use futures_util::StreamExt;
        use tower::ServiceExt;
        fn document(bytes: &[u8]) -> Value {
            let text = std::str::from_utf8(bytes).expect("UTF8");
            serde_json::from_str(
                text.lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .expect("data"),
            )
            .expect("JSON")
        }
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store),
            None,
        );
        state
            .token_rates
            .publish(floating_test_rates(1))
            .expect("initial rates");
        let app = crate::router(state.clone()).expect("router");
        let request = |reconnect: bool| {
            Request::builder()
            .uri(format!("/api/sync/stream?protocol={SYNC_PROTOCOL_VERSION_TEXT}&resources=%5B%7B%22kind%22%3A%22drafts%22%7D%5D&tokenRates=true&reconnect={reconnect}"))
            .header("host", "127.0.0.1:4321").header("x-canvas-token", "token")
            .extension(ConnectInfo(std::net::SocketAddr::from(([127,0,0,1],12345))))
            .body(Body::empty()).expect("request")
        };
        let response = app.clone().oneshot(request(false)).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let session = document(&body.next().await.expect("session").expect("bytes"));
        assert!(session["sessionId"].is_string());
        let resources = document(&body.next().await.expect("resources").expect("bytes"));
        assert_eq!(resources["reason"], "initial");
        let initial = document(&body.next().await.expect("initial rates").expect("bytes"));
        assert_eq!(initial["snapshot"], true);
        assert_eq!(initial["records"], floating_test_rates(1)["records"]);
        state
            .token_rates
            .publish(floating_test_rates(2))
            .expect("changed rates");
        let delta = document(&body.next().await.expect("delta").expect("bytes"));
        assert_eq!(delta["snapshot"], false);
        assert_eq!(delta["records"], floating_test_rates(2)["records"]);
        drop(body);
        let before_reconnects =
            studio_runtime_services::diagnostics::sync_recovery_counters()["reconnect"]
                .as_u64()
                .expect("counter is numeric");
        let response = app.oneshot(request(true)).await.expect("reconnect");
        let mut body = response.into_body().into_data_stream();
        let session = document(&body.next().await.expect("session").expect("bytes"));
        assert!(session["sessionId"].is_string());
        let resources = document(&body.next().await.expect("resources").expect("bytes"));
        assert_eq!(resources["reason"], "reconnect");
        assert!(
            studio_runtime_services::diagnostics::sync_recovery_counters()["reconnect"]
                .as_u64()
                .expect("counter is numeric")
                > before_reconnects
        );
        let reconnect = document(&body.next().await.expect("reconnect rates").expect("bytes"));
        assert_eq!(reconnect["snapshot"], true);
        assert_eq!(reconnect["records"], floating_test_rates(2)["records"]);
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "sync_stream",
        routes: Some(routes),
        dependencies: &[],
    }
}
