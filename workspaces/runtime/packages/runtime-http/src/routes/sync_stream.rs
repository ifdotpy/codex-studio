//! HTTP adapter for the protocol-3 sync event stream.

use std::collections::{BTreeSet, VecDeque};
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
use studio_runtime_services::change_notifications::{ChangeSnapshot, MAX_SAFE_REVISION};
use studio_runtime_services::progress_watch::{ProgressFileWatch, watch_progress_files};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore, SyncDocumentsRepository};
use tokio::time::{Instant, Interval, interval_at};

use super::extractors::{ApiHeaders, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, get};
use crate::error::ApiError;
use crate::router::HttpState;
use crate::schema::{API_SCHEMA_HASH_HEADER, api_schema_hash};

const MAX_RESOURCE_QUERY_BYTES: usize = 65_536;
const MAX_RESOURCE_COUNT: usize = 256;
const RESOURCE_HEARTBEAT_SECONDS: u64 = 15;
static STREAM_DEPENDENCIES: &[Dependency] = &[
    Dependency::new("provider-session/app-server"),
    Dependency::new("usage-policy/token-rates"),
];
#[cfg(windows)]
static WINDOWS_STREAM_DEPENDENCIES: &[Dependency] = &[
    Dependency::new("provider-session/app-server"),
    Dependency::new("resource-policy/windows-file-identity"),
    Dependency::new("usage-policy/token-rates"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![get("/api/sync/stream", stream_status(), sync_stream)])
}

#[cfg(windows)]
fn stream_status() -> OperationStatus {
    OperationStatus::partial(WINDOWS_STREAM_DEPENDENCIES)
}

#[cfg(not(windows))]
fn stream_status() -> OperationStatus {
    OperationStatus::partial(STREAM_DEPENDENCIES)
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
    let change_hub = Arc::clone(&state.sync_changes);
    let mut changes = change_hub.subscribe();
    let mut token_rates = state.token_rates.subscribe();
    let subscription_snapshot = changes.borrow_and_update().clone();
    let progress_agents = match active_panel_agents(Arc::clone(&store), &resources).await {
        Ok(agents) => agents,
        Err(error) => return error.into_response(),
    };
    let progress_watch =
        match watch_progress_files(store.root(), &progress_agents, (*change_hub).clone()) {
            Ok(watch) => watch,
            Err(_) => {
                return ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Resource stream is unavailable; reconnect to retry.",
                )
                .into_response();
            }
        };

    let initial_entity_sequence = match read_entity_sequence(Some(store.clone()), &resources).await
    {
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
    let initial_token_rates = token_rates.borrow_and_update().clone();
    initial.push_back(token_rates_event(
        &state.workspace_id,
        &subscription_snapshot.epoch,
        subscription_snapshot.revision,
        &initial_token_rates,
    ));

    let stream_state = ResourceStream {
        initial,
        workspace_id: state.workspace_id,
        epoch: subscription_snapshot.epoch,
        last_revision: subscription_snapshot.revision,
        resources,
        entity_sequence: initial_entity_sequence,
        store: Some(store),
        changes,
        token_rates,
        _progress_watch: progress_watch,
        heartbeat: interval_at(
            Instant::now() + Duration::from_secs(RESOURCE_HEARTBEAT_SECONDS),
            Duration::from_secs(RESOURCE_HEARTBEAT_SECONDS),
        ),
    };
    event_response(stream::unfold(stream_state, next_frame))
}

async fn active_panel_agents(
    store: Arc<RuntimeStore>,
    resources: &[Value],
) -> Result<Vec<String>, ApiError> {
    let agents = resources
        .iter()
        .filter(|resource| resource.get("kind").and_then(Value::as_str) == Some("panel"))
        .filter_map(|resource| resource.get("agentId").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if agents.is_empty() {
        return Ok(Vec::new());
    }
    tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let mut active = Vec::new();
        for id in agents {
            let agent = AgentRepository::get(&connection, &AgentId::from(id.as_str()))
                .map_err(|_| ApiError::internal())?;
            if agent.is_some_and(|agent| !agent.extra.get("deletedAt").is_some_and(json_truthy)) {
                active.push(id);
            }
        }
        Ok(active)
    })
    .await
    .map_err(|_| ApiError::internal())?
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
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
    store: Option<Arc<RuntimeStore>>,
    resources: &[Value],
) -> Result<i64, ApiError> {
    if !resources
        .iter()
        .any(|resource| resource.get("kind").and_then(Value::as_str) == Some("state"))
    {
        return Ok(0);
    }
    let store = store.ok_or_else(ApiError::internal)?;
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
    store: Option<Arc<RuntimeStore>>,
    changes: tokio::sync::watch::Receiver<ChangeSnapshot>,
    token_rates:
        tokio::sync::watch::Receiver<studio_runtime_services::token_rate::TokenRateSnapshot>,
    _progress_watch: Option<ProgressFileWatch>,
    heartbeat: Interval,
}

async fn next_frame(
    mut state: ResourceStream,
) -> Option<(Result<Event, Infallible>, ResourceStream)> {
    if let Some(event) = state.initial.pop_front() {
        return Some((Ok(event), state));
    }

    loop {
        enum Wake {
            Changes(Result<(), tokio::sync::watch::error::RecvError>),
            TokenRates(Result<(), tokio::sync::watch::error::RecvError>),
            Heartbeat,
        }
        let wake = tokio::select! {
            biased;
            changed = state.changes.changed() => Wake::Changes(changed),
            changed = state.token_rates.changed() => Wake::TokenRates(changed),
            _ = state.heartbeat.tick() => Wake::Heartbeat,
        };
        let snapshot = match wake {
            Wake::Changes(Err(_)) | Wake::TokenRates(Err(_)) => return None,
            Wake::TokenRates(Ok(())) => {
                let rates = state.token_rates.borrow_and_update().clone();
                return Some((
                    Ok(token_rates_event(
                        &state.workspace_id,
                        &state.epoch,
                        state.last_revision,
                        &rates,
                    )),
                    state,
                ));
            }
            Wake::Changes(Ok(())) => state.changes.borrow_and_update().clone(),
            Wake::Heartbeat => state.changes.borrow().clone(),
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

fn token_rates_event(
    workspace_id: &str,
    epoch: &str,
    revision: i64,
    rates: &studio_runtime_services::token_rate::TokenRateSnapshot,
) -> Event {
    let rates = rates.to_value();
    sse_event(
        "token-rates",
        Some(revision),
        json!({
            "protocol": 3,
            "workspaceId": workspace_id,
            "epoch": epoch,
            "revision": revision,
            "rates": rates["rates"],
            "teams": rates["teams"],
        }),
    )
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
    use studio_runtime_services::change_notifications::ChangeNotificationHub;

    #[tokio::test]
    async fn overflow_is_recovered_by_next_frame_with_the_latest_full_resource_event() {
        let hub = ChangeNotificationHub::new();
        let mut changes = hub.subscribe();
        let baseline = changes.borrow_and_update().clone();
        let state = ResourceStream {
            initial: VecDeque::new(),
            workspace_id: "workspace".to_owned(),
            epoch: baseline.epoch,
            last_revision: baseline.revision,
            resources: vec![json!({"kind": "panel", "agentId": "agent-a"})],
            entity_sequence: 0,
            store: None,
            changes,
            token_rates: studio_runtime_services::token_rate::TokenRateHub::new().subscribe(),
            _progress_watch: None,
            heartbeat: interval_at(
                Instant::now() + Duration::from_secs(RESOURCE_HEARTBEAT_SECONDS),
                Duration::from_secs(RESOURCE_HEARTBEAT_SECONDS),
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
        assert!(frame.contains("\"reason\":\"overflow\""), "{frame}");
        assert!(frame.contains("\"revision\":3"), "{frame}");
    }

    #[tokio::test]
    async fn shutdown_closes_a_watch_receiver() {
        let hub = ChangeNotificationHub::new();
        let changes = hub.subscribe();
        let baseline = changes.borrow().clone();
        let state = ResourceStream {
            initial: VecDeque::new(),
            workspace_id: "workspace".to_owned(),
            epoch: baseline.epoch,
            last_revision: baseline.revision,
            resources: Vec::new(),
            entity_sequence: 0,
            store: None,
            changes,
            _progress_watch: None,
            heartbeat: interval_at(
                Instant::now() + Duration::from_secs(RESOURCE_HEARTBEAT_SECONDS),
                Duration::from_secs(RESOURCE_HEARTBEAT_SECONDS),
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
        dependencies: &[
        "provider-session/app-server",
        "resource-policy/windows-file-identity",
        "usage-policy/token-rates",
        ],
    }
}
