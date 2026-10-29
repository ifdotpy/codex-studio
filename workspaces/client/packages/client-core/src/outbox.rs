//! Message queue transitions ported from web/src/sync/send.ts.
use crate::protocol::{Request, Response};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub workspace: String,
    pub body: Value,
    pub status: String,
    pub attempted: Option<bool>,
    pub created: u64,
    pub display_pending: bool,
    pub attachments: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Claim {
    pub claimed: bool,
    pub entry: Entry,
}

// These are decision projections; Entry keeps the original body and receipt.
#[derive(Default, Deserialize)]
struct BodyIdentity {
    #[serde(default, deserialize_with = "loose_string")]
    id: Option<String>,
    #[serde(default, deserialize_with = "present")]
    room: Option<Value>,
    #[serde(default, deserialize_with = "loose_string")]
    text: Option<String>,
}
#[derive(Default, Deserialize)]
struct ReceiptObservation {
    #[serde(default, deserialize_with = "loose_string")]
    id: Option<String>,
    #[serde(default, deserialize_with = "loose_string")]
    status: Option<String>,
    #[serde(default, deserialize_with = "present")]
    room: Option<Value>,
    #[serde(default, deserialize_with = "loose_string")]
    text: Option<String>,
    #[serde(default, deserialize_with = "object_field")]
    deliveries: bool,
    #[serde(default, deserialize_with = "loose_string")]
    error: Option<String>,
}
fn loose_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Value::deserialize(d)?.as_str().map(str::to_owned))
}
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}
fn object_field<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Value::deserialize(d)?.is_object())
}
fn observation<T: serde::de::DeserializeOwned + Default>(value: &Value) -> T {
    if value.is_object() {
        T::deserialize(value).unwrap_or_default()
    } else {
        T::default()
    }
}
impl BodyIdentity {
    fn room(&self) -> Result<&str, String> {
        self.room
            .as_ref()
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 512)
            .ok_or_else(|| "room is required.".into())
    }
    fn id(&self) -> Result<&str, String> {
        self.id
            .as_deref()
            .filter(|value| !value.is_empty() && value.len() <= 512)
            .ok_or_else(|| "id is required.".into())
    }
}
impl ReceiptObservation {
    fn confirmed(&self) -> bool {
        matches!(
            self.status.as_deref(),
            Some("delivered" | "accepted" | "sent")
        )
    }
}

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS client_outbox (
        workspace TEXT NOT NULL, id TEXT NOT NULL, room TEXT NOT NULL,
        payload TEXT NOT NULL, in_flight INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(workspace,id));",
    )?;
    // The previous process no longer owns an HTTP wait. Keep attempted=true so
    // restart cannot turn an uncertain delivery into a safe local cancellation.
    connection.execute(
        "UPDATE client_outbox SET in_flight=0 WHERE in_flight<>0",
        [],
    )?;
    Ok(())
}

fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .ok_or_else(|| format!("{name} is required."))
}

pub fn list(connection: &Connection, workspace: &str) -> Result<Vec<Entry>, String> {
    let mut statement = connection
        .prepare("SELECT payload FROM client_outbox WHERE workspace=?1")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([workspace], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut entries = Vec::<Entry>::new();
    for row in rows {
        entries.push(
            serde_json::from_str(&row.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
        );
    }
    entries.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
    Ok(entries)
}

fn load(connection: &Connection, workspace: &str, id: &str) -> Result<(Entry, bool), String> {
    let row: Option<(String, bool)> = connection
        .query_row(
            "SELECT payload,in_flight FROM client_outbox WHERE workspace=?1 AND id=?2",
            params![workspace, id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let (payload, in_flight) = row.ok_or("This message is no longer on this device.")?;
    Ok((
        serde_json::from_str(&payload).map_err(|e| e.to_string())?,
        in_flight,
    ))
}

fn save(connection: &Connection, entry: &Entry, in_flight: bool) -> Result<(), String> {
    let identity: BodyIdentity = observation(&entry.body);
    let room = identity.room()?;
    connection.execute("INSERT INTO client_outbox(workspace,id,room,payload,in_flight) VALUES(?1,?2,?3,?4,?5)
        ON CONFLICT(workspace,id) DO UPDATE SET payload=excluded.payload,in_flight=excluded.in_flight",
        params![entry.workspace,entry.id,room,serde_json::to_string(entry).map_err(|e| e.to_string())?,in_flight])
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn apply(connection: &Connection, method: &str, params: &Value) -> Result<Value, String> {
    let workspace = text(params, "workspace")?;
    if method == "outbox.list" {
        return serde_json::to_value(list(connection, workspace)?).map_err(|e| e.to_string());
    }
    if method == "outbox.acknowledge_many" {
        let ids = params["ids"]
            .as_array()
            .ok_or("Message identities are required.")?;
        if ids.iter().any(|id| id.as_str().is_none()) {
            return Err("Invalid message identity.".into());
        }
        for mut entry in list(connection, workspace)? {
            if ids.iter().any(|id| id.as_str() == Some(&entry.id)) {
                let (_, in_flight) = load(connection, workspace, &entry.id)?;
                entry.display_pending = false;
                save(connection, &entry, in_flight)?;
            }
        }
        return Ok(json!({"acknowledged":true}));
    }
    if method == "outbox.enqueue" {
        let body = params
            .get("body")
            .filter(|value| value.is_object())
            .ok_or("A message body is required.")?;
        let identity: BodyIdentity = observation(body);
        let id = identity.id()?;
        identity.room()?;
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM client_outbox WHERE workspace=?1 AND id=?2)",
                params![workspace, id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if exists {
            let (entry, _) = load(connection, workspace, id)?;
            if entry.body != *body {
                return Err("Message identity already has different content.".into());
            }
            if entry.status == "failed" {
                return Err(entry.error.unwrap_or_else(|| "Message failed.".into()));
            }
            return serde_json::to_value(entry).map_err(|e| e.to_string());
        }
        let entry = Entry {
            id: id.into(),
            workspace: workspace.into(),
            body: body.clone(),
            status: "queued".into(),
            attempted: Some(false),
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_millis() as u64,
            display_pending: true,
            attachments: params
                .get("attachments")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            display_text: None,
            error: None,
            receipt: None,
        };
        save(connection, &entry, false)?;
        return serde_json::to_value(entry).map_err(|e| e.to_string());
    }
    let id = text(params, "id")?;
    let (mut entry, mut in_flight) = load(connection, workspace, id)?;
    match method {
        "outbox.claim" => {
            let identity: BodyIdentity = observation(&entry.body);
            let room = identity.room()?;
            let occupied: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE workspace=?1 AND room=?2 AND in_flight<>0)",
                params![workspace,room], |row| row.get(0)).map_err(|e| e.to_string())?;
            let first = list(connection, workspace)?.into_iter().find(|other| {
                other.status == "queued"
                    && observation::<BodyIdentity>(&other.body).room == identity.room
            });
            if entry.status != "queued"
                || in_flight
                || occupied
                || first.as_ref().map(|first| first.id.as_str()) != Some(id)
            {
                return serde_json::to_value(Claim {
                    claimed: false,
                    entry,
                })
                .map_err(|e| e.to_string());
            }
            entry.attempted = Some(true);
            save(connection, &entry, true)?;
            return serde_json::to_value(Claim {
                claimed: true,
                entry,
            })
            .map_err(|e| e.to_string());
        }
        "outbox.change" => {
            if !["queued", "paused"].contains(&entry.status.as_str()) {
                return Err("This message already left the device queue.".into());
            }
            match text(params, "action")? {
                "cancel" => {
                    if entry.attempted != Some(false) {
                        return Err(
                            "Delivery may have started. Stop retrying or check the conversation."
                                .into(),
                        );
                    }
                    entry.status = "cancelled".into();
                    entry.error = None;
                    entry.receipt = Some(json!({"id":id,"status":"cancelled"}));
                }
                "pause" => entry.status = "paused".into(),
                "resume" => entry.status = "queued".into(),
                _ => return Err("Unknown queue action.".into()),
            }
        }
        "outbox.acknowledge" => entry.display_pending = false,
        "outbox.display" => {
            entry.display_text = Some(
                params
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or("Display text is required.")?
                    .into(),
            )
        }
        "outbox.receipt" => {
            let receipt = params.get("receipt").ok_or("A receipt is required.")?;
            let observed: ReceiptObservation = observation(receipt);
            if observed.id.as_deref() != Some(id) {
                return Err("The delivery response does not match this message.".into());
            }
            let status = observed.status.as_deref();
            let acknowledged = status.is_some_and(|value| {
                [
                    "queued",
                    "pending",
                    "reserved",
                    "dispatching",
                    "delivered",
                    "accepted",
                    "sent",
                    "uncertain",
                    "failed",
                    "cancelled",
                ]
                .contains(&value)
            });
            let body: BodyIdentity = observation(&entry.body);
            let legacy = status.is_none()
                && observed.room == body.room
                && observed.text.as_deref() == body.text.as_deref().map(str::trim)
                && observed.deliveries;
            if !acknowledged && !legacy {
                return Err("The delivery response does not match this message.".into());
            }
            let confirmed = entry
                .receipt
                .as_ref()
                .is_some_and(|value| observation::<ReceiptObservation>(value).confirmed());
            let preserve = entry.status == "cancelled"
                || (["accepted", "uncertain"].contains(&entry.status.as_str())
                    && (confirmed || (!observed.confirmed() && status != Some("uncertain"))));
            if !preserve {
                entry.status = match status {
                    Some("uncertain") => "uncertain",
                    Some("failed" | "cancelled") => "failed",
                    _ => "accepted",
                }
                .into();
                entry.error = observed.error;
                entry.receipt = Some(receipt.clone());
            }
            in_flight = false;
        }
        "outbox.failure" => {
            if ["queued", "paused"].contains(&entry.status.as_str()) {
                entry.error = Some(
                    params
                        .get("error")
                        .and_then(Value::as_str)
                        .ok_or("An error message is required.")?
                        .into(),
                );
                if params.get("retryable").and_then(Value::as_bool) == Some(false) {
                    entry.status = "failed".into();
                }
            }
            in_flight = false;
        }
        _ => return Err("Unknown outbox command.".into()),
    }
    save(connection, &entry, in_flight)?;
    serde_json::to_value(entry).map_err(|e| e.to_string())
}

pub fn dispatch(connection: &mut Connection, request: &Request) -> Response {
    let result = (|| -> Result<Response, String> {
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let durable = matches!(
            request.method.as_str(),
            "outbox.enqueue"
                | "outbox.change"
                | "outbox.display"
                | "outbox.acknowledge"
                | "outbox.acknowledge_many"
        );
        if durable
            && let Some(response) =
                crate::store::replay(&transaction, request).map_err(|e| e.to_string())?
        {
            return Ok(response);
        }
        let response = match apply(&transaction, &request.method, &request.params) {
            Ok(value) => Response::success(request.id.clone(), value),
            Err(error) => Response::failure(request.id.clone(), "outbox_rejected", error),
        };
        if durable {
            crate::store::record(&transaction, request, &response).map_err(|e| e.to_string())?;
        }
        transaction.commit().map_err(|e| e.to_string())?;
        Ok(response)
    })();
    match result {
        Ok(response) => response,
        Err(error) => Response::failure(request.id.clone(), "outbox_rejected", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Connection {
        crate::store::Store::open(std::path::Path::new(":memory:"))
            .unwrap()
            .connection
    }
    fn command(db: &mut Connection, method: &str, params: Value) -> Response {
        static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        dispatch(
            db,
            &Request {
                id: format!(
                    "test:{}",
                    ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ),
                method: method.into(),
                params,
            },
        )
    }
    fn enqueue(db: &mut Connection, id: &str, text: &str) {
        assert!(
            command(
                db,
                "outbox.enqueue",
                json!({"workspace":"w","body":{"id":id,"room":"r","text":text}})
            )
            .error
            .is_none()
        );
    }
    #[test]
    fn replayed_pause_does_not_undo_a_later_resume() {
        let mut db = database();
        enqueue(&mut db, "a", "one");
        let pause = Request {
            id: "pause-once".into(),
            method: "outbox.change".into(),
            params: json!({"workspace":"w","id":"a","action":"pause"}),
        };
        assert!(dispatch(&mut db, &pause).error.is_none());
        command(
            &mut db,
            "outbox.change",
            json!({"workspace":"w","id":"a","action":"resume"}),
        );
        assert!(dispatch(&mut db, &pause).error.is_none());
        assert_eq!(list(&db, "w").unwrap()[0].status, "queued");
    }
    #[test]
    fn claim_order_and_restart_preserve_uncertain_delivery() {
        let mut db = database();
        initialize(&db).unwrap();
        enqueue(&mut db, "a", "one");
        enqueue(&mut db, "b", "two");
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"b"}))
                .result
                .unwrap()["claimed"],
            false
        );
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                .result
                .unwrap()["claimed"],
            true
        );
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                .result
                .unwrap()["claimed"],
            false
        );
        initialize(&db).unwrap();
        assert!(
            command(
                &mut db,
                "outbox.change",
                json!({"workspace":"w","id":"a","action":"cancel"})
            )
            .error
            .is_some()
        );
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                .result
                .unwrap()["claimed"],
            true
        );
    }
    #[test]
    fn receipts_cannot_downgrade_confirmed_delivery_or_change_identity() {
        let mut db = database();
        initialize(&db).unwrap();
        enqueue(&mut db, "a", "one");
        assert!(
            command(
                &mut db,
                "outbox.enqueue",
                json!({"workspace":"w","body":{"id":"a","room":"r","text":"changed"}})
            )
            .error
            .is_some()
        );
        assert!(
            command(
                &mut db,
                "outbox.receipt",
                json!({"workspace":"w","id":"a","receipt":{"id":"other","status":"sent"}})
            )
            .error
            .is_some()
        );
        command(
            &mut db,
            "outbox.receipt",
            json!({"workspace":"w","id":"a","receipt":{"id":"a","status":"sent"}}),
        );
        let result = command(
            &mut db,
            "outbox.receipt",
            json!({"workspace":"w","id":"a","receipt":{"id":"a","status":"uncertain"}}),
        );
        assert_eq!(result.result.unwrap()["receipt"]["status"], "sent");
    }
    #[test]
    fn legacy_receipt_presence_conflicts_and_uncertainty_preserve_state() {
        let mut db = database();
        enqueue(&mut db, "a", "  hello  ");
        let before = list(&db, "w").unwrap();
        for receipt in [
            json!({"id":"a","text":"hello","deliveries":{}}),
            json!({"id":"a","room":null,"text":"hello","deliveries":{}}),
            json!({"id":"other","room":"r","text":"hello","deliveries":{}}),
        ] {
            assert!(
                command(
                    &mut db,
                    "outbox.receipt",
                    json!({"workspace":"w","id":"a","receipt":receipt})
                )
                .error
                .is_some()
            );
            assert_eq!(
                serde_json::to_value(list(&db, "w").unwrap()).unwrap(),
                serde_json::to_value(&before).unwrap()
            );
        }
        let legacy =
            json!({"id":"a","room":"r","text":"hello","deliveries":{},"future":{"opaque":true}});
        let accepted = command(
            &mut db,
            "outbox.receipt",
            json!({"workspace":"w","id":"a","receipt":legacy}),
        )
        .result
        .unwrap();
        assert_eq!(accepted["status"], "accepted");
        assert_eq!(accepted["receipt"], legacy);
        let uncertain = command(
            &mut db,
            "outbox.receipt",
            json!({"workspace":"w","id":"a","receipt":{"id":"a","status":"uncertain"}}),
        )
        .result
        .unwrap();
        assert_eq!(uncertain["status"], "uncertain");
        let delayed = command(
            &mut db,
            "outbox.receipt",
            json!({"workspace":"w","id":"a","receipt":legacy}),
        )
        .result
        .unwrap();
        assert_eq!(delayed["status"], "uncertain");
        assert_eq!(delayed["receipt"]["status"], "uncertain");
        let confirmed = command(
            &mut db,
            "outbox.receipt",
            json!({"workspace":"w","id":"a","receipt":{"id":"a","status":"sent"}}),
        )
        .result
        .unwrap();
        assert_eq!(confirmed["status"], "accepted");
    }

    #[test]
    fn legacy_receipt_missing_room_differs_from_explicit_null() {
        let mut db = database();
        enqueue(&mut db, "a", "hello");
        db.execute(
            "UPDATE client_outbox SET payload=json_remove(payload,'$.body.room') WHERE id='a'",
            [],
        )
        .unwrap();
        let missing = command(
            &mut db,
            "outbox.receipt",
            json!({"workspace":"w","id":"a","receipt":{"id":"a","text":"hello","deliveries":{}}}),
        );
        assert_eq!(missing.error.unwrap().message, "room is required.");
        let null = command(
            &mut db,
            "outbox.receipt",
            json!({"workspace":"w","id":"a","receipt":{"id":"a","room":null,"text":"hello","deliveries":{}}}),
        );
        assert_eq!(
            null.error.unwrap().message,
            "The delivery response does not match this message."
        );
    }

    #[test]
    fn cancelled_unsent_messages_are_not_claimed() {
        let mut db = database();
        initialize(&db).unwrap();
        enqueue(&mut db, "a", "one");
        assert!(
            command(
                &mut db,
                "outbox.change",
                json!({"workspace":"w","id":"a","action":"cancel"})
            )
            .error
            .is_none()
        );
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                .result
                .unwrap()["claimed"],
            false
        );
    }
}
