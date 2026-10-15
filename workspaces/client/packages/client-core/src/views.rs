//! UI subscriptions contain values and errors. Rust owns refresh and retry policy.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    protocol::{Request, Response},
    sync,
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

pub struct Views {
    tasks: HashMap<String, JoinHandle<()>>,
    backend: Arc<Result<Backend, String>>,
    store: StoreSender,
    changes: watch::Receiver<u64>,
    output: mpsc::Sender<Value>,
    terminals: crate::terminal::Terminals,
}

impl Views {
    pub fn new(
        backend: Arc<Result<Backend, String>>,
        store: StoreSender,
        changes: watch::Receiver<u64>,
        output: mpsc::Sender<Value>,
        terminals: crate::terminal::Terminals,
    ) -> Self {
        Self {
            tasks: HashMap::new(),
            backend,
            store,
            changes,
            output,
            terminals,
        }
    }
    pub fn command(&mut self, request: &Request) -> Response {
        let Some(id) = request.params["subscription"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 128)
        else {
            return Response::failure(
                request.id.clone(),
                "invalid_subscription",
                "A subscription identity is required.",
            );
        };
        if request.method == "view.unsubscribe" {
            if let Some(task) = self.tasks.remove(id) {
                task.abort();
            }
            return Response::success(request.id.clone(), json!({"closed":true}));
        }
        if request.method != "view.subscribe"
            || self.tasks.contains_key(id)
            || self.tasks.len() >= 64
        {
            return Response::failure(
                request.id.clone(),
                "invalid_subscription",
                "The subscription already exists or the view limit was reached.",
            );
        }
        let topic = request.params["topic"].as_str().unwrap_or("");
        if !matches!(
            topic,
            "projection"
                | "outbox"
                | "drafts"
                | "sse"
                | "terminal"
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
        let (id, topic, params) = (id.to_owned(), topic.to_owned(), request.params.clone());
        let (backend, store, changes, output) = (
            self.backend.clone(),
            self.store.clone(),
            self.changes.clone(),
            self.output.clone(),
        );
        let terminal = if topic == "terminal" {
            match self
                .terminals
                .watch(params["handle"].as_str().unwrap_or(""))
            {
                Ok(value) => Some(value),
                Err(error) => return Response::failure(request.id.clone(), "terminal", error),
            }
        } else {
            None
        };
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
                } else if let Some(mut terminal) = terminal {
                    loop {
                        let value = terminal.borrow_and_update().clone();
                        if !value.is_null()
                            && output
                                .send(json!({"event":id,"value":value}))
                                .await
                                .is_err()
                        {
                            break;
                        }
                        if terminal.changed().await.is_err() {
                            break;
                        }
                    }
                } else if topic == "sse" {
                    crate::events::run(id, params, backend, output).await;
                } else {
                    run(id, topic, params, backend, store, changes, output).await;
                }
            }),
        );
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

/// Projection events carry the full document once, then patches against the
/// last document this subscription delivered. Large unchanged snapshots no
/// longer cross the native transport every refresh.
#[derive(Default)]
struct Patcher {
    last: Option<(String, u64, Value)>,
}
impl Patcher {
    fn event(&mut self, id: &str, value: Value) -> Option<Value> {
        let workspace = value["workspaceId"].as_str().unwrap_or("").to_owned();
        let document = &value["document"];
        let (Some(seq), Some(payload)) = (document["seq"].as_u64(), document["payload"].as_str())
        else {
            self.last = None;
            return Some(json!({"event":id,"value":value}));
        };
        if document["_deleted"] == true {
            self.last = None;
            return Some(json!({"event":id,"value":value}));
        }
        if let Some((last_workspace, last_seq, _)) = &self.last {
            if *last_workspace == workspace && *last_seq == seq {
                return None;
            }
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
    params: Value,
    backend: Arc<Result<Backend, String>>,
    store: StoreSender,
    mut changes: watch::Receiver<u64>,
    output: mpsc::Sender<Value>,
) {
    let mut previous = None;
    let mut patcher = Patcher::default();
    let mut workspace = params["workspace"].as_str().unwrap_or("").to_owned();
    let scope = params["scope"].as_str().unwrap_or("");
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
                workspace = sync::open(backend, &store).await?["workspaceId"]
                    .as_str()
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
                let cached_error = cached["payload"].as_str().and_then(sync::projection_error);
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
        let event = match result {
            Ok(value) => json!({"event":id,"value":value}),
            Err(error) => json!({"event":id,"error":error}),
        };
        let sent = if topic == "projection" && event.get("value").is_some() {
            match patcher.event(&id, event["value"].clone()) {
                Some(event) => output.send(event).await.map_err(|_| ()),
                None => Ok(()),
            }
        } else {
            // A later value must reach the UI after an error clears its state.
            patcher.last = None;
            send_changed(&output, &mut previous, event).await
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
                    other => &mut target[other.as_u64().unwrap() as usize],
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
