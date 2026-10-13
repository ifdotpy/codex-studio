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
use studio_runtime_services::resource_notify::{ResourceNotifications, ResourceNotifyError};
#[cfg(test)]
use studio_runtime_services::token_rate::TokenRateHub;
use studio_runtime_services::token_rate::TokenRateSnapshot as TokenRates;
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore, SyncDocumentsRepository};
use tokio::time::{Instant, Interval, interval_at};

use super::extractors::{ApiHeaders, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, get};
use crate::error::ApiError;
use crate::router::HttpState;
use crate::schema::{API_SCHEMA_HASH_HEADER, api_schema_hash};

/// P18's catalog route calls this after the shared boundary authenticates and validates.
pub(crate) async fn notify_resources(
    state: HttpState,
    body: Value,
) -> Result<Json<Value>, ApiError> {
    let notifications = state.resource_notifications;
    let request_id = body["requestId"].clone();
    tokio::task::spawn_blocking(move || notifications.publish_once(&body))
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
        match state.resource_notifications.entity_reset_after(0) {
            Ok(reset) => reset,
            Err(_) => return ApiError::internal().into_response(),
        },
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
    let mut token_rates = state.token_rates.subscribe();
    let rates = token_rates.borrow_and_update().clone();
    initial.push_back(token_rate_event(
        &state.workspace_id,
        &subscription_snapshot,
        &rates,
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
        notifications: state.resource_notifications,
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
    notifications: Arc<ResourceNotifications>,
    token_rates: tokio::sync::watch::Receiver<TokenRates>,
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
        let snapshot = tokio::select! {
            biased;
            changed = state.changes.changed() => {
                if changed.is_err() {
                    return None;
                }
                state.changes.borrow_and_update().clone()
            },
            changed = state.token_rates.changed() => {
                if changed.is_err() { return None; }
                let rates = state.token_rates.borrow_and_update().clone();
                let snapshot = state.changes.borrow().clone();
                if snapshot.closed { return None; }
                return Some((Ok(token_rate_event(&state.workspace_id, &snapshot, &rates)), state));
            },
            _ = state.heartbeat.tick() => state.changes.borrow().clone(),
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
            let entity_reset = match state.notifications.entity_reset_after(state.last_revision) {
                Ok(reset) => reset,
                Err(_) => return None,
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
                entity_reset,
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

fn token_rate_event(workspace_id: &str, snapshot: &ChangeSnapshot, rates: &TokenRates) -> Event {
    let rates = rates.to_value();
    sse_event(
        "token-rates",
        Some(snapshot.revision),
        json!({
            "protocol": 3, "workspaceId": workspace_id,
            "epoch": snapshot.epoch, "revision": snapshot.revision,
            "rates": rates["rates"], "teams": rates["teams"],
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
    entity_reset: bool,
) -> Value {
    let resource_versions = resources
        .iter()
        .map(|resource| {
            let version = if resource.get("kind").and_then(Value::as_str) == Some("state") {
                entity_sequence
            } else {
                revision
            };
            let mut value = json!({"revision": version});
            if resource.get("kind").and_then(Value::as_str) == Some("state") {
                value["entitySequenceReset"] = json!(entity_reset);
            }
            value
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

    mod python {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../runtime-services/tests/support/prepared_python.rs"
        ));
    }

    #[tokio::test]
    async fn stream_documents_match_python_resource_hub() {
        use std::io::Write;
        use std::path::Path;
        use std::process::{Command, Stdio};
        let rate = json!({"turnId":"turn", "active":false,"estimated":true,"rate":1.25,"outputTokens":400});
        let cases = json!([
            {"resources":[{"kind":"accounts"},{"kind":"session-cost","agentId":"agent"}],"sequence":0,"reconnect":false,"rates":{"rates":{},"teams":{}}},
            {"resources":[{"kind":"state"}],"sequence":42,"reconnect":true,"rates":{"rates":{"agent":rate},"teams":{"lead":{"agent":rate}}}}
        ]);
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let mut child = Command::new(python::prepared_python(&source))
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../runtime-services/tests/wave7_p14_oracle.py"),
            )
            .arg("documents")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("oracle starts");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(cases.to_string().as_bytes())
            .expect("cases written");
        let output = child.wait_with_output().expect("oracle finishes");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Vec<Value> = serde_json::from_slice(&output.stdout).expect("oracle JSON");
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
            assert_eq!(
                serde_json::from_str::<Value>(data).expect("JSON"),
                expected["rates"]
            );
        }
    }

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
            notifications: Arc::new(ResourceNotifications::new(Arc::new(hub.clone()))),
            token_rates: TokenRateHub::new().subscribe(),
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
    async fn token_rate_burst_keeps_only_latest_snapshot_and_monotone_cursor() {
        let hub = Arc::new(ChangeNotificationHub::new());
        let notifications = Arc::new(ResourceNotifications::new(hub.clone()));
        let token_hub = TokenRateHub::with_change_callback({
            let hub = hub.clone();
            Arc::new(move || hub.notify_one())
        });
        let changes = hub.subscribe();
        let baseline = changes.borrow().clone();
        let state = ResourceStream {
            initial: VecDeque::new(),
            workspace_id: "workspace".to_owned(),
            epoch: baseline.epoch,
            last_revision: baseline.revision,
            resources: vec![json!({"kind":"accounts"})],
            entity_sequence: 0,
            store: None,
            changes,
            notifications: notifications.clone(),
            token_rates: token_hub.subscribe(),
            _progress_watch: None,
            heartbeat: interval_at(
                Instant::now() + Duration::from_secs(RESOURCE_HEARTBEAT_SECONDS),
                Duration::from_secs(RESOURCE_HEARTBEAT_SECONDS),
            ),
        };
        for index in 0..=MAX_RESOURCE_COUNT {
            token_hub.publish(json!({
                "rates": {"agent": {"turnId":"turn", "active":true, "estimated":false, "rate":index, "outputTokens":index}}, "teams": {}
            })).expect("publish");
        }
        let (_, state) = next_frame(state).await.expect("resource frame");
        assert!(state.initial.is_empty());
        assert_eq!(state.last_revision, MAX_RESOURCE_COUNT as i64 + 1);
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
            notifications: Arc::new(ResourceNotifications::new(Arc::new(hub.clone()))),
            token_rates: TokenRateHub::new().subscribe(),
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
