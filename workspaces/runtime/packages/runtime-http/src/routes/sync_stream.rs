//! HTTP adapter for the protocol-3 sync event stream.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use futures_util::stream;
use serde_json::{Value, json};
use studio_runtime_services::change_notifications::{
    ChangeNotificationHub, ChangeSubscription, MAX_SAFE_REVISION,
};
use studio_runtime_store::{RuntimeStore, SyncDocumentsRepository};
use tokio::sync::Notify;
use tokio::time::{Instant, Interval, interval_at};

use super::extractors::{ApiHeaders, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get};
use crate::error::ApiError;
use crate::router::HttpState;
use crate::schema::{API_SCHEMA_HASH_HEADER, api_schema_hash};

const MAX_RESOURCE_QUERY_BYTES: usize = 65_536;
const MAX_RESOURCE_COUNT: usize = 256;
const RESOURCE_HEARTBEAT_SECONDS: u64 = 15;

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
    if protocol.is_some_and(|value| value != "3")
        || header_protocol.is_some_and(|value| value != "3")
        || (protocol != Some("3") && header_protocol != Some("3"))
    {
        return (
            StatusCode::UPGRADE_REQUIRED,
            Json(json!({
                "error": "Unsupported sync protocol version",
                "supportedVersions": [3],
            })),
        )
            .into_response();
    }

    let server_hash = api_schema_hash();
    let renderer_hash = request_headers
        .get(API_SCHEMA_HASH_HEADER)
        .and_then(|value| value.to_str().ok())
        .or_else(|| query.typed_value("apiSchema").and_then(Value::as_str));
    if renderer_hash.is_some_and(|value| value != server_hash) {
        return event_response(stream::iter([Ok(sse_event(
            "api-schema",
            None,
            json!({"hash": server_hash, "mismatch": true}),
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
    let hub = Arc::clone(&state.event_wake);
    let wake = Arc::new(Notify::new());
    let wake_for_hub = Arc::clone(&wake);
    let (subscription_snapshot, subscription) = hub.subscribe(move || {
        // Notify stores at most one permit: slow clients coalesce into a later
        // full invalidation frame instead of accumulating an unbounded queue.
        wake_for_hub.notify_one();
    });

    let initial_entity_sequence = match read_entity_sequence(store.clone(), &resources).await {
        Ok(sequence) => sequence,
        Err(error) => return error.into_response(),
    };
    let reason = if headers.contains_key("last-event-id") {
        "reconnect"
    } else {
        "initial"
    };
    let initial_event = resource_event(
        &state.workspace_id,
        &subscription_snapshot.epoch,
        subscription_snapshot.revision,
        reason,
        &resources,
        initial_entity_sequence,
    );
    let mut initial = VecDeque::new();
    if renderer_hash.is_some() {
        initial.push_back(sse_event("api-schema", None, json!({"hash": server_hash})));
    }
    initial.push_back(sse_event(
        "resources",
        Some(subscription_snapshot.revision),
        initial_event,
    ));

    let stream_state = ResourceStream {
        initial,
        workspace_id: state.workspace_id,
        epoch: subscription_snapshot.epoch,
        last_revision: subscription_snapshot.revision,
        resources,
        entity_sequence: initial_entity_sequence,
        store,
        hub,
        wake,
        _subscription: subscription,
        heartbeat: interval_at(
            Instant::now() + Duration::from_secs(RESOURCE_HEARTBEAT_SECONDS),
            Duration::from_secs(RESOURCE_HEARTBEAT_SECONDS),
        ),
    };
    event_response(stream::unfold(stream_state, next_frame))
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
    if raw.len() > MAX_RESOURCE_QUERY_BYTES {
        return Err(invalid());
    }
    let parsed: Value = serde_json::from_str(raw).map_err(|_| invalid())?;
    let values = parsed.as_array().ok_or_else(invalid)?;
    if values.is_empty()
        || values.len() > MAX_RESOURCE_COUNT
        || values
            .iter()
            .any(|resource| !state.validators.validate_component("ResourceRef", resource))
    {
        return Err(invalid());
    }
    Ok(values.clone())
}

async fn read_entity_sequence(
    store: Arc<RuntimeStore>,
    resources: &[Value],
) -> Result<i64, ApiError> {
    if !resources
        .iter()
        .any(|resource| resource.get("kind").and_then(Value::as_str) == Some("state"))
    {
        return Ok(0);
    }
    let sequence = tokio::task::spawn_blocking(move || {
        let connection = store.read()?;
        SyncDocumentsRepository::entity_sequence(&connection)
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|_| ApiError::internal())?;
    if !(0..=MAX_SAFE_REVISION).contains(&sequence) {
        return Err(ApiError::internal());
    }
    Ok(sequence)
}

struct ResourceStream {
    initial: VecDeque<Event>,
    workspace_id: String,
    epoch: String,
    last_revision: i64,
    resources: Vec<Value>,
    entity_sequence: i64,
    store: Arc<RuntimeStore>,
    hub: Arc<ChangeNotificationHub>,
    wake: Arc<Notify>,
    _subscription: ChangeSubscription,
    heartbeat: Interval,
}

async fn next_frame(
    mut state: ResourceStream,
) -> Option<(Result<Event, Infallible>, ResourceStream)> {
    if let Some(event) = state.initial.pop_front() {
        return Some((Ok(event), state));
    }

    loop {
        let snapshot = tokio::select! {
            biased;
            _ = state.wake.notified() => state.hub.snapshot(),
            _ = state.heartbeat.tick() => state.hub.snapshot(),
        };
        if snapshot.closed {
            return None;
        }
        if snapshot.revision > state.last_revision {
            let entity_sequence =
                match read_entity_sequence(state.store.clone(), &state.resources).await {
                    Ok(sequence) => sequence,
                    // Once headers are sent, closing the stream is the visible
                    // fail-closed behavior; the client will reconnect and retry.
                    Err(_) => return None,
                };
            let reason = if snapshot.revision.saturating_sub(state.last_revision) > 1 {
                "overflow"
            } else {
                "change"
            };
            state.last_revision = snapshot.revision;
            state.entity_sequence = entity_sequence;
            let event = resource_event(
                &state.workspace_id,
                &state.epoch,
                snapshot.revision,
                reason,
                &state.resources,
                state.entity_sequence,
            );
            return Some((
                Ok(sse_event("resources", Some(snapshot.revision), event)),
                state,
            ));
        }
        if snapshot.revision == state.last_revision {
            let event = json!({
                "protocol": 3,
                "workspaceId": state.workspace_id,
                "epoch": state.epoch,
                "revision": snapshot.revision,
            });
            return Some((
                Ok(sse_event("heartbeat", Some(snapshot.revision), event)),
                state,
            ));
        }
    }
}

fn resource_event(
    workspace_id: &str,
    epoch: &str,
    revision: i64,
    reason: &str,
    resources: &[Value],
    entity_sequence: i64,
) -> Value {
    let resource_versions = resources
        .iter()
        .map(|resource| {
            let version = if resource.get("kind").and_then(Value::as_str) == Some("state") {
                entity_sequence
            } else {
                revision
            };
            json!({"revision": version})
        })
        .collect::<Vec<_>>();
    json!({
        "protocol": 3,
        "workspaceId": workspace_id,
        "epoch": epoch,
        "revision": revision,
        "reason": reason,
        "resources": resources,
        "resourceVersions": resource_versions,
    })
}

fn sse_event(name: &str, revision: Option<i64>, payload: Value) -> Event {
    let event = Event::default().event(name).data(payload.to_string());
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
    use futures_util::FutureExt;

    #[tokio::test]
    async fn slow_client_wake_coalesces_and_reports_the_latest_revision() {
        let hub = ChangeNotificationHub::new();
        let wake = Arc::new(Notify::new());
        let wake_for_hub = Arc::clone(&wake);
        let (_, _subscription) = hub.subscribe(move || wake_for_hub.notify_one());

        hub.notify_one();
        hub.notify_one();
        wake.notified().await;

        assert!(wake.notified().now_or_never().is_none());
        assert_eq!(hub.snapshot().revision, 2);
    }

    #[tokio::test]
    async fn shutdown_wakes_client_and_marks_stream_closed() {
        let hub = ChangeNotificationHub::new();
        let wake = Arc::new(Notify::new());
        let wake_for_hub = Arc::clone(&wake);
        let (_, _subscription) = hub.subscribe(move || wake_for_hub.notify_one());
        let pending = wake.notified();

        hub.close();
        pending.await;

        assert!(hub.snapshot().closed);
    }
}
