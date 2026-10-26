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

pub(crate) fn initialize_for_migration(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS client_outbox (
        workspace TEXT NOT NULL, id TEXT NOT NULL, room TEXT NOT NULL,
        payload TEXT NOT NULL, in_flight INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(workspace,id));
        CREATE TABLE IF NOT EXISTS client_outbox_removal_fences (
        workspace TEXT NOT NULL, id TEXT NOT NULL,
        active INTEGER NOT NULL CHECK(active IN (0,1)),
        PRIMARY KEY(workspace,id));",
    )
}

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    initialize_for_migration(connection)?;
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
    load_optional(connection, workspace, id)?
        .ok_or_else(|| "This message is no longer on this device.".into())
}

fn load_optional(
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

fn save(connection: &Connection, entry: &Entry, in_flight: bool) -> Result<(), String> {
    let room = text(&entry.body, "room")?;
    connection.execute("INSERT INTO client_outbox(workspace,id,room,payload,in_flight) VALUES(?1,?2,?3,?4,?5)
        ON CONFLICT(workspace,id) DO UPDATE SET payload=excluded.payload,in_flight=excluded.in_flight",
        params![entry.workspace,entry.id,room,serde_json::to_string(entry).map_err(|e| e.to_string())?,in_flight])
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn removal_fenced(connection: &Connection, workspace: &str, id: &str) -> Result<bool, String> {
    connection
        .query_row(
            "SELECT active FROM client_outbox_removal_fences WHERE workspace=?1 AND id=?2",
            params![workspace, id],
            |row| row.get(0),
        )
        .optional()
        .map(|active| active.unwrap_or(false))
        .map_err(|e| e.to_string())
}

fn apply_removal_fence(connection: &Connection, entry: &mut Entry) -> Result<bool, String> {
    if !removal_fenced(connection, &entry.workspace, &entry.id)?
        || !["queued", "paused"].contains(&entry.status.as_str())
    {
        return Ok(false);
    }
    let cancelled = entry.attempted == Some(false);
    entry.status = if cancelled { "cancelled" } else { "paused" }.into();
    if cancelled {
        entry.receipt = Some(json!({"id":entry.id,"status":"cancelled"}));
    }
    Ok(cancelled)
}

/// Import a validated legacy removal in the caller's migration transaction.
/// A later native resume leaves a cleared row that old imports cannot replace.
pub(crate) fn import_removal_fence(
    connection: &Connection,
    workspace: &str,
    id: &str,
) -> Result<(), String> {
    let scope = json!({"workspace":workspace,"id":id});
    text(&scope, "workspace")?;
    text(&scope, "id")?;
    connection.execute(
        "INSERT OR IGNORE INTO client_outbox_removal_fences(workspace,id,active) VALUES(?1,?2,1)",
        params![workspace,id],
    ).map_err(|e| e.to_string())?;
    if let Some((mut entry, in_flight)) = load_optional(connection, workspace, id)? {
        apply_removal_fence(connection, &mut entry)?;
        save(connection, &entry, in_flight)?;
    }
    Ok(())
}

/// Restore a saved intention without enqueue, claim, or delivery. A previous
/// process's wait has ended; its attempted flag and receipt remain unchanged.
pub(crate) fn import_entry(connection: &Connection, entry: &Entry) -> Result<(), String> {
    if entry.id != text(&entry.body, "id")?
        || entry.workspace.is_empty()
        || ![
            "queued",
            "paused",
            "cancelled",
            "accepted",
            "uncertain",
            "failed",
        ]
        .contains(&entry.status.as_str())
        || entry.created > studio_client_domain::MAX_SAFE_INTEGER as u64
        || entry
            .receipt
            .as_ref()
            .is_some_and(|r| r["id"].as_str() != Some(&entry.id))
    {
        return Err("Invalid imported message intention.".into());
    }
    text(&entry.body, "room")?;
    let mut entry = entry.clone();
    apply_removal_fence(connection, &mut entry)?;
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM client_outbox WHERE workspace=?1 AND id=?2)",
            params![entry.workspace, entry.id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if exists {
        let (old, _) = load(connection, &entry.workspace, &entry.id)?;
        if serde_json::to_value(old).map_err(|e| e.to_string())?
            != serde_json::to_value(&entry).map_err(|e| e.to_string())?
        {
            return Err(
                "The saved message conflicts with native data. Both sources were preserved.".into(),
            );
        }
        return Ok(());
    }
    save(connection, &entry, false)
}

fn apply(connection: &Connection, method: &str, params: &Value) -> Result<Value, String> {
    if matches!(method, "outbox.enqueue" | "outbox.claim")
        && crate::uploads::import_pending(connection)?
    {
        return Err("Finish the saved attachment import before sending messages.".into());
    }
    let workspace = text(params, "workspace")?;
    if method == "outbox.list" {
        return serde_json::to_value(list(connection, workspace)?).map_err(|e| e.to_string());
    }
    if method == "outbox.stop_removed" {
        let id = text(params, "id")?;
        connection
            .execute(
                "INSERT INTO client_outbox_removal_fences(workspace,id,active) VALUES(?1,?2,1)
            ON CONFLICT(workspace,id) DO UPDATE SET active=1",
                params![workspace, id],
            )
            .map_err(|e| e.to_string())?;
        let Some((mut entry, in_flight)) = load_optional(connection, workspace, id)? else {
            return Ok(json!(false));
        };
        let cancelled = apply_removal_fence(connection, &mut entry)?
            || (entry.status == "cancelled"
                && entry.attempted == Some(false)
                && entry.receipt.as_ref().is_some_and(|receipt| {
                    receipt["id"] == entry.id && receipt["status"] == "cancelled"
                }));
        save(connection, &entry, in_flight)?;
        return Ok(json!(cancelled));
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
            let (mut entry, in_flight) = load(connection, workspace, id)?;
            if entry.body != *body {
                return Err("Message identity already has different content.".into());
            }
            if entry.status == "failed" {
                return Err(entry.error.unwrap_or_else(|| "Message failed.".into()));
            }
            apply_removal_fence(connection, &mut entry)?;
            save(connection, &entry, in_flight)?;
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
        apply_removal_fence(connection, &mut entry)?;
        save(connection, &entry, false)?;
        return serde_json::to_value(entry).map_err(|e| e.to_string());
    }
    let id = text(params, "id")?;
    let (mut entry, mut in_flight) = load(connection, workspace, id)?;
    match method {
        "outbox.claim" => {
            if removal_fenced(connection, workspace, id)? {
                apply_removal_fence(connection, &mut entry)?;
                save(connection, &entry, in_flight)?;
                return Ok(json!({"claimed":false,"entry":entry}));
            }
            let room = text(&entry.body, "room")?;
            let occupied: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE workspace=?1 AND room=?2 AND in_flight<>0)",
                params![workspace,room], |row| row.get(0)).map_err(|e| e.to_string())?;
            let first = list(connection, workspace)?.into_iter().find(|other| {
                other.status == "queued" && other.body.get("room") == entry.body.get("room")
            });
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
                    connection.execute(
                        "INSERT INTO client_outbox_removal_fences(workspace,id,active) VALUES(?1,?2,0)
                        ON CONFLICT(workspace,id) DO UPDATE SET active=0",
                        params![workspace,id],
                    ).map_err(|e| e.to_string())?;
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
            let acknowledged = studio_runtime_schema::validate(
                "studio_api__work__models__MessageReceipt",
                &json!({"id":id,"status":receipt.get("status")}),
            )?;
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
                | "outbox.stop_removed"
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
    fn native_outbox_work_receipt_validator_is_registered() {
        let schema = "studio_api__work__models__MessageReceipt";
        assert!(
            studio_runtime_schema::validate(schema, &json!({"id":"a","status":"stored_only"}))
                .unwrap()
        );
        assert!(
            !studio_runtime_schema::validate(schema, &json!({"id":"a","status":"unknown-status"}))
                .unwrap()
        );
    }

    #[test]
    fn native_outbox_accepts_every_canonical_work_receipt_status_without_retry() {
        let schema: Value = serde_json::from_str(include_str!(
            "../../../../runtime/packages/runtime-http/openapi.json"
        ))
        .unwrap();
        let statuses = schema
            .pointer("/components/schemas/studio_api__work__models__MessageReceipt/properties/status/enum")
            .and_then(Value::as_array)
            .expect("the work receipt schema owns the delivery statuses");
        assert!(!statuses.is_empty());
        for status in statuses {
            let status = status.as_str().unwrap();
            let mut db = database();
            enqueue(&mut db, "a", "original");
            assert_eq!(
                command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                    .result
                    .unwrap()["claimed"],
                true
            );
            let receipt = json!({"id":"a","status":status,"materialized":false,"room":"r","deliveries":{"remote":"stored_only"}});
            let result = command(
                &mut db,
                "outbox.receipt",
                json!({"workspace":"w","id":"a","receipt":receipt}),
            );
            assert!(result.error.is_none(), "{status}: {:?}", result.error);
            let (entry, in_flight) = load(&db, "w", "a").unwrap();
            assert_eq!(
                entry.status,
                match status {
                    "uncertain" => "uncertain",
                    "failed" | "cancelled" => "failed",
                    _ => "accepted",
                },
                "{status}"
            );
            assert_eq!(entry.receipt, Some(receipt), "{status}");
            assert_eq!(entry.attempted, Some(true), "{status}");
            assert!(!in_flight, "{status}");
            assert_eq!(
                command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                    .result
                    .unwrap()["claimed"],
                false,
                "{status}"
            );
        }
    }

    #[test]
    fn native_outbox_invalid_work_receipts_preserve_the_claim_without_any_write() {
        for receipt in [
            json!({"id":"a","status":"unknown-status"}),
            json!({"id":"other","status":"stored_only"}),
        ] {
            let mut db = database();
            enqueue(&mut db, "a", "original");
            assert_eq!(
                command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                    .result
                    .unwrap()["claimed"],
                true
            );
            let stored = |db: &Connection| {
                db.query_row(
                    "SELECT payload,in_flight FROM client_outbox WHERE workspace='w' AND id='a'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
                )
                .unwrap()
            };
            let before = stored(&db);
            assert!(
                command(
                    &mut db,
                    "outbox.receipt",
                    json!({"workspace":"w","id":"a","receipt":receipt})
                )
                .error
                .is_some(),
                "{receipt}"
            );
            assert_eq!(stored(&db), before, "{receipt}");
        }
    }

    #[test]
    fn native_outbox_removal_before_enqueue_survives_reopen_and_is_workspace_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let stop = Request {
            id: "remove-before-enqueue".into(),
            method: "outbox.stop_removed".into(),
            params: json!({"workspace":"w","id":"a"}),
        };
        {
            let mut db = crate::store::Store::open(&path).unwrap().connection;
            assert_eq!(dispatch(&mut db, &stop).result, Some(json!(false)));
            assert!(list(&db, "w").unwrap().is_empty());
        }
        let mut db = crate::store::Store::open(&path).unwrap().connection;
        enqueue(&mut db, "a", "original");
        let (entry, in_flight) = load(&db, "w", "a").unwrap();
        assert_eq!(entry.status, "cancelled");
        assert_eq!(entry.attempted, Some(false));
        assert_eq!(entry.body["text"], "original");
        assert_eq!(entry.receipt, Some(json!({"id":"a","status":"cancelled"})));
        assert!(!in_flight);
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                .result
                .unwrap()["claimed"],
            false
        );
        assert!(
            command(
                &mut db,
                "outbox.enqueue",
                json!({"workspace":"other","body":{"id":"a","room":"r","text":"other"}})
            )
            .error
            .is_none()
        );
        assert_eq!(
            command(
                &mut db,
                "outbox.claim",
                json!({"workspace":"other","id":"a"})
            )
            .result
            .unwrap()["claimed"],
            true
        );
        assert_eq!(
            dispatch(&mut db, &stop).result,
            Some(json!(false)),
            "replay returns the original absent result"
        );
    }

    #[test]
    fn native_outbox_removed_queued_message_is_cancelled_before_claim() {
        let mut db = database();
        enqueue(&mut db, "a", "original");
        assert_eq!(
            command(
                &mut db,
                "outbox.stop_removed",
                json!({"workspace":"w","id":"a"})
            )
            .result,
            Some(json!(true))
        );
        let (entry, in_flight) = load(&db, "w", "a").unwrap();
        assert_eq!(entry.status, "cancelled");
        assert_eq!(entry.attempted, Some(false));
        assert_eq!(entry.receipt, Some(json!({"id":"a","status":"cancelled"})));
        assert!(!in_flight);
        enqueue(&mut db, "a", "original");
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                .result
                .unwrap()["claimed"],
            false
        );
    }

    #[test]
    fn native_outbox_repeated_removal_confirms_the_saved_local_cancellation() {
        let mut db = database();
        enqueue(&mut db, "a", "original");
        for _ in 0..2 {
            assert_eq!(
                command(
                    &mut db,
                    "outbox.stop_removed",
                    json!({"workspace":"w","id":"a"})
                )
                .result,
                Some(json!(true)),
                "a confirmed local cancellation remains confirmed on another removal"
            );
        }
        let (entry, in_flight) = load(&db, "w", "a").unwrap();
        assert_eq!(entry.receipt, Some(json!({"id":"a","status":"cancelled"})));
        assert_eq!(entry.attempted, Some(false));
        assert!(!in_flight);
    }

    #[test]
    fn native_outbox_removed_claim_preserves_its_wait_and_accepts_exact_completion() {
        let mut db = database();
        enqueue(&mut db, "a", "original");
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                .result
                .unwrap()["claimed"],
            true
        );
        let (before, _) = load(&db, "w", "a").unwrap();
        assert_eq!(
            command(
                &mut db,
                "outbox.stop_removed",
                json!({"workspace":"w","id":"a"})
            )
            .result,
            Some(json!(false))
        );
        let (held, in_flight) = load(&db, "w", "a").unwrap();
        assert_eq!(held.status, "paused");
        assert_eq!(held.attempted, Some(true));
        assert_eq!(held.body, before.body);
        assert_eq!(held.receipt, before.receipt);
        assert!(in_flight, "removal does not cancel an HTTP wait");
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                .result
                .unwrap()["claimed"],
            false
        );
        let receipt = json!({"id":"a","status":"sent","deliveries":{"agent":"delivered"}});
        assert!(
            command(
                &mut db,
                "outbox.receipt",
                json!({"workspace":"w","id":"a","receipt":receipt})
            )
            .error
            .is_none()
        );
        let (completed, in_flight) = load(&db, "w", "a").unwrap();
        assert_eq!(completed.status, "accepted");
        assert_eq!(completed.receipt, Some(receipt));
        assert!(!in_flight);
    }

    #[test]
    fn native_outbox_removed_legacy_attempt_keeps_its_exact_receipt() {
        let mut db = database();
        enqueue(&mut db, "a", "original");
        let (mut legacy, _) = load(&db, "w", "a").unwrap();
        legacy.attempted = None;
        legacy.receipt = Some(json!({"id":"a","status":"pending","materialized":false}));
        legacy.error = Some("The earlier response was lost.".into());
        save(&db, &legacy, false).unwrap();
        assert_eq!(
            command(
                &mut db,
                "outbox.stop_removed",
                json!({"workspace":"w","id":"a"})
            )
            .result,
            Some(json!(false))
        );
        let (held, in_flight) = load(&db, "w", "a").unwrap();
        assert_eq!(held.status, "paused");
        assert_eq!(held.attempted, None);
        assert_eq!(held.body, legacy.body);
        assert_eq!(held.receipt, legacy.receipt);
        assert_eq!(held.error, legacy.error);
        assert!(!in_flight);
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                .result
                .unwrap()["claimed"],
            false
        );
    }

    #[test]
    fn native_outbox_removal_fence_blocks_a_later_import_claim() {
        let mut source = database();
        enqueue(&mut source, "a", "original");
        let entry = load(&source, "w", "a").unwrap().0;
        let mut db = database();
        assert_eq!(
            command(
                &mut db,
                "outbox.stop_removed",
                json!({"workspace":"w","id":"a"})
            )
            .result,
            Some(json!(false))
        );
        import_entry(&db, &entry).unwrap();
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                .result
                .unwrap()["claimed"],
            false
        );
        let held = load(&db, "w", "a").unwrap();
        assert_eq!(held.0.status, "cancelled");
        assert_eq!(held.0.attempted, Some(false));
        assert!(!held.1);
    }

    #[test]
    fn native_outbox_removal_replay_does_not_restore_a_fence_after_explicit_resume() {
        let mut db = database();
        enqueue(&mut db, "a", "original");
        command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}));
        command(
            &mut db,
            "outbox.failure",
            json!({"workspace":"w","id":"a","error":"Lost response","retryable":true}),
        );
        let stop = Request {
            id: "remove-once".into(),
            method: "outbox.stop_removed".into(),
            params: json!({"workspace":"w","id":"a"}),
        };
        let original = dispatch(&mut db, &stop);
        assert_eq!(original.result, Some(json!(false)));
        assert_eq!(load(&db, "w", "a").unwrap().0.status, "paused");
        assert!(
            command(
                &mut db,
                "outbox.change",
                json!({"workspace":"w","id":"a","action":"resume"})
            )
            .error
            .is_none()
        );
        assert_eq!(
            serde_json::to_value(dispatch(&mut db, &stop)).unwrap(),
            serde_json::to_value(original).unwrap()
        );
        assert_eq!(load(&db, "w", "a").unwrap().0.status, "queued");
        let changed = Request {
            params: json!({"workspace":"other","id":"a"}),
            ..stop
        };
        assert_eq!(
            dispatch(&mut db, &changed).error.unwrap().code,
            "identity_conflict"
        );
        assert!(
            command(
                &mut db,
                "outbox.enqueue",
                json!({"workspace":"other","body":{"id":"a","room":"r","text":"other"}})
            )
            .error
            .is_none()
        );
        assert_eq!(
            command(
                &mut db,
                "outbox.claim",
                json!({"workspace":"other","id":"a"})
            )
            .result
            .unwrap()["claimed"],
            true
        );
        assert_eq!(
            command(&mut db, "outbox.claim", json!({"workspace":"w","id":"a"}))
                .result
                .unwrap()["claimed"],
            true
        );
    }

    #[test]
    fn native_outbox_removed_terminal_receipts_are_not_changed() {
        for status in ["sent", "uncertain", "failed"] {
            let mut db = database();
            enqueue(&mut db, "a", "original");
            command(
                &mut db,
                "outbox.receipt",
                json!({"workspace":"w","id":"a","receipt":{"id":"a","status":status}}),
            );
            let before = load(&db, "w", "a").unwrap();
            assert_eq!(
                command(
                    &mut db,
                    "outbox.stop_removed",
                    json!({"workspace":"w","id":"a"})
                )
                .result,
                Some(json!(false))
            );
            let after = load(&db, "w", "a").unwrap();
            assert_eq!(
                serde_json::to_value(after.0).unwrap(),
                serde_json::to_value(before.0).unwrap()
            );
            assert_eq!(after.1, before.1);
        }
    }

    #[test]
    fn native_outbox_import_refuses_foreign_or_malformed_fences_atomically() {
        let workspace = "a".repeat(32);
        let prefix = studio_runtime_schema::shared_values::REMOVED_SEND_STORAGE_PREFIX;
        for (key, raw) in [
            (format!("{prefix}{}", json!(["b".repeat(32), "a"])), "1"),
            (format!("{prefix}{}", json!([workspace, 7])), "1"),
            (format!("{prefix}{}", json!([workspace, "a"])), "0"),
            (format!("{prefix}{}", json!([workspace, ""])), "1"),
        ] {
            let mut store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
            assert!(
                store
                    .dispatch(Request {
                        id: "identity".into(),
                        method: "identity.set".into(),
                        params: json!({"workspaceId":workspace})
                    })
                    .error
                    .is_none()
            );
            let source =
                json!({"version":1,"preferences":{"theme":"preserve-on-error",(key.clone()):raw}});
            let response = store.dispatch(Request {
                id: "import".into(),
                method: "client_storage.import".into(),
                params: json!({"manifestId":"invalid-fence","serializedSource":source.to_string()}),
            });
            assert!(response.error.is_some(), "{key}: {raw}");
            for table in [
                "client_preferences",
                "client_outbox_removal_fences",
                "client_storage_migrations",
                "client_outbox",
            ] {
                let count: i64 = store
                    .connection
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(count, 0, "{table}: {key}");
            }
        }
    }

    #[test]
    fn native_outbox_import_preserves_the_browser_removal_fence_before_claim() {
        let mut store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let workspace = "a".repeat(32);
        assert!(
            store
                .dispatch(Request {
                    id: "identity".into(),
                    method: "identity.set".into(),
                    params: json!({"workspaceId":workspace})
                })
                .error
                .is_none()
        );
        // Preserve the exact legacy JSON-pair key independently of the generated owner.
        let key = format!("studio-removed-send:{}", json!([workspace, "a"]));
        let source = json!({
            "version":1,
            "preferences":{(key.clone()):"1"},
            "outbox":[{"workspace":workspace,"id":"a","body":{"id":"a","room":"r","text":"original"},"status":"queued","attempted":false,"created":7,"displayPending":true,"attachments":[]}]
        });
        let import = Request {
            id: "import".into(),
            method: "client_storage.import".into(),
            params: json!({"manifestId":"legacy-removal","serializedSource":source.to_string()}),
        };
        assert!(store.dispatch(import).error.is_none());
        let claim = store.dispatch(Request {
            id: "claim".into(),
            method: "outbox.claim".into(),
            params: json!({"workspace":workspace,"id":"a"}),
        });
        assert_eq!(claim.result.unwrap()["claimed"], false);
        assert_eq!(
            load(&store.connection, &workspace, "a").unwrap().0.status,
            "cancelled"
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT value FROM client_preferences WHERE key=?1",
                    [key],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "1"
        );
    }

    #[test]
    fn native_outbox_stale_cross_manifest_import_cannot_restore_a_resumed_fence() {
        let mut store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let workspace = "a".repeat(32);
        assert!(
            store
                .dispatch(Request {
                    id: "identity".into(),
                    method: "identity.set".into(),
                    params: json!({"workspaceId":workspace})
                })
                .error
                .is_none()
        );
        let key = format!("studio-removed-send:{}", json!([workspace, "a"]));
        let source = json!({
            "version":1,
            "preferences":{(key.clone()):"1"},
            "outbox":[{"workspace":workspace,"id":"a","body":{"id":"a","room":"r","text":"original"},"status":"queued","attempted":true,"created":7,"displayPending":true,"attachments":[],"error":"Lost response"}]
        });
        assert!(store.dispatch(Request { id: "first-import".into(), method: "client_storage.import".into(), params: json!({"manifestId":"first-source","serializedSource":source.to_string()}) }).error.is_none());
        let first_claim = store.dispatch(Request {
            id: "first-claim".into(),
            method: "outbox.claim".into(),
            params: json!({"workspace":workspace,"id":"a"}),
        });
        assert_eq!(first_claim.result.unwrap()["claimed"], false);
        assert_eq!(
            load(&store.connection, &workspace, "a").unwrap().0.status,
            "paused"
        );
        assert!(
            store
                .dispatch(Request {
                    id: "explicit-resume".into(),
                    method: "outbox.change".into(),
                    params: json!({"workspace":workspace,"id":"a","action":"resume"})
                })
                .error
                .is_none()
        );
        let stale = json!({"version":1,"preferences":{(key.clone()):"1"}});
        assert!(store.dispatch(Request { id: "different-import".into(), method: "client_storage.import".into(), params: json!({"manifestId":"second-source","serializedSource":stale.to_string()}) }).error.is_none());
        let next_claim = store.dispatch(Request {
            id: "next-claim".into(),
            method: "outbox.claim".into(),
            params: json!({"workspace":workspace,"id":"a"}),
        });
        assert_eq!(
            next_claim.result.unwrap()["claimed"],
            true,
            "an old browser fence cannot override a later native resume"
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT value FROM client_preferences WHERE key=?1",
                    [key],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "1",
            "migration preserves the original preference bytes"
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
}
