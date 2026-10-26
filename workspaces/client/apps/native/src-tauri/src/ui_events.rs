//! Fixed UI events. Native event payloads are checked before channel delivery.
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tauri::{
    AppHandle, Listener, Manager, WebviewWindow,
    ipc::{Channel, JavaScriptChannelId},
};

/// The complete UI event allowlist. No renderer input selects a topic.
pub(crate) const CONNECT_METHOD: &str = "ui.events.connect";
pub(crate) const DISCONNECT_METHOD: &str = "ui.events.disconnect";
pub(crate) const TOPICS: [&str; 4] = [
    "studio-client-event",
    "studio-close-request",
    "studio-desktop-error",
    "studio-native-navigate",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    callback: JavaScriptChannelId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Disconnect {
    lease_id: String,
}

struct Leases<T>(HashMap<String, (String, T)>);
impl<T> Default for Leases<T> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}
impl<T> Leases<T> {
    fn replace(&mut self, window: &str, id: String, value: T) -> Option<T> {
        self.0
            .insert(window.into(), (id, value))
            .map(|(_, value)| value)
    }

    fn take_exact(&mut self, window: &str, id: &str) -> Option<T> {
        if self.0.get(window).is_some_and(|(current, _)| current == id) {
            self.0.remove(window).map(|(_, value)| value)
        } else {
            None
        }
    }
}

fn envelope(topic: &str, payload: &str) -> Option<Value> {
    if !TOPICS.contains(&topic) {
        return None;
    }
    let payload: Value = serde_json::from_str(payload).ok()?;
    let object = payload.as_object()?;
    let keys = |allowed: &[&str]| object.keys().all(|key| allowed.contains(&key.as_str()));
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    };
    let optional_text = |key: &str| !object.contains_key(key) || text(key);
    let valid = if topic == TOPICS[0] {
        // Committed view and error values retain their existing inner owners.
        keys(&["event", "value", "error"])
            && text("event")
            && (object.contains_key("value") ^ object.contains_key("error"))
    } else if topic == TOPICS[1] {
        keys(&["id", "action"])
            && object.get("id").and_then(Value::as_u64).is_some()
            && (!object.contains_key("action")
                || matches!(payload["action"].as_str(), Some("close" | "reload")))
    } else if topic == TOPICS[2] {
        keys(&["message"]) && payload["message"].is_string()
    } else {
        keys(&["agentId", "section", "serverId", "itemId"])
            && text("agentId")
            && payload["section"] == "messages"
            && optional_text("serverId")
            && optional_text("itemId")
    };
    valid.then(|| json!({"event":topic,"payload":payload}))
}

struct Delivery {
    active: Mutex<bool>,
    channel: Channel<Value>,
}
type DeliverySlot = Arc<Mutex<Option<Delivery>>>;

struct Connection {
    delivery: DeliverySlot,
    listeners: Vec<tauri::EventId>,
}
impl Connection {
    fn retire(self, app: &AppHandle) {
        if let Ok(mut slot) = self.delivery.lock() {
            *slot = None;
        }
        // These IDs belong only to this fixed UI relay, never to model work.
        for listener in self.listeners {
            app.unlisten(listener);
        }
    }
}

/// App-managed state belongs to the host's one locked native profile.
#[derive(Default)]
pub(crate) struct UiEvents(Mutex<Leases<Connection>>);

fn authorized(window: &WebviewWindow) -> Result<(), String> {
    crate::trusted(window)?;
    if window.try_state::<crate::ProfileLock>().is_none() {
        return Err("The native profile lock is unavailable.".into());
    }
    Ok(())
}

fn replace_delivery(slot: &DeliverySlot, channel: Channel<Value>) -> Result<(), String> {
    *slot.lock().map_err(|_| "The UI event lease stopped.")? = Some(Delivery {
        active: Mutex::new(true),
        channel,
    });
    Ok(())
}

fn deliver(slot: &DeliverySlot, topic: &str, payload: &str) {
    let Ok(slot) = slot.lock() else {
        return;
    };
    if let Some(delivery) = slot.as_ref() {
        // Keep the slot locked through send. A reconnect cannot send to both
        // the old and new channel while one native event is in this callback.
        let _ = forward(&delivery.active, &delivery.channel, topic, payload);
    }
}

pub(crate) fn connect(window: &WebviewWindow, params: &Value) -> Result<Value, String> {
    authorized(window)?;
    let params: Connect =
        serde_json::from_value(params.clone()).map_err(|_| "Invalid UI event connection.")?;
    let id = studio_client_core::server_keys::random_id()?;
    let channel = params
        .callback
        .channel_on::<_, Value>(window.as_ref().clone());
    let state = window
        .try_state::<UiEvents>()
        .ok_or("The UI event broker is unavailable.")?;
    let mut leases = state.0.lock().map_err(|_| "The UI event broker stopped.")?;
    if let Some((current, connection)) = leases.0.get_mut(window.label()) {
        replace_delivery(&connection.delivery, channel)?;
        *current = id.clone();
    } else {
        let delivery = Arc::new(Mutex::new(None));
        replace_delivery(&delivery, channel)?;
        let listeners = TOPICS
            .into_iter()
            .map(|topic| {
                let delivery = delivery.clone();
                // A native window target is required. Never use listen_any here.
                window.listen(topic, move |event| {
                    deliver(&delivery, topic, event.payload())
                })
            })
            .collect();
        let _ = leases.replace(
            window.label(),
            id.clone(),
            Connection {
                delivery,
                listeners,
            },
        );
    }
    Ok(json!({"leaseId":id}))
}

pub(crate) fn disconnect(window: &WebviewWindow, params: &Value) -> Result<Value, String> {
    authorized(window)?;
    let params: Disconnect =
        serde_json::from_value(params.clone()).map_err(|_| "Invalid UI event disconnection.")?;
    if params.lease_id.is_empty() {
        return Err("Invalid UI event lease identity.".into());
    }
    let state = window
        .try_state::<UiEvents>()
        .ok_or("The UI event broker is unavailable.")?;
    let mut leases = state.0.lock().map_err(|_| "The UI event broker stopped.")?;
    let connection = leases.take_exact(window.label(), &params.lease_id);
    let disconnected = connection.is_some();
    if let Some(connection) = connection {
        connection.retire(window.app_handle());
    }
    Ok(json!({"disconnected":disconnected}))
}

/// Only native main-document navigation and window destruction call this hook.
/// A renderer must use disconnect with its exact lease, never this label input.
pub(crate) fn retire_window(app: &AppHandle, window: &str) -> Result<bool, String> {
    let Some(state) = app.try_state::<UiEvents>() else {
        return Ok(false);
    };
    let mut leases = state.0.lock().map_err(|_| "The UI event broker stopped.")?;
    let connection = leases.0.remove(window).map(|(_, connection)| connection);
    let retired = connection.is_some();
    if let Some(connection) = connection {
        connection.retire(app);
    }
    Ok(retired)
}

fn forward(
    active: &Mutex<bool>,
    channel: &Channel<Value>,
    topic: &str,
    payload: &str,
) -> Result<bool, String> {
    let mut active = active.lock().map_err(|_| "The UI event lease stopped.")?;
    if !*active {
        return Ok(false);
    }
    let Some(value) = envelope(topic, payload) else {
        return Ok(false);
    };
    if channel.send(value).is_err() {
        *active = false;
        return Err("The UI event channel closed.".into());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tauri::ipc::InvokeResponseBody;

    #[test]
    fn native_ui_events_preserve_the_four_existing_payload_contracts() {
        let samples = [
            (
                TOPICS[0],
                json!({"event":"context:subscription","value":{"document":{"id":"original"},"cursor":"7"}}),
            ),
            (
                TOPICS[0],
                json!({"event":"context:subscription","error":{"code":"offline","message":"The server is unavailable."}}),
            ),
            (TOPICS[1], json!({"id":9,"action":"reload"})),
            (TOPICS[1], json!({"id":10})),
            (TOPICS[2], json!({"message":"The native operation failed."})),
            (
                TOPICS[3],
                json!({"agentId":"agent","section":"messages","serverId":"paired","itemId":"message"}),
            ),
        ];
        for (topic, payload) in samples {
            assert_eq!(
                envelope(topic, &payload.to_string()),
                Some(json!({"event":topic,"payload":payload}))
            );
        }
    }

    #[test]
    fn native_ui_events_filter_raw_events_before_channel_send() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = sent.clone();
        let channel = Channel::<Value>::new(move |body| {
            let InvokeResponseBody::Json(body) = body else {
                panic!("UI events must use JSON");
            };
            sink.lock()
                .unwrap()
                .push(serde_json::from_str::<Value>(&body).unwrap());
            Ok(())
        });
        let active = Mutex::new(true);
        for topic in [
            "tauri://drag-enter",
            "tauri://drag-drop",
            "file-drop",
            "arbitrary",
        ] {
            assert!(
                !forward(
                    &active,
                    &channel,
                    topic,
                    r#"{"paths":["/private/owned-fixture"]}"#
                )
                .unwrap()
            );
        }
        for topic in TOPICS {
            assert!(
                !forward(
                    &active,
                    &channel,
                    topic,
                    r#"{"paths":["/private/owned-fixture"]}"#
                )
                .unwrap()
            );
        }
        assert!(sent.lock().unwrap().is_empty());
        let original = json!({"event":"context:original","value":{"id":"original-request","status":"unknown"}});
        assert!(forward(&active, &channel, TOPICS[0], &original.to_string()).unwrap());
        assert_eq!(
            *sent.lock().unwrap(),
            vec![json!({"event":TOPICS[0],"payload":original})]
        );
    }

    #[test]
    fn native_ui_events_reject_malformed_or_extended_payloads() {
        let cases = [
            (
                TOPICS[0],
                json!({"event":"context","value":1,"error":"conflict"}),
            ),
            (
                TOPICS[0],
                json!({"event":"context","value":1,"paths":["/private/owned-fixture"]}),
            ),
            (TOPICS[0], json!({"event":"","value":1})),
            (TOPICS[1], json!({"id":-1,"action":"close"})),
            (TOPICS[1], json!({"id":1,"action":"reset"})),
            (
                TOPICS[2],
                json!({"message":"error","paths":["/private/owned-fixture"]}),
            ),
            (
                TOPICS[3],
                json!({"agentId":"a","section":"files","path":"/private/owned-fixture"}),
            ),
        ];
        for (topic, payload) in cases {
            assert!(envelope(topic, &payload.to_string()).is_none());
        }
        assert!(envelope(TOPICS[0], "invalid JSON").is_none());
    }

    #[test]
    fn native_ui_events_stale_disconnect_cannot_remove_a_new_window_lease() {
        let mut leases = Leases::<Vec<u32>>::default();
        assert!(
            leases
                .replace("main", "old".into(), vec![1, 2, 3, 4])
                .is_none()
        );
        assert_eq!(
            leases.replace("main", "new".into(), vec![5, 6, 7, 8]),
            Some(vec![1, 2, 3, 4])
        );
        assert!(leases.take_exact("main", "old").is_none());
        assert!(leases.take_exact("other", "new").is_none());
        assert_eq!(leases.take_exact("main", "new"), Some(vec![5, 6, 7, 8]));
        assert!(leases.take_exact("main", "new").is_none());
    }

    #[test]
    fn native_ui_events_retired_callbacks_cannot_forward_after_replacement() {
        let channel = Channel::<Value>::new(|_| panic!("A retired UI lease cannot send"));
        assert!(
            !forward(
                &Mutex::new(false),
                &channel,
                TOPICS[1],
                r#"{"id":7,"action":"close"}"#
            )
            .unwrap()
        );
    }

    #[test]
    fn native_ui_events_reconnect_replaces_only_the_ui_delivery_slot() {
        fn sink(sent: Arc<Mutex<Vec<Value>>>) -> Channel<Value> {
            Channel::new(move |body| {
                let InvokeResponseBody::Json(body) = body else {
                    panic!("UI events must use JSON");
                };
                sent.lock()
                    .unwrap()
                    .push(serde_json::from_str(&body).unwrap());
                Ok(())
            })
        }
        let old = Arc::new(Mutex::new(Vec::new()));
        let current = Arc::new(Mutex::new(Vec::new()));
        let slot = Arc::new(Mutex::new(None));
        replace_delivery(&slot, sink(old.clone())).unwrap();
        deliver(&slot, TOPICS[1], r#"{"id":1,"action":"close"}"#);
        replace_delivery(&slot, sink(current.clone())).unwrap();
        deliver(&slot, TOPICS[1], r#"{"id":2,"action":"reload"}"#);
        assert_eq!(old.lock().unwrap().len(), 1);
        assert_eq!(current.lock().unwrap().len(), 1);
        assert_eq!(current.lock().unwrap()[0]["payload"]["id"], 2);
        // An already-captured listener cannot reach the retired channel.
        *slot.lock().unwrap() = None;
        deliver(&slot, TOPICS[1], r#"{"id":3,"action":"close"}"#);
        assert_eq!(old.lock().unwrap().len(), 1);
        assert_eq!(current.lock().unwrap().len(), 1);
    }

    #[test]
    fn native_ui_events_connect_and_disconnect_accept_only_their_exact_fields() {
        assert!(serde_json::from_value::<Connect>(json!({"callback":"__CHANNEL__:7"})).is_ok());
        for extra in ["event", "topic", "context", "window"] {
            let mut value = json!({"callback":"__CHANNEL__:7"});
            value[extra] = json!("foreign");
            assert!(serde_json::from_value::<Connect>(value).is_err());
        }
        assert!(serde_json::from_value::<Connect>(json!({"callback":7})).is_err());
        assert!(serde_json::from_value::<Disconnect>(json!({"leaseId":"original"})).is_ok());
        assert!(
            serde_json::from_value::<Disconnect>(json!({"leaseId":"original","window":"other"}))
                .is_err()
        );
    }
}
