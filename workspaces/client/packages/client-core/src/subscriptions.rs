//! Native subscription protocol. Only committed screen values cross the UI boundary.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    events::Parser,
    protocol::{Request, Response},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};
use studio_runtime_schema::{
    SYNC_PROTOCOL_VERSION_TEXT, api_schema, first_error,
    shared_values::{MAX_SYNC_RESOURCE_COUNT, SYNC_STREAM_EVENT_MAX_BYTES},
};
use tokio::sync::mpsc;

// Native stream silence deadline, in seconds. A silent stream cannot prove liveness.
const NATIVE_STREAM_SILENCE_SECONDS: u64 = 60;
// Native reconnect backoff bounds, in milliseconds. These are not UI deadlines.
const NATIVE_RECONNECT_BASE_MS: u64 = 500;
const NATIVE_RECONNECT_MAX_MS: u64 = 15_000;
// Explicit fresh-view read deadline, in seconds. Cached data cannot satisfy this read.
const NATIVE_SNAPSHOT_DEADLINE_SECONDS: u64 = 120;

pub(crate) fn schema_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        api_schema::schema_hash(
            &serde_json::from_slice(include_bytes!(
                "../../../../runtime/packages/runtime-http/openapi.json"
            ))
            .expect("committed OpenAPI document"),
        )
    })
}

fn validate(schema: &str, value: &Value) -> Result<(), String> {
    match first_error(schema, value).map_err(|error| error.to_string())? {
        Some((path, message)) => Err(format!("Invalid {schema} at {path}: {message}")),
        None => Ok(()),
    }
}

fn checked_event(name: &str, data: &str, workspace: &str) -> Result<Value, String> {
    if data.len() > SYNC_STREAM_EVENT_MAX_BYTES {
        return Err("The subscription event exceeds its byte limit.".into());
    }
    let schema = match name {
        "resources" => "ResourceChangeEvent",
        "entity-transfer" => "ResourceEntityTransferEvent",
        "resource-progress" => "ResourceProgressEvent",
        "heartbeat" => "ResourceHeartbeatEvent",
        "token-rates" => "ResourceTokenRatesEvent",
        "session" => "SyncSessionEvent",
        "subscriptions" => "SyncSubscriptionAck",
        _ => return Err("Unknown native subscription event.".into()),
    };
    let value: Value = serde_json::from_str(data).map_err(|error| error.to_string())?;
    validate(schema, &value)?;
    if value.get("workspaceId").is_some_and(|id| id != workspace) {
        return Err("The subscription belongs to another workspace.".into());
    }
    Ok(value)
}

async fn emit(output: &mpsc::Sender<Value>, id: &str, value: Value) -> Result<(), String> {
    output
        .send(json!({"event":id,"value":value}))
        .await
        .map_err(|_| "The native subscription view closed.".into())
}

fn owned(resources: &[Value], resource: &Value) -> Result<(), String> {
    if resources.contains(resource) {
        Ok(())
    } else {
        Err("The subscription event names an unselected resource.".into())
    }
}

async fn stream_path(
    store: &StoreSender,
    workspace: &str,
    resources: &[Value],
    tokens: bool,
    fresh: bool,
) -> Result<String, String> {
    let mut since = Vec::new();
    for resource in resources {
        let cached = state(
            store,
            "projection.readSubscription",
            json!({"workspace":workspace,"resource":resource}),
        )
        .await?;
        if let Some(cursor) = cached
            .get("cursor")
            .filter(|value| !value.is_null() && !fresh)
        {
            validate("SubscriptionCursor", cursor)?;
            if cursor["workspaceId"] != workspace {
                return Err("The saved subscription cursor belongs to another workspace.".into());
            }
            // Terminal deltas require a baseline under the current stream owner.
            if resource["kind"] != "terminal-output" {
                since.push(json!({"resource":resource,"since":cursor}));
            }
        }
    }
    let mut url = reqwest::Url::parse("http://native.invalid/api/sync/stream").unwrap();
    url.query_pairs_mut()
        .append_pair("protocol", SYNC_PROTOCOL_VERSION_TEXT)
        .append_pair(
            "resources",
            &serde_json::to_string(resources).map_err(|e| e.to_string())?,
        )
        .append_pair(
            "since",
            &serde_json::to_string(&since).map_err(|e| e.to_string())?,
        )
        .append_pair("tokenRates", if tokens { "true" } else { "false" })
        .append_pair("reconnect", "true")
        .append_pair(api_schema::API_SCHEMA_HASH_PARAM, schema_hash());
    Ok(format!("{}?{}", url.path(), url.query().unwrap()))
}

async fn apply_frame(
    id: &str,
    name: &str,
    frame: Value,
    resources: &[Value],
    workspace: &str,
    backend: &Backend,
    store: &StoreSender,
    output: &mpsc::Sender<Value>,
) -> Result<(), String> {
    match name {
        "resources" => {
            for resource in frame["resources"]
                .as_array()
                .ok_or("Missing resource selection.")?
            {
                owned(resources, resource)?;
            }
            for (index, entry) in frame["resourceVersions"]
                .as_array()
                .ok_or("Missing resource revisions.")?
                .iter()
                .enumerate()
            {
                let resource = entry
                    .get("resource")
                    .filter(|v| !v.is_null())
                    .unwrap_or(&frame["resources"][index]);
                owned(resources, resource)?;
            }
            for (index, entry) in frame["resourceVersions"]
                .as_array()
                .ok_or("Missing resource revisions.")?
                .iter()
                .enumerate()
            {
                let resource = entry
                    .get("resource")
                    .filter(|v| !v.is_null())
                    .unwrap_or(&frame["resources"][index]);
                owned(resources, resource)?;
                let committed = state(store, "projection.applySubscription", json!({
                    "workspace":workspace,"resource":resource,"epoch":frame["epoch"],"entry":entry
                })).await?;
                let committed = apply_voice_view(backend, store, committed).await?;
                // No UI notification can precede the SQLite transaction receipt.
                emit(output, id, json!({"type":"resource","snapshot":committed})).await?;
            }
        }
        "entity-transfer" => {
            owned(resources, &frame["resource"])?;
            let committed = state(
                store,
                "projection.stageSubscriptionFragment",
                json!({"workspace":workspace,"frame":frame}),
            )
            .await?;
            if !committed.is_null() {
                let committed = apply_voice_view(backend, store, committed).await?;
                emit(output, id, json!({"type":"resource","snapshot":committed})).await?;
            }
        }
        "resource-progress" => {
            owned(resources, &frame["resource"])?;
            emit(
                output,
                id,
                json!({"type":"resource","snapshot":{
                    "resource":frame["resource"],"data":frame["value"],"status":"ready",
                    "version":{"epoch":frame["epoch"],"revision":frame["revision"],
                        "progress":{"resource":frame["resource"],"value":frame["value"]}}
                }}),
            )
            .await?;
        }
        "token-rates" => emit(output, id, json!({"type":"token-rates","value":frame})).await?,
        "heartbeat" => {}
        // Fixed-interest streams do not post subscription mutations. A diff ack
        // is not evidence that one of this owner's commands was accepted.
        "subscriptions" => return Err("Unexpected subscription mutation acknowledgement.".into()),
        _ => return Err("Unexpected native subscription event.".into()),
    }
    Ok(())
}

async fn apply_voice_view(
    backend: &Backend,
    store: &StoreSender,
    mut snapshot: Value,
) -> Result<Value, String> {
    if snapshot["resource"]["kind"] == "voice" && snapshot["status"] == "ready" {
        let agent = snapshot["resource"]["agentId"]
            .as_str()
            .filter(|agent| !agent.is_empty())
            .ok_or("Missing voice subscription owner.")?
            .to_owned();
        let epoch = snapshot["version"]["epoch"]
            .as_str()
            .filter(|epoch| !epoch.is_empty())
            .ok_or("Missing voice subscription epoch.")?
            .to_owned();
        snapshot["data"] =
            crate::voice::apply_records(store, &agent, &epoch, &snapshot["data"]).await?;
        if !snapshot["data"]["session"].is_null() {
            crate::voice::apply_session(backend, store, &agent, &snapshot["data"]["session"])
                .await?;
        }
    }
    Ok(snapshot)
}

/// A failed stream stops only voice sessions selected by this immutable owner.
async fn close_selected_voice(
    backend: &Backend,
    store: &StoreSender,
    resources: &[Value],
) -> Result<(), String> {
    let agents = resources
        .iter()
        .filter(|resource| resource["kind"] == "voice")
        .filter_map(|resource| resource["agentId"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for agent in agents {
        crate::voice::transport_closed(backend, store, agent).await?;
    }
    Ok(())
}

pub async fn run(
    id: String,
    params: Value,
    backend: Arc<Result<Backend, String>>,
    store: StoreSender,
    output: mpsc::Sender<Value>,
) {
    let result: Result<(), String> = async {
        let backend = backend.as_ref().as_ref().map_err(Clone::clone)?;
        let workspace = params["workspace"].as_str().filter(|id| crate::projections::valid_workspace(id))
            .ok_or("A verified subscription workspace is required.")?;
        let resources = params["resources"].as_array().ok_or("A resource selection is required.")?;
        if resources.len() > MAX_SYNC_RESOURCE_COUNT {
            return Err("The native resource selection exceeds its limit.".into());
        }
        for resource in resources { validate("ResourceRef", resource)?; }
        let tokens = params["tokenRates"].as_bool().unwrap_or(false);
        let fresh = params["forceSnapshot"].as_bool().unwrap_or(false);
        if resources.is_empty() && !tokens { return Err("The native subscription has no interest.".into()); }
        let mut retry = NATIVE_RECONNECT_BASE_MS;
        loop {
            emit(&output, &id, json!({"type":"connection","status":"connecting","stream":"connecting"})).await?;
            let path = match stream_path(&store, workspace, resources, tokens, fresh).await {
                Ok(path) => path,
                Err(message) => {
                    // A cache failure opens no stream and authorizes no retry.
                    if let Err(cleanup) = close_selected_voice(backend, &store, resources).await {
                        return Err(format!("{message} Voice cleanup failed: {cleanup}"));
                    }
                    return Err(message);
                }
            };
            let attempt: Result<bool, String> = async {
                let Some(mut response) = backend.event_stream(&path, "").await.map_err(|e|e.message)? else {
                    close_selected_voice(backend, &store, resources).await?;
                    emit(&output, &id, json!({"type":"connection","status":"offline","stream":"stopped"})).await?;
                    return Ok(false);
                };
                let mut parser = Parser::new(String::new());
                let mut handshake = false;
                let mut session = None;
                let mut preference_ready = false;
                loop {
                    let chunk = tokio::time::timeout(Duration::from_secs(NATIVE_STREAM_SILENCE_SECONDS), response.chunk()).await
                        .map_err(|_| "The native subscription stopped responding.")?
                        .map_err(|e|e.to_string())?;
                    let Some(chunk) = chunk else { return Ok(true); };
                    for event in parser.feed(&chunk)? {
                        let name = event["type"].as_str().ok_or("Missing subscription event type.")?;
                        let data = event["data"].as_str().ok_or("Missing subscription event data.")?;
                        if name == "api-schema" {
                            let value: Value = serde_json::from_str(data).map_err(|e|e.to_string())?;
                            if value["hash"] != schema_hash() || value.get(api_schema::API_SCHEMA_MISMATCH_FIELD).is_some_and(|v|v != false) {
                                close_selected_voice(backend, &store, resources).await?;
                                emit(&output, &id, json!({"type":"connection","status":"schema-mismatch","stream":"stopped"})).await?;
                                return Ok(false);
                            }
                            handshake = true;
                            continue;
                        }
                        let frame = checked_event(name, data, workspace)?;
                        if name == "session" {
                            if session.replace(frame["sessionId"].clone()).is_some() { return Err("The stream session changed without a new connection.".into()); }
                            continue;
                        }
                        if !handshake || session.is_none() { return Err("The subscription did not complete its schema and session handshake.".into()); }
                        apply_frame(&id, name, frame, resources, workspace, backend, &store, &output).await?;
                        if !preference_ready {
                            // This wake follows this connection's validated schema,
                            // session and resource frame, never an untrusted hint.
                            state(&store,"preference_sync.network_ready",json!({})).await?;
                            preference_ready = true;
                        }
                        retry = NATIVE_RECONNECT_BASE_MS;
                        emit(&output, &id, json!({"type":"connection","status":"live","stream":"open"})).await?;
                    }
                }
            }.await;
            if matches!(attempt, Ok(false)) { return Ok(()); }
            let message = attempt.err().unwrap_or_else(|| "The native subscription stream closed.".into());
            close_selected_voice(backend, &store, resources).await?;
            emit(&output, &id, json!({"type":"connection","status":"degraded","stream":"connecting","message":message})).await?;
            tokio::time::sleep(Duration::from_millis(retry)).await;
            retry = retry.saturating_mul(2).min(NATIVE_RECONNECT_MAX_MS);
        }
    }.await;
    if let Err(message) = result {
        let _ = output
            .send(json!({"event":id,"error":{"code":"subscription_error","message":message}}))
            .await;
    }
}

/// A fresh read owns a temporary stream until one selected view commits. A lost
/// response never repeats a mutation because this operation sends no mutation.
pub async fn dispatch(backend: &Backend, store: &StoreSender, request: Request) -> Response {
    if request.method != "subscriptions.snapshot" {
        return Response::failure(
            request.id,
            "invalid_subscription",
            "Unknown subscription command.",
        );
    }
    let params = &request.params;
    let Some(workspace) = params["workspace"]
        .as_str()
        .filter(|id| crate::projections::valid_workspace(id))
    else {
        return Response::failure(
            request.id,
            "invalid_subscription",
            "A verified subscription workspace is required.",
        );
    };
    let resource = params["resource"].clone();
    if let Err(message) = validate("ResourceRef", &resource) {
        return Response::failure(request.id, "invalid_subscription", message);
    }
    let (output, mut events) = mpsc::channel(8);
    let task = tokio::spawn(run(
        request.id.clone(),
        json!({"workspace":workspace,"resources":[resource],"tokenRates":false,"forceSnapshot":true}),
        Arc::new(Ok(backend.clone())),
        store.clone(),
        output,
    ));
    struct CancelOnDrop(tokio::task::AbortHandle);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _cancel = CancelOnDrop(task.abort_handle());
    let read = async {
        while let Some(event) = events.recv().await {
            if let Some(error) = event.get("error") {
                return Err(error["message"]
                    .as_str()
                    .unwrap_or("The fresh subscription read failed.")
                    .to_owned());
            }
            let value = &event["value"];
            if value["type"] == "connection"
                && matches!(
                    value["status"].as_str(),
                    Some("schema-mismatch" | "offline")
                )
            {
                return Err("The server cannot confirm a fresh subscription view.".into());
            }
            if value["type"] == "resource"
                && value["snapshot"]["resource"] == resource
                && value["snapshot"]["status"] == "ready"
            {
                return Ok(value["snapshot"].clone());
            }
        }
        Err("The fresh subscription stream closed before its view committed.".into())
    };
    let result: Result<Value, String> =
        match tokio::time::timeout(Duration::from_secs(NATIVE_SNAPSHOT_DEADLINE_SECONDS), read)
            .await
        {
            Ok(result) => result,
            Err(_) => Err("The server did not confirm a fresh subscription view in time.".into()),
        };
    task.abort();
    let _ = task.await;
    match result {
        Ok(value) => Response::success(request.id, value),
        Err(message) => Response::failure(request.id, "subscription_unconfirmed", message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice_frame(agent: &str, epoch: &str, after: u64, cursor: u64, record_agent: &str) -> Value {
        let resource = json!({"kind":"voice","agentId":agent});
        json!({"protocol":4,"workspaceId":"a".repeat(32),"epoch":epoch,"revision":cursor,
        "reason":"change","resources":[resource.clone()],"resourceVersions":[{
            "resource":resource,"revision":cursor,"replyKind":"snapshot",
            "voiceRecords":{"after":after,"cursor":cursor,"session":null,"records":[{
                "id":format!("record-{cursor}"),"agent":record_agent,"session":"fixture-session",
                "seq":cursor,"kind":"user","text":format!("voice text {cursor}"),
                "item_id":"","previous_item_id":"","payload":"{}","created":cursor
            }]}
        }]})
    }

    /// Exercise the real HTTP stream parser and SQLite projection owner, without audio.
    async fn voice_stream_frames(frames: Vec<Value>, resource: Value) -> (Vec<Value>, Value) {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, _) = crate::backend::tests::read_http(&mut socket).await;
            assert!(headers.starts_with("GET /api/sync/stream?"), "{headers}");
            let mut body = format!(
                "event: session\ndata: {{\"sessionId\":\"00000000-0000-0000-0000-000000000001\"}}\n\nevent: api-schema\ndata: {{\"hash\":\"{}\"}}\n\n",
                schema_hash()
            );
            for frame in frames {
                body.push_str(&format!("event: resources\ndata: {frame}\n\n"));
            }
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut database =
            crate::store::Store::open(&directory.path().join("client.sqlite3")).unwrap();
        let (store, mut commands) = mpsc::channel::<crate::delivery::StoreCommand>(8);
        let worker = tokio::spawn(async move {
            while let Some((request, receipt)) = commands.recv().await {
                let _ = receipt.send(database.dispatch(request));
            }
        });
        let (output, mut events) = mpsc::channel(32);
        let task = tokio::spawn(run(
            "voice-view".into(),
            json!({"workspace":"a".repeat(32),"resources":[resource.clone()]}),
            Arc::new(Ok(backend)),
            store.clone(),
            output,
        ));
        let received = tokio::time::timeout(Duration::from_secs(5), async {
            let mut received = Vec::new();
            while let Some(event) = events.recv().await {
                let terminal = event.get("error").is_some()
                    || (event["value"]["type"] == "connection"
                        && matches!(
                            event["value"]["status"].as_str(),
                            Some("degraded" | "schema-mismatch" | "offline")
                        ));
                received.push(event);
                if terminal {
                    break;
                }
            }
            received
        })
        .await;
        task.abort();
        let _ = task.await;
        server.await.unwrap();
        let cached = state(
            &store,
            "projection.readSubscription",
            json!({"workspace":"a".repeat(32),"resource":resource}),
        )
        .await
        .unwrap();
        drop(store);
        worker.await.unwrap();
        (
            received.expect("the finite stream must report its terminal state"),
            cached,
        )
    }

    #[tokio::test]
    async fn native_voice_subscription_merges_transcript_frames_without_durable_voice_bytes() {
        let resource = json!({"kind":"voice","agentId":"voice-chat"});
        let (events, cached) = voice_stream_frames(
            vec![
                voice_frame("voice-chat", "source", 0, 1, "voice-chat"),
                voice_frame("voice-chat", "source", 1, 2, "voice-chat"),
            ],
            resource.clone(),
        )
        .await;
        let snapshots = events
            .iter()
            .filter(|event| event["value"]["type"] == "resource")
            .map(|event| &event["value"]["snapshot"])
            .collect::<Vec<_>>();
        assert_eq!(snapshots.len(), 2, "{events:?}");
        assert_eq!(
            snapshots[1]["data"]["records"].as_array().unwrap().len(),
            2,
            "each voice frame must reach the ephemeral transcript owner"
        );
        assert_eq!(snapshots[1]["data"]["records"][0]["id"], "record-1");
        assert_eq!(snapshots[1]["data"]["records"][1]["id"], "record-2");
        assert_eq!(snapshots[1]["data"]["view"]["gap"], false);
        assert_eq!(
            cached["data"],
            Value::Null,
            "voice transcript bytes must not enter SQLite"
        );
        assert_eq!(cached["version"], Value::Null);
    }

    #[tokio::test]
    async fn native_voice_subscription_refuses_foreign_record_owner_before_ui_publication() {
        let resource = json!({"kind":"voice","agentId":"selected-chat"});
        let (events, cached) = voice_stream_frames(
            vec![voice_frame("selected-chat", "source", 0, 1, "another-chat")],
            resource,
        )
        .await;
        assert!(
            !events
                .iter()
                .any(|event| event["value"]["type"] == "resource"),
            "a valid wire record from another chat cannot cross the selected voice owner: {events:?}"
        );
        assert_eq!(cached["data"], Value::Null);
    }

    #[tokio::test]
    async fn native_voice_subscription_refuses_a_retired_epoch_on_the_real_stream() {
        let resource = json!({"kind":"voice","agentId":"epoch-chat"});
        let (events, cached) = voice_stream_frames(
            vec![
                voice_frame("epoch-chat", "old", 0, 1, "epoch-chat"),
                voice_frame("epoch-chat", "new", 0, 1, "epoch-chat"),
                voice_frame("epoch-chat", "old", 1, 2, "epoch-chat"),
            ],
            resource,
        )
        .await;
        let snapshots = events
            .iter()
            .filter(|event| event["value"]["type"] == "resource")
            .map(|event| &event["value"]["snapshot"])
            .collect::<Vec<_>>();
        assert_eq!(
            snapshots.len(),
            2,
            "retired epoch cannot publish another voice display: {events:?}"
        );
        assert_eq!(snapshots[1]["version"]["epoch"], "new");
        assert_eq!(cached["data"], Value::Null);
    }

    async fn voice_lifecycle_stream(mode: &str, scope: &str) -> (bool, Value, Vec<Value>) {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let directory = tempfile::tempdir().unwrap();
        let mut workers = Vec::new();
        let mut stores = Vec::new();
        for name in ["owner", "sibling"] {
            let mut database =
                crate::store::Store::open(&directory.path().join(format!("{name}.sqlite3")))
                    .unwrap();
            assert!(
                database
                    .dispatch(Request {
                        id: "identity".into(),
                        method: "identity.set".into(),
                        params: json!({"workspaceId":"a".repeat(32)})
                    })
                    .error
                    .is_none()
            );
            if mode == "cursor-corruption" {
                let baseline = database.dispatch(Request {
                    id: "baseline".into(),
                    method: "projection.applySubscription".into(),
                    params: json!({"workspace":"a".repeat(32),"resource":{"kind":"state"},
                        "epoch":"source","entry":resource_frame()["resourceVersions"][0]}),
                });
                assert!(baseline.error.is_none(), "{baseline:?}");
                assert_eq!(database.connection.execute(
                    "UPDATE client_subscription_views SET document=json_set(document,'$.cursor.workspaceId',?1)",
                    ["b".repeat(32)],
                ).unwrap(), 1);
            }
            let (sender, mut commands) = mpsc::channel::<crate::delivery::StoreCommand>(8);
            workers.push(tokio::spawn(async move {
                while let Some((request, receipt)) = commands.recv().await {
                    let _ = receipt.send(database.dispatch(request));
                }
            }));
            stores.push(sender);
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let fixture = crate::voice::test_owned_session(
            &backend,
            &stores[0],
            "voice-owner",
            "stream-start",
            true,
        )
        .await
        .unwrap();
        let session_id = fixture.session_id().to_owned();
        let agent = if scope == "another-agent" {
            "another-agent"
        } else {
            "voice-owner"
        };
        let resource = if scope == "state-only" {
            json!({"kind":"state"})
        } else {
            json!({"kind":"voice","agentId":agent})
        };
        let resources = if mode == "cursor-corruption" && scope != "state-only" {
            vec![resource, json!({"kind":"state"})]
        } else {
            vec![resource]
        };
        let mut frame = voice_frame(agent, "source", 0, 1, agent);
        frame["resourceVersions"][0]["voiceRecords"]["session"] = json!({
            "session_id":if scope == "another-session" { "changed-session" } else { &session_id },
            "state":"ended","sdp":null,"error":null,"ended":1
        });
        let hash = if mode == "schema-stop" {
            "different-schema"
        } else {
            schema_hash()
        };
        let mut body = format!(
            "event: session\ndata: {{\"sessionId\":\"00000000-0000-0000-0000-000000000001\"}}\n\nevent: api-schema\ndata: {{\"hash\":\"{hash}\"}}\n\n"
        );
        if mode == "session" {
            body.push_str(&format!("event: resources\ndata: {frame}\n\n"));
        } else if mode == "malformed" {
            body.push_str("event: resources\ndata: {\n\n");
        }
        let keep_stream_open = mode == "session";
        let cache_failure = mode == "cursor-corruption";
        let ends = Arc::new(std::sync::Mutex::new(Vec::new()));
        let saved_ends = ends.clone();
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            if !cache_failure {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (headers, _) = crate::backend::tests::read_http(&mut socket).await;
                assert!(headers.starts_with("GET /api/sync/stream?"), "{headers}");
                if keep_stream_open {
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n",body.len()).as_bytes()).await.unwrap();
                    let _ = held.await;
                    return;
                }
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (headers, body) = crate::backend::tests::read_http(&mut socket).await;
                let (status, reply) = if headers.starts_with("GET /api/session ") {
                    ("200 OK", json!({"token":"fixture"}))
                } else {
                    assert!(headers.starts_with("POST /api/voice/end "), "{headers}");
                    saved_ends.lock().unwrap().push(body);
                    (
                        "503 Service Unavailable",
                        json!({"error":"End outcome unknown"}),
                    )
                };
                let reply = reply.to_string();
                socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
            }
        });
        let stream_store = stores[usize::from(scope == "sibling-store")].clone();
        let (output, mut events) = mpsc::channel(16);
        let task = tokio::spawn(run(
            "lifecycle-view".into(),
            json!({"workspace":"a".repeat(32),"resources":resources}),
            Arc::new(Ok(backend)),
            stream_store,
            output,
        ));
        let observed = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                if (keep_stream_open && event["value"]["type"] == "resource")
                    || event.get("error").is_some()
                    || (event["value"]["type"] == "connection"
                        && matches!(
                            event["value"]["status"].as_str(),
                            Some("degraded" | "schema-mismatch" | "offline")
                        ))
                {
                    if cache_failure {
                        let message = event["error"]["message"]
                            .as_str()
                            .or_else(|| event["value"]["message"].as_str());
                        assert_eq!(
                            message,
                            Some("The saved subscription cursor belongs to another workspace.")
                        );
                    }
                    return;
                }
            }
        })
        .await;
        let closed = fixture.closed();
        // Read the original identity without creating another claim.
        let (reply, receive) = tokio::sync::oneshot::channel();
        stores[0]
            .send((
                Request {
                    id: "stream-start".into(),
                    method: "voice_journal.begin".into(),
                    params: json!({"agent":"voice-owner","muted":false}),
                },
                reply,
            ))
            .await
            .unwrap();
        let journal = receive.await.unwrap().result.unwrap();
        fixture.clear().await;
        task.abort();
        let _ = task.await;
        let _ = release.send(());
        server.abort();
        if let Err(error) = server.await {
            assert!(error.is_cancelled(), "the HTTP fixture failed: {error}");
        }
        let ended = ends.lock().unwrap().clone();
        drop(stores);
        for worker in workers {
            worker.await.unwrap();
        }
        observed.expect("the owned stream must report its frame or terminal state");
        assert_eq!(journal["row"]["sessionId"], session_id);
        (closed, journal, ended)
    }

    #[test]
    fn native_voice_subscription_applies_async_session_only_to_its_exact_owner() {
        let _serial = crate::media::AUDIO_TEST_MUTEX.lock().unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for scope in ["sibling-store", "another-agent", "another-session", "owned"] {
                let (closed, journal, ends) = voice_lifecycle_stream("session", scope).await;
                assert_eq!(closed, scope == "owned", "{scope}: {journal}");
                assert_eq!(
                    journal["row"]["phase"],
                    if scope == "owned" { "ended" } else { "claimed" },
                    "{scope}"
                );
                assert!(ends.is_empty(), "a server End must not send another End");
            }
        });
    }

    #[test]
    fn native_voice_subscription_transport_failure_stops_only_selected_owner_and_keeps_end_unknown()
    {
        let _serial = crate::media::AUDIO_TEST_MUTEX.lock().unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for mode in ["eof", "schema-stop", "malformed"] {
                for scope in ["sibling-store", "another-agent", "state-only", "owned"] {
                    let (closed, journal, ends) = voice_lifecycle_stream(mode, scope).await;
                    assert_eq!(closed, scope == "owned", "{mode}/{scope}: {journal}");
                    assert_eq!(journal["row"]["phase"], if scope == "owned" {"unknown"} else {"claimed"}, "{mode}/{scope}");
                    if scope == "owned" {
                        assert_eq!(ends, vec![json!({"agent":"voice-owner","session_id":journal["row"]["sessionId"]})], "{mode}");
                    } else {
                        assert!(ends.is_empty(), "a stream cannot stop a sibling: {mode}/{scope}");
                    }
                }
            }
        });
    }

    #[test]
    fn native_voice_subscription_cursor_failure_stops_selected_owner_before_any_stream_get() {
        let _serial = crate::media::AUDIO_TEST_MUTEX.lock().unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for scope in ["sibling-store", "another-agent", "state-only", "owned"] {
                let (closed, journal, ends) =
                    voice_lifecycle_stream("cursor-corruption", scope).await;
                assert_eq!(closed, scope == "owned", "{scope}: {journal}");
                assert_eq!(
                    journal["row"]["phase"],
                    if scope == "owned" {
                        "unknown"
                    } else {
                        "claimed"
                    },
                    "{scope}"
                );
                if scope == "owned" {
                    assert_eq!(
                        ends,
                        vec![
                            json!({"agent":"voice-owner","session_id":journal["row"]["sessionId"]})
                        ]
                    );
                } else {
                    assert!(
                        ends.is_empty(),
                        "a cursor failure cannot stop a sibling: {scope}"
                    );
                }
            }
        });
    }

    fn resource_frame() -> Value {
        json!({"protocol":4,"workspaceId":"a".repeat(32),"epoch":"first","revision":0,
            "reason":"initial","resources":[{"kind":"state"}],"resourceVersions":[{
                "resource":{"kind":"state"},"revision":0,"cursor":{"workspaceId":"a".repeat(32),"seq":0},
                "replyKind":"snapshot","entityChanges":{"after":0,"through":0,"documents":[]}}]})
    }

    #[test]
    fn native_subscription_accepts_only_canonical_owned_resource_frames() {
        let frame = resource_frame();
        assert_eq!(
            checked_event("resources", &frame.to_string(), &"a".repeat(32)).unwrap(),
            frame
        );
        assert!(checked_event("resources", &frame.to_string(), &"b".repeat(32)).is_err());
        let mut invalid = frame.clone();
        invalid["resourceVersions"][0]["cursor"]["seq"] = json!(-1);
        assert!(checked_event("resources", &invalid.to_string(), &"a".repeat(32)).is_err());
        invalid = frame;
        invalid["unknown"] = json!(true);
        assert!(checked_event("resources", &invalid.to_string(), &"a".repeat(32)).is_err());
        assert!(checked_event("resources", "{", &"a".repeat(32)).is_err());
    }

    #[test]
    fn native_subscription_never_accepts_unknown_or_oversized_wire_events() {
        assert!(checked_event("invented", "{}", &"a".repeat(32)).is_err());
        let data =
            " ".repeat(studio_runtime_schema::shared_values::SYNC_STREAM_EVENT_MAX_BYTES + 1);
        assert!(checked_event("resources", &data, &"a".repeat(32)).is_err());
    }

    #[tokio::test]
    async fn native_subscription_publishes_only_after_the_store_commit_receipt() {
        use crate::protocol::Response;
        let workspace = "a".repeat(32);
        let frame = resource_frame();
        let resources = frame["resources"].as_array().unwrap().clone();
        let (store, mut commands) = mpsc::channel(1);
        let (output, mut events) = mpsc::channel(4);
        let task = tokio::spawn(async move {
            let backend = Backend::new("http://127.0.0.1:1").unwrap();
            apply_frame(
                "view",
                "resources",
                frame,
                &resources,
                &workspace,
                &backend,
                &store,
                &output,
            )
            .await
        });
        let (request, receipt) = commands.recv().await.unwrap();
        assert_eq!(request.method, "projection.applySubscription");
        assert_eq!(request.params["resource"], json!({"kind":"state"}));
        assert!(
            events.try_recv().is_err(),
            "A frame is not a storage receipt."
        );
        let committed = json!({"resource":{"kind":"state"},"data":[],"status":"ready","version":{"epoch":"first","revision":0}});
        receipt
            .send(Response::success(request.id, committed.clone()))
            .unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(
            events.recv().await.unwrap(),
            json!({"event":"view","value":{"type":"resource","snapshot":committed}})
        );
    }

    #[tokio::test]
    async fn native_subscription_reconnect_uses_only_a_committed_owned_cursor() {
        use crate::protocol::Response;
        let workspace = "a".repeat(32);
        let resource = json!({"kind":"state"});
        let (store, mut commands) = mpsc::channel(1);
        let expected = workspace.clone();
        let task =
            tokio::spawn(
                async move { stream_path(&store, &expected, &[resource], false, false).await },
            );
        let (request, receipt) = commands.recv().await.unwrap();
        assert_eq!(request.method, "projection.readSubscription");
        receipt
            .send(Response::success(
                request.id,
                json!({"cursor":{"workspaceId":workspace,"seq":7}}),
            ))
            .unwrap();
        let path = task.await.unwrap().unwrap();
        let url = reqwest::Url::parse(&format!("http://native.invalid{path}")).unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query["protocol"], SYNC_PROTOCOL_VERSION_TEXT);
        assert_eq!(query[api_schema::API_SCHEMA_HASH_PARAM], schema_hash());
        assert_eq!(
            serde_json::from_str::<Value>(&query["since"]).unwrap(),
            json!([
                {"resource":{"kind":"state"},"since":{"workspaceId":workspace,"seq":7}}
            ])
        );
    }

    #[tokio::test]
    async fn native_fresh_snapshot_reads_the_real_stream_and_commits_before_returning() {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let frame = resource_frame();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, _) = crate::backend::tests::read_http(&mut socket).await;
            let target = headers
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let url = reqwest::Url::parse(&format!("http://native.invalid{target}")).unwrap();
            let query = url
                .query_pairs()
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(
                query["since"], "[]",
                "An explicit fresh read cannot use cached cursor proof."
            );
            assert_eq!(query[api_schema::API_SCHEMA_HASH_PARAM], schema_hash());
            let body = format!(
                "event: session\ndata: {{\"sessionId\":\"00000000-0000-0000-0000-000000000001\"}}\n\nevent: api-schema\ndata: {{\"hash\":\"{}\"}}\n\nevent: resources\ndata: {}\n\n",
                schema_hash(),
                frame
            );
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let mut database = crate::store::Store::open(&path).unwrap();
        let (store, mut commands) = mpsc::channel::<crate::delivery::StoreCommand>(8);
        let worker = tokio::spawn(async move {
            while let Some((request, receipt)) = commands.recv().await {
                let response = database.dispatch(request);
                let _ = receipt.send(response);
            }
        });
        let result = dispatch(
            &backend,
            &store,
            Request {
                id: "fresh-view".into(),
                method: "subscriptions.snapshot".into(),
                params: json!({"workspace":"a".repeat(32),"resource":{"kind":"state"}}),
            },
        )
        .await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.result.as_ref().unwrap()["status"], "ready");
        assert_eq!(result.result.as_ref().unwrap()["cursor"]["seq"], 0);
        drop(store);
        worker.await.unwrap();
        server.await.unwrap();
        let mut database = crate::store::Store::open(&path).unwrap();
        let saved = database.dispatch(Request {
            id: "readback".into(),
            method: "projection.readSubscription".into(),
            params: json!({"workspace":"a".repeat(32),"resource":{"kind":"state"}}),
        });
        assert_eq!(saved.result.unwrap()["cursor"]["seq"], 0);
    }
}
