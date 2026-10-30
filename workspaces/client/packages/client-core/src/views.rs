//! UI subscriptions contain values and errors. Rust owns refresh and retry policy.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    protocol::{Request, Response},
    sync,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use studio_client_domain::json_object::JsonObject;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

// Non-string optional fields historically behave like absent values.
#[derive(Deserialize)]
#[serde(untagged)]
enum TextField<'a> {
    Text(&'a str),
    Other(serde::de::IgnoredAny),
}
impl<'a> TextField<'a> {
    fn text(&self) -> Option<&'a str> {
        match self {
            Self::Text(value) => Some(value),
            Self::Other(_) => None,
        }
    }
}
#[derive(Deserialize)]
struct Identity<'a> {
    #[serde(borrow)]
    subscription: Option<TextField<'a>>,
}
#[derive(Deserialize)]
struct Reason<'a> {
    #[serde(borrow)]
    reason: Option<TextField<'a>>,
}
#[derive(Deserialize)]
struct Credit<'a> {
    #[serde(borrow)]
    credit: Option<TextField<'a>>,
}
#[derive(Deserialize)]
struct Resume<'a> {
    #[serde(borrow)]
    path: Option<TextField<'a>>,
}
#[derive(Default, Deserialize)]
#[serde(untagged)]
enum FlowControl {
    Enabled(bool),
    Other(serde::de::IgnoredAny),
    #[default]
    Missing,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subscribe<'a> {
    #[serde(borrow)]
    topic: Option<TextField<'a>>,
    #[serde(default)]
    flow_control: FlowControl,
    #[serde(borrow)]
    workspace: Option<TextField<'a>>,
    #[serde(borrow)]
    scope: Option<TextField<'a>>,
}
enum ViewCommand<'a> {
    Unsubscribe(Reason<'a>),
    Next(Credit<'a>),
    Resume(Resume<'a>),
    Restart(Reason<'a>),
    Subscribe(Subscribe<'a>),
    Unknown,
}
fn decode<'a, T: Deserialize<'a>>(value: &'a Value) -> Option<T> {
    JsonObject::<T>::deserialize(value).ok().map(|v| v.0)
}
fn text<'a>(field: &Option<TextField<'a>>) -> &'a str {
    field.as_ref().and_then(TextField::text).unwrap_or("")
}

pub struct Views {
    tasks: HashMap<String, JoinHandle<()>>,
    flows: HashMap<String, Arc<crate::events::EventFlow>>,
    backend: Arc<Result<Backend, String>>,
    store: StoreSender,
    changes: watch::Receiver<u64>,
    output: mpsc::Sender<Value>,
}

impl Views {
    pub fn new(
        backend: Arc<Result<Backend, String>>,
        store: StoreSender,
        changes: watch::Receiver<u64>,
        output: mpsc::Sender<Value>,
    ) -> Self {
        Self {
            tasks: HashMap::new(),
            flows: HashMap::new(),
            backend,
            store,
            changes,
            output,
        }
    }
    pub fn command(&mut self, request: &Request) -> Response {
        let identity = decode::<Identity>(&request.params);
        let Some(id) = identity
            .as_ref()
            .and_then(|v| v.subscription.as_ref())
            .and_then(TextField::text)
            .filter(|id| !id.is_empty() && id.len() <= 128)
        else {
            return Response::failure(
                request.id.clone(),
                "invalid_subscription",
                "A subscription identity is required.",
            );
        };
        let command = match request.method.as_str() {
            "view.unsubscribe" => {
                ViewCommand::Unsubscribe(decode(&request.params).expect("object"))
            }
            "view.next" => ViewCommand::Next(decode(&request.params).expect("object")),
            "view.resume" => ViewCommand::Resume(decode(&request.params).expect("object")),
            "view.restart" => ViewCommand::Restart(decode(&request.params).expect("object")),
            "view.subscribe" => ViewCommand::Subscribe(decode(&request.params).expect("object")),
            _ => ViewCommand::Unknown,
        };
        if let ViewCommand::Unsubscribe(reason) = &command {
            crate::stream_log::line(&format!(
                "unsubscribe {id} known={} views={} reason={}",
                self.tasks.contains_key(id),
                self.tasks.len(),
                text(&reason.reason)
            ));
            self.flows.remove(id);
            if let Some(task) = self.tasks.remove(id) {
                task.abort();
            }
            return Response::success(request.id.clone(), json!({"closed":true}));
        }
        if let ViewCommand::Next(credit) = &command {
            return if self.flows.get(id).is_some_and(|flow| {
                credit
                    .credit
                    .as_ref()
                    .and_then(TextField::text)
                    .is_some_and(|credit| flow.grant(credit))
            }) {
                Response::success(request.id.clone(), json!({"granted":true}))
            } else {
                crate::stream_log::line(&format!("invalid_credit {id}"));
                Response::failure(
                    request.id.clone(),
                    "invalid_credit",
                    "The event credit is no longer valid.",
                )
            };
        }
        if matches!(command, ViewCommand::Resume(_) | ViewCommand::Restart(_)) {
            let accepted = self.flows.get(id).is_some_and(|flow| match &command {
                ViewCommand::Restart(reason) => {
                    let reason = text(&reason.reason);
                    crate::stream_log::line(&format!("restart {id} reason={reason}"));
                    flow.restart(reason);
                    true
                }
                ViewCommand::Resume(resume) => resume
                    .path
                    .as_ref()
                    .and_then(TextField::text)
                    .is_some_and(|path| flow.resume(path)),
                ViewCommand::Unsubscribe(_)
                | ViewCommand::Next(_)
                | ViewCommand::Subscribe(_)
                | ViewCommand::Unknown => false,
            });
            return if accepted {
                Response::success(request.id.clone(), json!({"accepted":true}))
            } else {
                Response::failure(
                    request.id.clone(),
                    "invalid_subscription",
                    "The stream is not waiting for this request.",
                )
            };
        }
        if !matches!(command, ViewCommand::Subscribe(_))
            || self.tasks.contains_key(id)
            || self.tasks.len() >= 64
        {
            crate::stream_log::line(&format!(
                "refuse {id} method={} views={}",
                request.method,
                self.tasks.len()
            ));
            return Response::failure(
                request.id.clone(),
                "invalid_subscription",
                "The subscription already exists or the view limit was reached.",
            );
        }
        let ViewCommand::Subscribe(subscription) = command else {
            unreachable!()
        };
        let topic = text(&subscription.topic);
        if !matches!(
            topic,
            "projection"
                | "outbox"
                | "drafts"
                | "sse"
                | "microphone"
                | "voice"
                | "transcription"
                | "navigation"
        ) {
            return Response::failure(
                request.id.clone(),
                "invalid_subscription",
                "Unknown view topic.",
            );
        }
        let log_id = id.to_owned();
        let (id, topic, params) = (id.to_owned(), topic.to_owned(), request.params.clone());
        let workspace = text(&subscription.workspace).to_owned();
        let scope = text(&subscription.scope).to_owned();
        let flow = match subscription.flow_control {
            FlowControl::Missing | FlowControl::Enabled(false) => None,
            FlowControl::Enabled(true) if topic == "sse" => match crate::events::EventFlow::new() {
                Ok(flow) => Some(Arc::new(flow)),
                Err(error) => return Response::failure(request.id.clone(), "event_credit", error),
            },
            FlowControl::Enabled(_) | FlowControl::Other(_) => {
                return Response::failure(
                    request.id.clone(),
                    "invalid_subscription",
                    "Flow control requires a server event subscription.",
                );
            }
        };
        if let Some(flow) = &flow {
            self.flows.insert(id.clone(), flow.clone());
        }
        let (backend, store, changes, output) = (
            self.backend.clone(),
            self.store.clone(),
            self.changes.clone(),
            self.output.clone(),
        );
        let microphone = if topic == "microphone" || topic == "voice" {
            let attached = if topic == "voice" {
                crate::voice::attach(&params["session"])
            } else {
                crate::media::attach(&params["capture"])
            };
            match attached {
                Ok(value) => Some(value),
                Err(error) => return Response::failure(request.id.clone(), "media", error),
            }
        } else {
            None
        };
        self.tasks.insert(
            id.clone(),
            tokio::spawn(async move {
                if let Some(mut audio) = microphone {
                    while let Some(value) = audio.recv().await {
                        if output
                            .send(json!({"event":id,"value":value}))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                } else if topic == "transcription" || topic == "navigation" {
                    let mut events = if topic == "transcription" {
                        crate::speech::progress().subscribe()
                    } else {
                        crate::notifications::navigation().subscribe()
                    };
                    loop {
                        match events.recv().await {
                            Ok(value) => {
                                if output
                                    .send(json!({"event":id,"value":value}))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                } else if topic == "sse" {
                    crate::events::run_controlled(id, params, backend, output, flow).await;
                } else {
                    run(
                        id,
                        topic,
                        (workspace, scope),
                        backend,
                        store,
                        changes,
                        output,
                    )
                    .await;
                }
            }),
        );
        crate::stream_log::line(&format!("subscribe {} views={}", log_id, self.tasks.len()));
        Response::success(request.id.clone(), json!({"subscribed":true}))
    }
}

impl Drop for Views {
    fn drop(&mut self) {
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
    }
}

async fn send_changed(
    output: &mpsc::Sender<Value>,
    previous: &mut Option<Value>,
    value: Value,
) -> Result<(), ()> {
    if previous.as_ref() != Some(&value) {
        output.send(value.clone()).await.map_err(|_| ())?;
        *previous = Some(value);
    }
    Ok(())
}

/// Appends the operations that turn `old` into `new`. A path holds object keys
/// and array indexes. `{"p":path,"v":value}` sets a value; `{"p":path}` deletes a key.
/// Arrays of different length are replaced whole.
pub fn diff(old: &Value, new: &Value, path: &mut Vec<Value>, ops: &mut Vec<Value>) {
    if old == new {
        return;
    }
    match (old, new) {
        (Value::Object(before), Value::Object(after)) => {
            for (key, value) in after {
                path.push(Value::String(key.clone()));
                match before.get(key) {
                    Some(previous) => diff(previous, value, path, ops),
                    None => ops.push(json!({"p":path.clone(),"v":value})),
                }
                path.pop();
            }
            for key in before.keys().filter(|key| !after.contains_key(*key)) {
                path.push(Value::String(key.clone()));
                ops.push(json!({"p":path.clone()}));
                path.pop();
            }
        }
        (Value::Array(before), Value::Array(after)) if before.len() == after.len() => {
            for (index, (previous, value)) in before.iter().zip(after).enumerate() {
                path.push(json!(index));
                diff(previous, value, path, ops);
                path.pop();
            }
        }
        _ => ops.push(json!({"p":path.clone(),"v":new})),
    }
}

// Patch only the fields used by this view. Missing IDs and malformed unrelated
// fields must still be forwarded unchanged to the UI.
#[derive(Deserialize)]
#[serde(untagged)]
enum Sequence {
    Number(u64),
    Other(serde::de::IgnoredAny),
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenWorkspace<'a> {
    #[serde(borrow)]
    workspace_id: Option<TextField<'a>>,
}
#[derive(Deserialize)]
struct ProjectionDocument<'a> {
    seq: Option<Sequence>,
    #[serde(borrow)]
    payload: Option<TextField<'a>>,
    #[serde(default, rename = "_deleted")]
    deleted: FlowControl,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectionEnvelope<'a> {
    #[serde(borrow)]
    workspace_id: Option<TextField<'a>>,
    #[serde(borrow)]
    document: JsonObject<ProjectionDocument<'a>>,
}

/// Projection events carry the full document once, then patches against the
/// last document this subscription delivered. Large unchanged snapshots no
/// longer cross the native transport every refresh.
#[derive(Default)]
struct Patcher {
    last: Option<(String, u64, Value)>,
}
impl Patcher {
    fn event(&mut self, id: &str, value: Value) -> Option<Value> {
        let Some(envelope) = decode::<ProjectionEnvelope>(&value) else {
            self.last = None;
            return Some(json!({"event":id,"value":value}));
        };
        let workspace = text(&envelope.workspace_id).to_owned();
        let document = envelope.document.0;
        let (Some(Sequence::Number(seq)), Some(payload)) = (
            document.seq,
            document.payload.as_ref().and_then(TextField::text),
        ) else {
            self.last = None;
            return Some(json!({"event":id,"value":value}));
        };
        if matches!(document.deleted, FlowControl::Enabled(true)) {
            self.last = None;
            return Some(json!({"event":id,"value":value}));
        }
        if let Some((last_workspace, last_seq, _)) = &self.last
            && *last_workspace == workspace
            && *last_seq == seq
        {
            return None;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(payload) else {
            self.last = None;
            return Some(json!({"event":id,"value":value}));
        };
        let event = match self.last.take() {
            Some((last_workspace, base, old)) if last_workspace == workspace => {
                let mut ops = vec![];
                diff(&old, &parsed, &mut vec![], &mut ops);
                let patch =
                    json!({"workspaceId":workspace,"patch":{"base":base,"seq":seq,"ops":ops}});
                if serde_json::to_vec(&patch)
                    .map(|b| b.len() * 2 < payload.len())
                    .unwrap_or(false)
                {
                    json!({"event":id,"value":patch})
                } else {
                    json!({"event":id,"value":value})
                }
            }
            _ => json!({"event":id,"value":value}),
        };
        self.last = Some((workspace, seq, parsed));
        Some(event)
    }
}

async fn run(
    id: String,
    topic: String,
    projection: (String, String),
    backend: Arc<Result<Backend, String>>,
    store: StoreSender,
    mut changes: watch::Receiver<u64>,
    output: mpsc::Sender<Value>,
) {
    let (mut workspace, scope) = projection;
    let mut previous = None;
    let mut patcher = Patcher::default();
    let scope = scope.as_str();
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await; // Consume the immediate tick before the initial refresh.
    let mut refresh = true;
    let mut network_error: Option<crate::protocol::CommandError> = None;
    loop {
        let result = async {
            if workspace.is_empty() {
                let backend = backend
                    .as_ref()
                    .as_ref()
                    .map_err(|e| sync::error("backend_config", e))?;
                let opened = sync::open(backend, &store).await?;
                workspace = decode::<OpenWorkspace>(&opened)
                    .map(|value| text(&value.workspace_id))
                    .unwrap_or("")
                    .to_owned();
            }
            let value = if topic == "projection" {
                // Publish cached data before the first HTTP wait.
                let cached = state(
                    &store,
                    "projection.get",
                    json!({"workspace":workspace,"scope":scope}),
                )
                .await
                .map_err(|e| sync::error("storage_error", e))?;
                let cached_error = decode::<ProjectionDocument>(&cached)
                    .and_then(|value| value.payload.and_then(|field| field.text()))
                    .and_then(sync::projection_error);
                if previous.is_none() && !cached.is_null() && cached_error.is_none() {
                    let event =
                        patcher.event(&id, json!({"workspaceId":workspace,"document":cached}));
                    async {
                        match event {
                            Some(event) => output.send(event).await.map_err(|_| ()),
                            None => Ok(()),
                        }
                    }
                    .await
                    .map_err(|_| sync::error("disconnected", "The UI disconnected."))?;
                }
                let document = if refresh {
                    let backend = backend
                        .as_ref()
                        .as_ref()
                        .map_err(|e| sync::error("backend_config", e))?;
                    match sync::pull(backend, &store, &workspace, scope).await {
                        Ok(value) => {
                            network_error = None;
                            value
                        }
                        Err(error) => {
                            network_error = Some(error.clone());
                            return Err(error);
                        }
                    }
                } else {
                    if let Some(error) = network_error.as_ref().or(cached_error.as_ref()) {
                        return Err(error.clone());
                    }
                    cached
                };
                json!({"workspaceId":workspace,"document":document})
            } else if topic == "drafts" {
                state(&store, "drafts.view", json!({"workspace":workspace}))
                    .await
                    .map_err(|e| sync::error("storage_error", e))?
            } else {
                let entries = state(&store, "outbox.list", json!({"workspace":workspace}))
                    .await
                    .map_err(|e| sync::error("storage_error", e))?;
                json!({"workspaceId":workspace,"entries":entries})
            };
            Ok::<_, crate::protocol::CommandError>(value)
        }
        .await;
        let sent = match result {
            Ok(value) if topic == "projection" => match patcher.event(&id, value) {
                Some(event) => output.send(event).await.map_err(|_| ()),
                None => Ok(()),
            },
            result => {
                // A later value must reach the UI after an error clears its state.
                patcher.last = None;
                let event = match result {
                    Ok(value) => json!({"event":id,"value":value}),
                    Err(error) => json!({"event":id,"error":error}),
                };
                send_changed(&output, &mut previous, event).await
            }
        };
        if sent.is_err() {
            return;
        }
        tokio::select! {
            _=interval.tick()=>refresh=true,
            result=changes.changed()=>{ if result.is_err(){return;} refresh=false; },
        }
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    fn request(method: &str, params: Value) -> Request {
        Request {
            id: "flow-test".into(),
            method: method.into(),
            params,
        }
    }
    fn views() -> Views {
        let (store, _) = mpsc::channel(1);
        let (_, changes) = watch::channel(0);
        let (output, _) = mpsc::channel(16);
        Views::new(Arc::new(Err("No backend".into())), store, changes, output)
    }
    #[tokio::test]
    async fn credits_are_one_use_and_bound_to_connection_and_subscription_incarnation() {
        let mut owner = views();
        let mut foreign = views();
        let subscribe = request(
            "view.subscribe",
            json!({"subscription":"one","topic":"sse","path":"/api/sync/stream","flowControl":true}),
        );
        assert!(owner.command(&subscribe).error.is_none());
        assert!(foreign.command(&subscribe).error.is_none());
        let token = owner.flows["one"].credit();
        let next = request("view.next", json!({"subscription":"one","credit":token}));
        assert!(foreign.command(&next).error.is_some());
        assert!(
            owner
                .command(&request(
                    "view.next",
                    json!({"subscription":"other","credit":token})
                ))
                .error
                .is_some()
        );
        assert!(owner.command(&next).error.is_none());
        assert!(owner.command(&next).error.is_some());
        assert!(
            owner
                .command(&request("view.unsubscribe", json!({"subscription":"one"})))
                .error
                .is_none()
        );
        assert!(owner.command(&next).error.is_some());
        assert!(owner.command(&subscribe).error.is_none());
        assert!(owner.command(&next).error.is_some());
    }
    #[tokio::test]
    async fn resume_and_restart_reach_only_this_connections_waiting_stream() {
        let mut owner = views();
        let mut foreign = views();
        let subscribe = request(
            "view.subscribe",
            json!({"subscription":"one","topic":"sse","path":"/api/sync/stream","flowControl":true}),
        );
        assert!(owner.command(&subscribe).error.is_none());
        let restart = request(
            "view.restart",
            json!({"subscription":"one","reason":"invalid-frame"}),
        );
        let resume = request(
            "view.resume",
            json!({"subscription":"one","path":"/api/sync/stream?next"}),
        );
        assert!(foreign.command(&restart).error.is_some());
        assert!(owner.command(&restart).error.is_none());
        // The stream has not asked for a path, so an early answer is refused.
        assert!(owner.command(&resume).error.is_some());
        owner.flows["one"].expect_path();
        assert!(foreign.command(&resume).error.is_some());
        assert!(owner.command(&resume).error.is_none());
        assert!(owner.command(&resume).error.is_some());
    }
    #[tokio::test]
    async fn projection_run_emits_cached_document_before_backend_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite3");
        let workspace = "a".repeat(32);
        let (sender, mut receiver) = mpsc::channel::<crate::delivery::StoreCommand>(8);
        let worker = std::thread::spawn(move || {
            let mut store = crate::store::Store::open(&path).unwrap();
            while let Some(command) = receiver.blocking_recv() {
                command.execute(&mut store);
            }
        });
        let document = json!({"id":"state","seq":1,"payload":"{\"cached\":true}"});
        state(
            &sender,
            "projection.put",
            json!({"workspace":workspace,"scope":"state","document":document}),
        )
        .await
        .unwrap();
        let (_changes, changes) = watch::channel(0);
        let (output, mut received) = mpsc::channel(8);
        let task = tokio::spawn(run(
            "one".into(),
            "projection".into(),
            (workspace, "state".into()),
            Arc::new(Err("No backend".into())),
            sender.clone(),
            changes,
            output,
        ));
        let first = received.recv().await.unwrap();
        assert_eq!(first["value"]["document"]["payload"], document["payload"]);
        let second = received.recv().await.unwrap();
        assert_eq!(second["error"]["code"], "backend_config");
        task.abort();
        drop(task.await);
        drop(sender);
        worker.join().unwrap();
    }
    #[tokio::test]
    async fn malformed_commands_leave_subscription_and_credit_unchanged() {
        let mut owner = views();
        let subscribe = request(
            "view.subscribe",
            json!({"subscription":"one","topic":"sse","flowControl":true}),
        );
        assert!(owner.command(&subscribe).error.is_none());
        let token = owner.flows["one"].credit();
        for params in [
            json!({"subscription":"one","credit":false}),
            json!(["one", token]),
        ] {
            assert!(owner.command(&request("view.next", params)).error.is_some());
        }
        assert!(
            owner
                .command(&request(
                    "view.next",
                    json!({"subscription":"one","credit":token})
                ))
                .error
                .is_none()
        );
        assert!(
            owner
                .command(&request(
                    "view.unsubscribe",
                    json!({"subscription":"one","reason":false})
                ))
                .error
                .is_none()
        );
        for flow in [json!(null), json!(0), json!([])] {
            assert!(
                owner
                    .command(&request(
                        "view.subscribe",
                        json!({"subscription":"bad","topic":"sse","flowControl":flow})
                    ))
                    .error
                    .is_some()
            );
            assert!(!owner.tasks.contains_key("bad"));
        }
    }
    #[tokio::test]
    async fn push_views_remain_push_and_flow_control_is_sse_only() {
        let mut owner = views();
        assert!(
            owner
                .command(&request(
                    "view.subscribe",
                    json!({"subscription":"push","topic":"sse"})
                ))
                .error
                .is_none()
        );
        assert!(!owner.flows.contains_key("push"));
        assert!(
            owner
                .command(&request(
                    "view.next",
                    json!({"subscription":"push","credit":"guess"})
                ))
                .error
                .is_some()
        );
        for params in [
            json!({"subscription":"bad","topic":"outbox","flowControl":true}),
            json!({"subscription":"bad","topic":"sse","flowControl":"true"}),
        ] {
            assert!(
                owner
                    .command(&request("view.subscribe", params))
                    .error
                    .is_some()
            );
            assert!(!owner.tasks.contains_key("bad"));
        }
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;
    fn apply(root: &mut Value, ops: &[Value]) {
        for op in ops {
            let path = op["p"].as_array().unwrap();
            if path.is_empty() {
                *root = op["v"].clone();
                continue;
            }
            let mut target = &mut *root;
            for key in &path[..path.len() - 1] {
                target = match key {
                    Value::String(k) => &mut target[k.as_str()],
                    other @ Value::Null
                    | other @ Value::Bool(_)
                    | other @ Value::Number(_)
                    | other @ Value::Array(_)
                    | other @ Value::Object(_) => &mut target[other.as_u64().unwrap() as usize],
                };
            }
            match (&path[path.len() - 1], op.get("v")) {
                (Value::String(k), Some(v)) => target[k.as_str()] = v.clone(),
                (Value::String(k), None) => {
                    target.as_object_mut().unwrap().remove(k);
                }
                (index, Some(v)) => target[index.as_u64().unwrap() as usize] = v.clone(),
                _ => panic!("delete by index"),
            }
        }
    }
    #[test]
    fn patches_rebuild_the_new_document_and_stay_small() {
        let old = json!({"nodes":[{"id":"a","events":[1,2],"at":1},{"id":"b","at":1}],"gone":true,"runtime":{"x":"Ж"}});
        let new = json!({"nodes":[{"id":"a","events":[1,2,3],"at":2},{"id":"b","at":1}],"runtime":{"x":"Ж","y":[]}});
        let mut ops = vec![];
        diff(&old, &new, &mut vec![], &mut ops);
        let mut rebuilt = old.clone();
        apply(&mut rebuilt, &ops);
        assert_eq!(rebuilt, new);
        assert_eq!(ops.len(), 4, "{ops:?}");
        let mut none = vec![];
        diff(&new, &new, &mut vec![], &mut none);
        assert!(none.is_empty());
    }
    #[test]
    fn malformed_projection_envelopes_are_forwarded_unchanged() {
        let mut patcher = Patcher::default();
        for value in [
            json!(null),
            json!({"document":[]}),
            json!({"document":{"seq":false,"payload":"{}"}}),
            json!({"document":{"seq":1,"payload":false}}),
        ] {
            assert_eq!(
                patcher.event("id", value.clone()).unwrap(),
                json!({"event":"id","value":value})
            );
            assert!(patcher.last.is_none());
        }
        // Only fields consumed by patching are required, matching historical views.
        assert!(
            patcher
                .event(
                    "id",
                    json!({"document":{"seq":1,"payload":"{}","_deleted":"no"}})
                )
                .is_some()
        );
        assert!(patcher.last.is_some());
    }
    #[test]
    fn patcher_sends_full_then_patch_skips_same_seq_and_resets_after_delete() {
        let document = |seq: u64, payload: Value| json!({"workspaceId":"w","document":{"id":"state","seq":seq,"payload":payload.to_string()}});
        let big: Vec<Value> = (0..200)
            .map(|i| json!({"id":i,"text":"x".repeat(50)}))
            .collect();
        let mut patcher = Patcher::default();
        let first = patcher
            .event("v", document(1, json!({"nodes":big})))
            .unwrap();
        assert!(first["value"]["document"].is_object());
        assert!(
            patcher
                .event("v", document(1, json!({"nodes":big})))
                .is_none()
        );
        let mut changed = big.clone();
        changed[3]["text"] = json!("changed");
        let second = patcher
            .event("v", document(2, json!({"nodes":changed})))
            .unwrap();
        assert_eq!(second["value"]["patch"]["base"], 1);
        assert_eq!(
            second["value"]["patch"]["ops"],
            json!([{"p":["nodes",3,"text"],"v":"changed"}])
        );
        let deleted = json!({"workspaceId":"w","document":{"id":"state","seq":3,"payload":"{}","_deleted":true}});
        assert!(patcher.event("v", deleted).unwrap()["value"]["document"]["_deleted"] == true);
        assert!(
            patcher
                .event("v", document(4, json!({"nodes":changed})))
                .unwrap()["value"]["document"]
                .is_object()
        );
        // A small document is cheaper to send whole than as a patch.
        let mut small = Patcher::default();
        small.event("v", document(1, json!({"a":1})));
        assert!(
            small.event("v", document(2, json!({"a":2}))).unwrap()["value"]["document"].is_object()
        );
    }
}
