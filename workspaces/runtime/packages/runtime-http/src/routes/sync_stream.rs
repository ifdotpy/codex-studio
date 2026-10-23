//! HTTP adapter for the protocol-3 sync event stream.

use std::collections::{BTreeSet, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use crate::generated::{
    DraftChangeBatch, EntityChangeBatch, ResourceChangeEvent, ResourceChangeEventReason,
    ResourceRef, ResourceRevisionEntry,
};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use futures_util::stream;
use serde::Serialize;
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::{
    MAX_SYNC_RESOURCE_COUNT, MAX_SYNC_RESOURCE_QUERY_BYTES, SYNC_DRAFT_STREAM_MAX_BYTES,
    SYNC_DRAFT_STREAM_MAX_DOCUMENTS, SYNC_ENTITY_STREAM_MAX_BYTES,
    SYNC_ENTITY_STREAM_MAX_DOCUMENTS, SYNC_RESOURCE_HEARTBEAT_SECONDS,
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

use super::extractors::{ApiHeaders, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get};
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

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![get(
        "/api/sync/stream",
        OperationStatus::Implemented,
        sync_stream,
    )])
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
        return event_response(stream::iter([Ok(sse_event(
            "api-schema",
            None,
            json!({"hash": server_hash, (crate::schema::API_SCHEMA_MISMATCH_FIELD): true}),
        ))]));
    }

    let resources = match parse_resources(&query, &state) {
        Ok(resources) => resources,
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
    let reason = if headers.contains_key("last-event-id") {
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
    let subscription_snapshot = view.cursor.clone();
    let initial_event = match resource_changes_event(
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
    let mut initial = VecDeque::new();
    if renderer_hash.is_some() {
        initial.push_back(sse_event("api-schema", None, json!({"hash": server_hash})));
    }
    initial.push_back(sse_event(
        "resources",
        Some(subscription_snapshot.revision),
        initial_event,
    ));
    let mut token_rates = state.token_rates.subscribe();
    let (rates, cursor) = token_snapshot(&mut token_rates, || changes.borrow().clone());
    initial.push_back(token_rate_event(&state.workspace_id, &cursor, &rates));

    let stream_state = ResourceStream {
        http_state: Some(state.clone()),
        initial,
        workspace_id: state.workspace_id,
        epoch: subscription_snapshot.epoch,
        last_revision: subscription_snapshot.revision,
        resources,
        entity_sequence: initial_entity_sequence,
        draft_sequence: initial_draft_sequence,
        store: Some(store),
        changes,
        notifications: state.resource_notifications,
        token_rates,
        consecutive_token_frames: 0,
        next_token_frame_at: None,
        next_resources_frame_at: None,
        heartbeat: interval_at(
            Instant::now() + Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
            Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
        ),
    };
    event_response(stream::unfold(stream_state, next_frame))
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
        ResourceRef::AccountsResource(_) => {
            serde_json::to_vec(&super::catalog::accounts(State(state)).await.ok()?.0).ok()
        }
        ResourceRef::LimitsResource(resource) => {
            let snapshot = if let Some(port) = &state.account_session {
                port.limits_snapshot(&resource.account_key).ok()?
            } else {
                state.account_policy.as_ref()?.limits(&resource.account_key)
            };
            let snapshot: crate::generated::UsageLimitsResponse =
                serde_json::from_value(snapshot).ok()?;
            serde_json::to_vec(&snapshot).ok()
        }
        ResourceRef::DesktopResource(_) => {
            serde_json::to_vec(&super::desktop::get(State(state)).await.ok()?.0).ok()
        }
        ResourceRef::TerminalsResource(_) => {
            serde_json::to_vec(&super::terminals::listing(State(state)).await.ok()?.0).ok()
        }
        ResourceRef::QueueResource(resource) => serde_json::to_vec(
            &super::work_items::queue_for_agent(state, resource.agent_id)
                .await
                .ok()?,
        )
        .ok(),
        ResourceRef::SessionCostResource(resource) => serde_json::to_vec(
            &super::insights::session_cost_for_agent(state, resource.agent_id)
                .await
                .ok()?
                .0,
        )
        .ok(),
        ResourceRef::ModelsResource(_) => {
            let catalog = state.model_catalog.cached_resource_content()?;
            let accounts = super::catalog::accounts(State(state)).await.ok()?.0;
            // Only account fields consumed by account_catalog/worker_catalog
            // affect model reads; labels and login receipts do not.
            let contexts = accounts
                .accounts
                .iter()
                .map(|account| {
                    (
                        &account.id,
                        &account.home,
                        &account.provider,
                        &account.status,
                        &account.error,
                        &account.deleted,
                        &account.disconnected,
                        &account.duplicate_of,
                        &account.can_attempt_native_proof,
                    )
                })
                .collect::<Vec<_>>();
            serde_json::to_vec(&(accounts.default_account_key, contexts, catalog)).ok()
        }
        ResourceRef::CostsResource(_) => {
            let policy = state.account_policy.clone()?;
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/resource-cost-content",
                move || {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_secs_f64();
                    let mut costs = policy
                        .accounts()
                        .accounts
                        .into_iter()
                        .map(|account| {
                            let account: crate::generated::Account =
                                serde_json::from_value(account).ok()?;
                            let value = studio_runtime_services::usage::account_cost_snapshot(
                                std::path::Path::new(&state.state_dir),
                                &account.id,
                                &serde_json::to_value(&account).ok()?,
                                now,
                            )
                            .ok()?;
                            // The generated response drops the scanner-only refresh hint.
                            serde_json::from_value::<crate::generated::AccountCostResponse>(value)
                                .ok()
                        })
                        .collect::<Option<Vec<_>>>()?;
                    costs.sort_by(|left, right| left.account_key.cmp(&right.account_key));
                    serde_json::to_vec(&costs).ok()
                },
            )
            .await
            .ok()
            .flatten()
        }
        _ => None,
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
    if values.is_empty()
        || values.len() > MAX_SYNC_RESOURCE_COUNT
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

/// Keep conversion and size checks outside the asynchronous SQLite read closure.
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
    let bytes = serde_json::to_vec(&batch).ok()?;
    if bytes.len() > SYNC_ENTITY_STREAM_MAX_BYTES {
        return None;
    }
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
    if serde_json::to_vec(&batch).ok()?.len() > SYNC_DRAFT_STREAM_MAX_BYTES {
        return None;
    }
    Some(batch)
}

struct ResourceStream {
    http_state: Option<HttpState>,
    initial: VecDeque<Event>,
    workspace_id: String,
    epoch: String,
    last_revision: i64,
    resources: Vec<Value>,
    entity_sequence: i64,
    draft_sequence: i64,
    store: Option<Arc<RuntimeStore>>,
    changes: tokio::sync::watch::Receiver<ChangeSnapshot>,
    notifications: Arc<ResourceNotifications>,
    token_rates: tokio::sync::watch::Receiver<Arc<TokenRates>>,
    consecutive_token_frames: usize,
    /// Earliest instant of the next token-rate frame, from `SYNC_FRAME_MIN_INTERVAL`.
    next_token_frame_at: Option<Instant>,
    /// Earliest instant of the next resources frame, from `SYNC_FRAME_MIN_INTERVAL`.
    next_resources_frame_at: Option<Instant>,
    heartbeat: Interval,
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
                changed = changed_not_before(&mut state.changes, state.next_resources_frame_at) => {
                    if changed.is_err() { return None; }
                    state.changes.borrow_and_update();
                    false
                },
                changed = changed_not_before(&mut state.token_rates, state.next_token_frame_at) => {
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
                changed = changed_not_before(&mut state.token_rates, state.next_token_frame_at) => {
                    if changed.is_err() { return None; }
                    state.consecutive_token_frames = state.consecutive_token_frames.saturating_add(1);
                    let frame = token_rate_sse_frame(&mut state)?;
                    state.next_token_frame_at = Some(Instant::now() + SYNC_FRAME_MIN_INTERVAL);
                    return Some((Ok(frame), state));
                },
                changed = changed_not_before(&mut state.changes, state.next_resources_frame_at) => {
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
                ResourceChangeEventReason::Overflow
            } else {
                ResourceChangeEventReason::Change
            };
            let event = resource_changes_event(
                &state.workspace_id,
                &view,
                reason,
                sequence,
                batch,
                draft_sequence,
                draft_changes,
            )
            .ok()?;
            state.next_resources_frame_at = Some(Instant::now() + SYNC_FRAME_MIN_INTERVAL);
            return Some((
                Ok(sse_event("resources", Some(view.cursor.revision), event)),
                state,
            ));
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
    receiver: &mut tokio::sync::watch::Receiver<T>,
    not_before: Option<Instant>,
) -> Result<(), tokio::sync::watch::error::RecvError> {
    if let Some(not_before) = not_before {
        tokio::time::sleep_until(not_before).await;
    }
    receiver.changed().await
}

fn token_rate_sse_frame(state: &mut ResourceStream) -> Option<Event> {
    let (rates, cursor) = token_snapshot(&mut state.token_rates, || state.changes.borrow().clone());
    if cursor.closed {
        return None;
    }
    Some(token_rate_event(&state.workspace_id, &cursor, &rates))
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
    changes: Option<EntityChangeBatch>,
    draft_sequence: i64,
    draft_changes: Option<DraftChangeBatch>,
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
                revision: if state {
                    sequence
                } else if drafts {
                    draft_sequence
                } else {
                    *revision
                },
                draft_changes: drafts.then(|| draft_changes.clone()).flatten().map(Some),
                entity_changes: state.then(|| changes.clone()).flatten().map(Some),
                entity_sequence_reset: state.then_some(Some(view.entity_reset)),
                entity_sequences: None,
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

/// The token-rates frame. The `rates` and `teams` members are the snapshot's shared,
/// once-serialized text; only the small cursor members are written per subscriber.
fn token_rate_event(workspace_id: &str, snapshot: &ChangeSnapshot, rates: &TokenRates) -> Event {
    let wire = rates.wire();
    let text = |value: &str| serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned());
    let data = format!(
        r#"{{"protocol":{},"workspaceId":{},"epoch":{},"revision":{},"rates":{},"teams":{}}}"#,
        SYNC_PROTOCOL_VERSION,
        text(workspace_id),
        text(&snapshot.epoch),
        snapshot.revision,
        wire.rates,
        wire.teams,
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
    async fn state_stream_draft_batches_keep_limits_and_recovery() {
        for scenario in ["maximum", "documents", "bytes", "payload"] {
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
                        json!({"device":"phone", "session":session, "text": if scenario == "bytes" { "é".repeat(SYNC_DRAFT_STREAM_MAX_BYTES) } else { "draft".into() }, "updated":1}).to_string()
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
        assert_eq!(batch["through"], baseline + 2);
        assert_eq!(
            batch["documents"].as_array().expect("full records").len(),
            2
        );
        assert_eq!(batch["documents"][1]["_deleted"], true);
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
    }

    #[tokio::test]
    async fn cost_content_ignores_account_listing_order() {
        use studio_runtime_services::account_policy::AccountPolicyState;
        fn policy(order: [&str; 2], root: &std::path::Path) -> AccountPolicyState {
            let entries = order
                .map(|id| {
                    format!(
                        "\"{id}\":{}",
                        json!({
                            "id":id, "home":root, "provider":"codex", "label":id,
                            "source":"Fixture", "status":"ready"
                        })
                    )
                })
                .join(",");
            let registry = format!(
                r#"{{"version":1,"defaultAccountKey":"first","accounts":{{{entries}}},"logins":{{}}}}"#
            );
            AccountPolicyState::from_startup(Some(registry.as_bytes()), []).expect("policy")
        }
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let mut state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store),
            Some(Arc::new(policy(["first", "second"], root.path()))),
        );
        let before = resource_content(state.clone(), json!({"kind":"costs"}))
            .await
            .expect("cost content");
        state.account_policy = Some(Arc::new(policy(["second", "first"], root.path())));
        let after = resource_content(state, json!({"kind":"costs"}))
            .await
            .expect("cost content");
        assert_eq!(
            before, after,
            "account order does not change either account's served costs"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unrelated_commits_do_not_reread_five_unchanged_resources() {
        use axum::body::Body;
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use futures_util::StreamExt;
        use studio_runtime_services::account_policy::AccountPolicyState;
        use tower::ServiceExt;

        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('agent-1',?1)",
                    [
                        json!({"id":"agent-1","rootId":"agent-1","epoch":1,"status":"idle"})
                            .to_string(),
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("agent");
        let policy = AccountPolicyState::from_startup(
            Some(br#"{"version":1,"defaultAccountKey":"default","accounts":{},"logins":{}}"#),
            [],
        )
        .expect("account policy");
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path()
                .join("unavailable-desktop-volume")
                .display()
                .to_string(),
            4321,
            None,
            Some(store.clone()),
            Some(Arc::new(policy)),
        );
        let resources = json!([
            {"kind":"accounts"}, {"kind":"desktop"}, {"kind":"terminals"},
            {"kind":"queue","agentId":"agent-1"}, {"kind":"models"}
        ]);
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("protocol", SYNC_PROTOCOL_VERSION_TEXT)
            .append_pair("resources", &resources.to_string())
            .finish();
        let request = Request::builder()
            .uri(format!("/api/sync/stream?{query}"))
            .header("host", "127.0.0.1:4321")
            .header("x-canvas-token", "token")
            .extension(ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                12345,
            ))))
            .body(Body::empty())
            .expect("request");
        let response = crate::router(state.clone())
            .expect("router")
            .oneshot(request)
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        assert!(
            String::from_utf8_lossy(&body.next().await.expect("initial").expect("bytes"))
                .contains("event: resources")
        );
        assert!(
            String::from_utf8_lossy(&body.next().await.expect("rates").expect("bytes"))
                .contains("event: token-rates")
        );
        for index in 0..42 {
            store.transaction(move |tx| {
                tx.execute("INSERT INTO runtime_items(id,agent,created,record) VALUES(?1,'other',1,'{}')", [format!("item-{index}")])?;
                Ok(())
            }).await.expect("unrelated commit");
        }
        let frame = body.next().await.expect("heartbeat").expect("bytes");
        assert!(
            String::from_utf8_lossy(&frame).contains("event: heartbeat"),
            "42 unrelated commits must cause zero resource re-reads: {}",
            String::from_utf8_lossy(&frame)
        );
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.queueMutationRevision',1) WHERE id='agent-1'", [])?;
            Ok(())
        }).await.expect("queue content changes");
        let frame = body.next().await.expect("queue refresh").expect("bytes");
        let text = String::from_utf8_lossy(&frame);
        let document: Value = serde_json::from_str(
            text.lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("data"),
        )
        .expect("JSON");
        assert_eq!(
            document["resources"],
            json!([{"kind":"queue","agentId":"agent-1"}])
        );
    }

    #[tokio::test]
    async fn stream_documents_match_frozen_resource_hub() {
        let rate = json!({"turnId":"turn", "active":false,"estimated":true,"rate":1.25,"outputTokens":400});
        let cases = json!([
            {"resources":[{"kind":"accounts"},{"kind":"session-cost","agentId":"agent"}],"sequence":0,"reconnect":false,"rates":{"rates":{},"teams":{}}},
            {"resources":[{"kind":"state"}],"sequence":42,"reconnect":true,"rates":{"rates":{"agent":rate},"teams":{"lead":{"agent":rate}}}}
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
            initial: VecDeque::new(),
            workspace_id: "workspace".to_owned(),
            epoch: baseline.epoch,
            last_revision: baseline.revision,
            resources: vec![json!({"kind": "queue", "agentId": "agent-a"})],
            entity_sequence: 0,
            draft_sequence: 0,
            store: None,
            changes,
            notifications: Arc::new(ResourceNotifications::new(Arc::new(hub.clone()))),
            token_rates: tokens.subscribe(),
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
            initial: VecDeque::new(),
            workspace_id: "workspace".to_owned(),
            epoch: baseline.epoch,
            last_revision: baseline.revision,
            resources: vec![json!({"kind":"accounts"})],
            entity_sequence: 0,
            draft_sequence: 0,
            store: None,
            changes,
            notifications: notifications.clone(),
            token_rates: token_hub.subscribe(),
            consecutive_token_frames: 0,
            next_token_frame_at: None,
            next_resources_frame_at: None,
            heartbeat: interval_at(
                Instant::now() + Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
                Duration::from_secs(SYNC_RESOURCE_HEARTBEAT_SECONDS),
            ),
        };
        for index in 0..=MAX_SYNC_RESOURCE_COUNT {
            token_hub.publish(json!({
                "rates": {"agent": {"turnId":"turn", "active":true, "estimated":false, "rate":index, "outputTokens":index}}, "teams": {}
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
        assert!(frame.contains("id: 257"), "{frame}");
        assert!(frame.contains("\"rate\":256"), "{frame}");
        drop(state); // Disconnect drops the only receiver; later publications remain bounded.
        assert!(
            token_hub
                .publish(json!({"rates":{},"teams":{}}))
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
        assert_eq!(token["rates"]["agent"]["rate"].as_f64(), Some(1.0));

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
        assert_eq!(token["rates"]["agent"]["rate"].as_f64(), Some(8.0));
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
        json!({"rates":{"agent":{"turnId":"turn","active":true,"estimated":false,"rate":rate,"outputTokens":rate}},"teams":{}})
    }

    fn floating_test_rates(rate: u64) -> Value {
        json!({"rates":{"agent":{"turnId":"turn","active":true,"estimated":false,"rate":rate as f64,"outputTokens":rate as f64}},"teams":{}})
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
            initial: VecDeque::new(),
            workspace_id: "workspace".into(),
            epoch: baseline.epoch,
            last_revision: baseline.revision,
            resources,
            entity_sequence: 0,
            draft_sequence: 0,
            store: None,
            changes,
            notifications,
            token_rates: tokens.subscribe(),
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
            assert_eq!(document["rates"], floating_test_rates(rate)["rates"]);
            assert_eq!(document["revision"], rate);
            assert!(document.get("resources").is_none());
        }
        let latest = event_document(second.next().await.expect("slow reader latest")).await;
        assert_eq!(latest["rates"], floating_test_rates(4)["rates"]);
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
            initial: VecDeque::new(),
            workspace_id: "workspace".to_owned(),
            epoch: baseline.epoch,
            last_revision: baseline.revision,
            resources: Vec::new(),
            entity_sequence: 0,
            draft_sequence: 0,
            store: None,
            changes,
            notifications: Arc::new(ResourceNotifications::new(Arc::new(hub.clone()))),
            token_rates: TokenRateHub::new().subscribe(),
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
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "sync_stream",
        routes: Some(routes),
        dependencies: &[],
    }
}
