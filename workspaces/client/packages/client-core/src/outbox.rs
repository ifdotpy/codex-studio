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

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS client_outbox (
        workspace TEXT NOT NULL, id TEXT NOT NULL, room TEXT NOT NULL,
        payload TEXT NOT NULL, in_flight INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(workspace,id));
        CREATE TABLE IF NOT EXISTS client_outbox_removed (
        workspace TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(workspace,id));",
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

/// Server receipt statuses (`MessageDelivery["status"]` in the web client).
const RECEIPT_STATUSES: [&str; 11] = [
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
    "stored_only",
];

/// The device queue states of an entry.
const ENTRY_STATUSES: [&str; 6] = [
    "queued",
    "paused",
    "cancelled",
    "accepted",
    "uncertain",
    "failed",
];

fn load(connection: &Connection, workspace: &str, id: &str) -> Result<(Entry, bool), String> {
    find(connection, workspace, id)?
        .ok_or_else(|| "This message is no longer on this device.".into())
}

fn find(
    connection: &Connection,
    workspace: &str,
    id: &str,
) -> Result<Option<(Entry, bool)>, String> {
    let row: Option<(String, bool)> = connection
        .query_row(
            "SELECT payload,in_flight FROM client_outbox WHERE workspace=?1 AND id=?2",
            params![workspace, id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    row.map(|(payload, in_flight)| {
        Ok((
            serde_json::from_str(&payload).map_err(|e| e.to_string())?,
            in_flight,
        ))
    })
    .transpose()
}

/// A removed message card fences its message identity: the message is
/// cancelled while unsent, else paused, also when it is queued later.
fn removed(connection: &Connection, workspace: &str, id: &str) -> Result<bool, String> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM client_outbox_removed WHERE workspace=?1 AND id=?2)",
            params![workspace, id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())
}

fn stop_removed(entry: &mut Entry) {
    if entry.attempted == Some(false) {
        entry.status = "cancelled".into();
        entry.receipt = Some(json!({"id":entry.id,"status":"cancelled"}));
    } else {
        entry.status = "paused".into();
    }
}

/// `receiptOutgoing` in the web client: a server receipt updates only a message
/// the server already acknowledged, and never replaces a delivered receipt.
pub fn reconcile(
    connection: &Connection,
    workspace: &str,
    room: &str,
    receipt: &Value,
) -> Result<bool, String> {
    let (Some(id), Some(status)) = (
        receipt.get("id").and_then(Value::as_str),
        receipt.get("status").and_then(Value::as_str),
    ) else {
        return Ok(false);
    };
    if !RECEIPT_STATUSES.contains(&status) {
        return Ok(false);
    }
    let Some((mut entry, in_flight)) = find(connection, workspace, id)? else {
        return Ok(false);
    };
    if entry.body.get("room").and_then(Value::as_str) != Some(room)
        || !["accepted", "uncertain"].contains(&entry.status.as_str())
    {
        return Ok(false);
    }
    let previous = serde_json::to_value(&entry).map_err(|e| e.to_string())?;
    let current = match &entry.receipt {
        Some(stored) if stored["status"] == "delivered" && status != "delivered" => stored.clone(),
        _ => receipt.clone(),
    };
    entry.status = match current["status"].as_str() {
        Some("uncertain") => "uncertain",
        Some("failed") => "failed",
        Some("cancelled") => "cancelled",
        _ => "accepted",
    }
    .into();
    entry.error = current
        .get("error")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
        .map(str::to_owned);
    if entry.status == "accepted"
        && current["status"] == "delivered"
        && current["materialized"] == true
    {
        entry.display_pending = false;
    }
    entry.receipt = Some(current);
    if serde_json::to_value(&entry).map_err(|e| e.to_string())? == previous {
        return Ok(false);
    }
    save(connection, &entry, in_flight)?;
    Ok(true)
}

/// One legacy browser queue row, kept as it was: its status and attempted
/// marker decide whether the message may still be cancelled.
fn import(connection: &Connection, workspace: &str, value: &Value) -> Result<bool, String> {
    let id = text(value, "id")?;
    let body = value.get("body").filter(|body| body.is_object());
    let status = value.get("status").and_then(Value::as_str);
    if body.and_then(|body| body.get("id")).and_then(Value::as_str) != Some(id)
        || body
            .map(|body| text(body, "room"))
            .is_none_or(|room| room.is_err())
        || !status.is_some_and(|status| ENTRY_STATUSES.contains(&status))
        || !value.get("created").is_some_and(Value::is_u64)
    {
        return Err(format!("Saved message {id} could not be read."));
    }
    let mut record = value.clone();
    record["workspace"] = json!(workspace);
    if record.get("displayPending").is_none() {
        record["displayPending"] = json!(true);
    }
    if record.get("attachments").is_none() {
        record["attachments"] = json!([]);
    }
    let entry: Entry = serde_json::from_value(record)
        .map_err(|_| format!("Saved message {id} could not be read."))?;
    if find(connection, workspace, id)?.is_some() {
        return Ok(false);
    }
    save(connection, &entry, false)?;
    Ok(true)
}

fn save(connection: &Connection, entry: &Entry, in_flight: bool) -> Result<(), String> {
    let room = text(&entry.body, "room")?;
    connection.execute("INSERT INTO client_outbox(workspace,id,room,payload,in_flight) VALUES(?1,?2,?3,?4,?5)
        ON CONFLICT(workspace,id) DO UPDATE SET payload=excluded.payload,in_flight=excluded.in_flight",
        params![entry.workspace,entry.id,room,serde_json::to_string(entry).map_err(|e| e.to_string())?,in_flight])
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn apply(connection: &Connection, method: &str, params: &Value) -> Result<Value, String> {
    // A message identity is minted here and stored only by `outbox.enqueue`.
    if method == "outbox.reserve" {
        return Ok(json!({"id":crate::server_keys::random_id()?}));
    }
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
    if method == "outbox.import" {
        let entries = params["entries"]
            .as_array()
            .ok_or("Saved messages are required.")?;
        let mut imported = 0;
        for value in entries {
            imported += usize::from(import(connection, workspace, value)?);
        }
        return Ok(json!({"imported":imported}));
    }
    if method == "outbox.reconcile" {
        let room = text(params, "room")?;
        let receipts = params["receipts"]
            .as_array()
            .ok_or("Receipts are required.")?;
        let mut changed = 0;
        for receipt in receipts {
            changed += usize::from(reconcile(connection, workspace, room, receipt)?);
        }
        return Ok(json!({"changed":changed}));
    }
    if method == "outbox.enqueue" {
        let body = params
            .get("body")
            .filter(|value| value.is_object())
            .ok_or("A message body is required.")?;
        let id = text(body, "id")?;
        text(body, "room")?;
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
        let mut entry = Entry {
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
        if removed(connection, workspace, id)? {
            stop_removed(&mut entry);
        }
        save(connection, &entry, false)?;
        return serde_json::to_value(entry).map_err(|e| e.to_string());
    }
    let id = text(params, "id")?;
    if method == "outbox.stop" {
        connection
            .execute(
                "INSERT OR IGNORE INTO client_outbox_removed(workspace,id) VALUES(?1,?2)",
                params![workspace, id],
            )
            .map_err(|e| e.to_string())?;
        let Some((mut entry, in_flight)) = find(connection, workspace, id)? else {
            return Ok(json!({"cancelled":false}));
        };
        if ["queued", "paused"].contains(&entry.status.as_str()) {
            stop_removed(&mut entry);
            save(connection, &entry, in_flight)?;
        }
        return Ok(json!({"cancelled":entry.status == "cancelled"}));
    }
    let (mut entry, mut in_flight) = load(connection, workspace, id)?;
    match method {
        "outbox.claim" => {
            let room = text(&entry.body, "room")?;
            let occupied: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE workspace=?1 AND room=?2 AND in_flight<>0)",
                params![workspace,room], |row| row.get(0)).map_err(|e| e.to_string())?;
            let first = list(connection, workspace)?.into_iter().find(|other| {
                other.status == "queued" && other.body.get("room") == entry.body.get("room")
            });
            if entry.status == "queued" && !in_flight && removed(connection, workspace, id)? {
                stop_removed(&mut entry);
                save(connection, &entry, false)?;
                return Ok(json!({"claimed":false,"entry":entry}));
            }
            if entry.status != "queued"
                || in_flight
                || occupied
                || first.as_ref().map(|first| first.id.as_str()) != Some(id)
            {
                return Ok(json!({"claimed":false,"entry":entry}));
            }
            entry.attempted = Some(true);
            save(connection, &entry, true)?;
            return Ok(json!({"claimed":true,"entry":entry}));
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
                "resume" => {
                    connection
                        .execute(
                            "DELETE FROM client_outbox_removed WHERE workspace=?1 AND id=?2",
                            params![workspace, id],
                        )
                        .map_err(|e| e.to_string())?;
                    entry.status = "queued".into();
                }
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
            if receipt.get("id").and_then(Value::as_str) != Some(id) {
                return Err("The delivery response does not match this message.".into());
            }
            let status = receipt.get("status").and_then(Value::as_str);
            let acknowledged = status.is_some_and(|value| RECEIPT_STATUSES.contains(&value));
            let legacy = status.is_none()
                && receipt.get("room") == entry.body.get("room")
                && receipt.get("text").and_then(Value::as_str)
                    == entry
                        .body
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::trim)
                && receipt.get("deliveries").is_some_and(Value::is_object);
            if !acknowledged && !legacy {
                return Err("The delivery response does not match this message.".into());
            }
            let confirmed = |value: Option<&Value>| {
                value
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str)
                    .is_some_and(|status| ["delivered", "accepted", "sent"].contains(&status))
            };
            let preserve = entry.status == "cancelled"
                || (["accepted", "uncertain"].contains(&entry.status.as_str())
                    && (confirmed(entry.receipt.as_ref())
                        || (!confirmed(Some(receipt)) && status != Some("uncertain"))));
            if !preserve {
                entry.status = match status {
                    Some("uncertain") => "uncertain",
                    Some("failed" | "cancelled") => "failed",
                    _ => "accepted",
                }
                .into();
                entry.error = receipt
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
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
                | "outbox.import"
                | "outbox.stop"
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

    fn claim(db: &mut Connection, id: &str) -> Value {
        command(db, "outbox.claim", json!({"workspace":"w","id":id}))
            .result
            .unwrap()
    }
    #[test]
    fn a_removed_card_cancels_unsent_messages_and_pauses_attempted_ones() {
        let mut db = database();
        // Removed before the queue saved it: the later enqueue is cancelled.
        command(&mut db, "outbox.stop", json!({"workspace":"w","id":"a"}));
        enqueue(&mut db, "a", "one");
        assert_eq!(list(&db, "w").unwrap()[0].status, "cancelled");
        assert_eq!(claim(&mut db, "a")["claimed"], false);
        // Removed after delivery started: paused, never cancelled.
        enqueue(&mut db, "b", "two");
        assert_eq!(claim(&mut db, "b")["claimed"], true);
        command(
            &mut db,
            "outbox.failure",
            json!({"workspace":"w","id":"b","error":"offline","retryable":true}),
        );
        let stopped = command(&mut db, "outbox.stop", json!({"workspace":"w","id":"b"}));
        assert_eq!(stopped.result.unwrap()["cancelled"], false);
        assert_eq!(list(&db, "w").unwrap()[1].status, "paused");
        // Resume clears the fence; the same identity is delivered again.
        command(
            &mut db,
            "outbox.change",
            json!({"workspace":"w","id":"b","action":"resume"}),
        );
        assert_eq!(claim(&mut db, "b")["claimed"], true);
    }
    #[test]
    fn import_keeps_the_legacy_attempted_marker_and_existing_rows() {
        let mut db = database();
        enqueue(&mut db, "a", "local");
        let legacy = |id: &str, text: &str| json!({"id":id,"body":{"id":id,"room":"r","text":text},"status":"queued","created":5});
        let result = command(
            &mut db,
            "outbox.import",
            json!({"workspace":"w","entries":[legacy("a","old"),legacy("b","old")]}),
        );
        assert_eq!(result.result.unwrap()["imported"], 1);
        let entries = list(&db, "w").unwrap();
        assert_eq!(entries[1].body["text"], "local");
        assert_eq!(entries[0].attempted, None);
        // An old row without the marker may have reached the server.
        let cancel = command(
            &mut db,
            "outbox.change",
            json!({"workspace":"w","id":"b","action":"cancel"}),
        );
        assert!(cancel.error.is_some());
        let bad = command(
            &mut db,
            "outbox.import",
            json!({"workspace":"w","entries":[{"id":"c","body":{"id":"other","room":"r"},"status":"queued","created":1}]}),
        );
        assert!(bad.error.is_some());
    }
    #[test]
    fn server_receipts_reconcile_only_acknowledged_messages_of_their_chat() {
        let mut db = database();
        enqueue(&mut db, "a", "one");
        enqueue(&mut db, "b", "two");
        claim(&mut db, "a");
        let stored = command(
            &mut db,
            "outbox.receipt",
            json!({"workspace":"w","id":"a","receipt":{"id":"a","status":"stored_only"}}),
        );
        assert_eq!(stored.result.unwrap()["status"], "accepted");
        let reconcile = |db: &mut Connection, room: &str, receipt: Value| {
            command(
                db,
                "outbox.reconcile",
                json!({"workspace":"w","room":room,"receipts":[receipt]}),
            )
            .result
            .unwrap()["changed"]
                .clone()
        };
        // A queued message keeps the idempotent send path.
        assert_eq!(
            reconcile(&mut db, "r", json!({"id":"b","status":"delivered"})),
            0
        );
        assert_eq!(
            reconcile(&mut db, "other", json!({"id":"a","status":"delivered"})),
            0
        );
        assert_eq!(
            reconcile(
                &mut db,
                "r",
                json!({"id":"a","status":"delivered","materialized":true})
            ),
            1
        );
        assert_eq!(
            reconcile(&mut db, "r", json!({"id":"a","status":"pending"})),
            0
        );
        let entry = list(&db, "w").unwrap().remove(0);
        assert_eq!(entry.receipt.unwrap()["status"], "delivered");
        assert!(!entry.display_pending);
    }

    #[test]
    fn reserved_message_identities_are_core_minted_and_unique() {
        let mut db = database();
        let mut reserve = || {
            command(&mut db, "outbox.reserve", json!({}))
                .result
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let first = reserve();
        let second = reserve();
        assert_ne!(first, second);
        assert_eq!(uuid::Uuid::parse_str(&first).unwrap().get_version_num(), 4);
        assert!(
            list(&db, "w").unwrap().is_empty(),
            "a reservation stores nothing"
        );
        // The reserved identity is the one the enqueue stores.
        let enqueued = command(
            &mut db,
            "outbox.enqueue",
            json!({"workspace":"w","body":{"id":first,"room":"r","text":"one"}}),
        );
        assert!(enqueued.error.is_none());
        assert_eq!(list(&db, "w").unwrap()[0].id, first);
    }

    #[test]
    fn a_streamed_receipt_entity_settles_its_message() {
        let mut db = database();
        let workspace = "a".repeat(32);
        let body = json!({"id":"m","room":"r","text":"one"});
        command(
            &mut db,
            "outbox.enqueue",
            json!({"workspace":workspace,"body":body}),
        );
        command(
            &mut db,
            "outbox.claim",
            json!({"workspace":workspace,"id":"m"}),
        );
        command(
            &mut db,
            "outbox.receipt",
            json!({"workspace":workspace,"id":"m","receipt":{"id":"m","status":"queued"}}),
        );
        let applied = crate::projections::dispatch(
            &db,
            &Request {
                id: "entities".into(),
                method: "projection.applyEntities".into(),
                params: json!({"workspace":workspace,"replace":false,"changes":[{
                    "op":"upsert","collection":"messageReceipt","id":"m","seq":1,
                    "value":{"id":"m","agent":"r","status":"delivered","materialized":true}
                }]}),
            },
        );
        assert!(applied.error.is_none(), "{:?}", applied.error);
        let entry = list(&db, &workspace).unwrap().remove(0);
        assert_eq!(entry.receipt.unwrap()["status"], "delivered");
        assert!(!entry.display_pending);
    }
}
