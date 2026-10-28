//! Snapshot readers and user-controlled room visibility using canonical room views.
use crate::submit_gates::Deletion;
use crate::{
    coordination::CoordinationError, event_write::truthy, sync_entity_context::RoomDirectory,
    sync_views::room_entity_view,
};
use rusqlite::{OptionalExtension, Transaction};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use studio_runtime_schema::persistence::{DeliveryStatus, EventStatus, RecipientDelivery};
use studio_runtime_store::{SyncDocumentsRepository, system_reads as records};

pub fn delivery_receipts(
    connection: &Transaction<'_>,
    agent: &str,
    ids: &[String],
) -> Result<Value, CoordinationError> {
    if ids.len() > 100
        || ids
            .iter()
            .any(|id| id.is_empty() || id.chars().count() > 200)
    {
        return Err(CoordinationError::InvalidRequest(
            "Supply at most 100 message IDs of 1 to 200 characters",
        ));
    }
    crate::event_write::checked_agent(connection, agent)
        .map_err(crate::coordination::event_write_error)?;
    let mut statement = connection.prepare_cached(
        "SELECT e.id,e.status,e.error,m.record,i.id,i.record FROM runtime_events e
        LEFT JOIN runtime_event_meta m ON m.id=e.id
        LEFT JOIN runtime_items i ON i.agent=e.agent AND i.id=COALESCE(
            CASE WHEN json_valid(m.record) THEN json_extract(m.record,'$.transcriptItemId') END,
            e.agent || ':' || e.id)
        WHERE e.agent=?1 AND e.kind='user' AND e.id IN (SELECT value FROM json_each(?2))",
    )?;
    let items = statement
        .query_map(
            rusqlite::params![
                agent,
                serde_json::to_string(ids).map_err(studio_runtime_store::StoreError::from)?
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )?
        .map(|row| {
            let (id, status, error, metadata, item_id, item) = row?;
            let mut event = records::validate(
                "runtime_events",
                &id,
                json!({"id":id,"status":status,"error":error}),
            )?;
            if let Some(raw) = metadata {
                records::decode("runtime_event_meta", &id, &raw)?;
            }
            let item = item_id
                .zip(item)
                .map(|(key, raw)| {
                    records::decode("runtime_items", &key, &raw).map(|value| (key, value))
                })
                .transpose()?;
            event["materialized"] = json!(item.is_some_and(|(key, item)| {
                (key == format!("{agent}:{id}")
                    && item["role"] == "user"
                    && item.get("inputs").is_none())
                    || item["inputs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|input| input["id"] == id && input["kind"] == "user")
            }));
            Ok(event)
        })
        .collect::<Result<Vec<_>, studio_runtime_store::StoreError>>()?;
    Ok(json!({"agent":agent,"items":items}))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRead {
    pub room: Value,
    pub messages: Vec<ChatMessage>,
    pub next_before: Option<i64>,
    pub next_after: Option<i64>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub seq: i64,
    pub id: String,
    pub room: String,
    pub sender: String,
    pub text: String,
    pub created: f64,
    pub deliveries: BTreeMap<String, RecipientDelivery>,
    pub sender_name: Value,
}

pub fn chat_read(
    connection: &Transaction<'_>,
    room_id: &str,
    before: Option<i64>,
    after: Option<i64>,
    requested_limit: i64,
) -> Result<ChatRead, CoordinationError> {
    if before.is_some() && after.is_some() {
        return Err(CoordinationError::InvalidRequest("Use one message cursor"));
    }
    let limit = requested_limit.clamp(1, 100) as usize;
    let context = RoomDirectory::load(connection)?;
    let (room, room_ids) = if let Some(root_id) = room_id.strip_prefix("feed:") {
        let root = context
            .agents()
            .iter()
            .find(|a| {
                a.id.as_deref() == Some(root_id)
                    && a.is_lead == Some(true)
                    && !a.deleted_at.as_ref().is_some_and(Deletion::is_deleted)
            })
            .ok_or(CoordinationError::InvalidRequest("Team is unavailable"))?;
        let members = context
            .agents()
            .iter()
            .filter(|a| {
                a.root_id.as_deref() == Some(root_id)
                    && !a.deleted_at.as_ref().is_some_and(Deletion::is_deleted)
            })
            .filter_map(|a| a.id.as_deref())
            .collect::<BTreeSet<_>>();
        let rooms = records::all(connection, "runtime_rooms")?;
        let mut selection = connection.prepare_cached("WITH
            team_member AS (SELECT value AS id FROM json_each(?1)),
            peer_team AS (SELECT t.value AS team FROM json_each(?2) p, json_each(p.value,'$.peerTeams') t),
            room AS (SELECT value AS record FROM json_each(?3))
            SELECT json_extract(record,'$.id') FROM room
            WHERE (json_extract(record,'$.kind')='broadcast' AND json_extract(record,'$.rootId')=?4)
            OR (json_extract(record,'$.kind')!='broadcast' AND json_array_length(record,'$.members')>0
                AND NOT EXISTS(SELECT 1 FROM json_each(record,'$.members') WHERE value NOT IN (SELECT id FROM team_member)))
            OR (json_extract(record,'$.kind')='private' AND json_array_length(record,'$.members')=2
                AND EXISTS(SELECT 1 FROM peer_team
                    WHERE EXISTS(SELECT 1 FROM json_each(team,'$.members') WHERE value IN (SELECT id FROM team_member))
                    AND NOT EXISTS(SELECT 1 FROM json_each(record,'$.members') WHERE value NOT IN (SELECT value FROM json_each(team,'$.members')))))")?;
        let ids = selection
            .query_map(
                rusqlite::params![
                    serde_json::to_string(&members)
                        .map_err(studio_runtime_store::StoreError::from)?,
                    serde_json::to_string(context.projects())
                        .map_err(studio_runtime_store::StoreError::from)?,
                    serde_json::to_string(&rooms)
                        .map_err(studio_runtime_store::StoreError::from)?,
                    root_id
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        (
            Value::Object(serde_json::Map::from_iter([
                ("id".into(), Value::String(room_id.to_owned())),
                (
                    "name".into(),
                    studio_runtime_store::projection_value(&root.name)
                        .map_err(studio_runtime_store::StoreError::from)?,
                ),
                (
                    "members".into(),
                    studio_runtime_store::projection_value(&members)
                        .map_err(studio_runtime_store::StoreError::from)?,
                ),
            ])),
            ids,
        )
    } else {
        let stored = records::get(connection, "runtime_rooms", room_id)?.ok_or(
            CoordinationError::InvalidRequest("Chat is unavailable or you are not a participant"),
        )?;
        let ctx = context.for_room(connection, &stored, false)?;
        let mut room = room_entity_view(&stored, &ctx)
            .filter(|r| r["members"].as_array().is_some_and(|m| !m.is_empty()))
            .ok_or(CoordinationError::InvalidRequest(
                "Chat is unavailable or you are not a participant",
            ))?;
        if let Some(object) = room.as_object_mut() {
            object.remove("lastMessage");
        }
        if let Some(name) = stored.get("customName") {
            room["customName"] = name.clone();
        }
        if let Some(federation) = &ctx.federation {
            for key in ["peerId", "localParticipants"] {
                room[key] = federation[key].clone();
            }
            room["federated"] = json!(true);
            room["remoteMembers"] = json!(
                federation["remoteMembers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|person| {
                        let mut p = person.clone();
                        p["id"] = json!(format!(
                            "remote:{}:{}",
                            federation["peerId"].as_str().unwrap_or_default(),
                            person["id"].as_str().unwrap_or_default()
                        ));
                        p
                    })
                    .collect::<Vec<_>>()
            );
        }
        (room, vec![room_id.to_owned()])
    };
    let mut names = context
        .agents()
        .iter()
        .filter_map(|a| {
            Some((
                a.id.clone()?,
                a.name
                    .value_or(None)
                    .cloned()
                    .map_or(Value::Null, Value::String),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    for person in room["remoteMembers"].as_array().into_iter().flatten() {
        if let Some(id) = person["id"].as_str().filter(|_| truthy(person.get("name"))) {
            names.insert(id.to_owned(), person["name"].clone());
        }
    }
    let mut messages = Vec::new();
    if !room_ids.is_empty() {
        let mut statement = connection.prepare_cached(&format!(
            "SELECT seq,id,room,sender,text,created,deliveries FROM runtime_chat_messages
             WHERE room IN (SELECT value FROM json_each(?1))
             AND (?2 IS NULL OR seq<?2) AND (?3 IS NULL OR seq>?3)
             ORDER BY seq {} LIMIT ?4",
            if after.is_some() { "ASC" } else { "DESC" }
        ))?;
        let rows = statement.query_map(
            rusqlite::params![
                serde_json::to_string(&room_ids).map_err(studio_runtime_store::StoreError::from)?,
                before,
                after,
                (limit + 1) as i64
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, f64>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )?;
        let mut page_bytes = 0usize;
        let mut more = false;
        for row in rows {
            let (seq, id, message_room, sender, text, created, deliveries) = row?;
            if messages.len() == limit {
                more = true;
                break;
            }
            page_bytes += text.len() + deliveries.len() + 256;
            if !messages.is_empty() && page_bytes > 1_000_000 {
                more = true;
                break;
            }
            let deliveries = message_deliveries(connection, &id, &deliveries)?;
            if seq < 1
                || seq > studio_runtime_schema::shared_values::MAX_SAFE_INTEGER
                || !created.is_finite()
            {
                return Err(records::invalid("runtime_chat_messages", &id).into());
            }
            let sender_name = names
                .get(&sender)
                .cloned()
                .unwrap_or_else(|| Value::String(sender.clone()));
            messages.push(ChatMessage {
                seq,
                id,
                room: message_room,
                sender,
                text,
                created,
                deliveries,
                sender_name,
            });
        }
        if after.is_none() {
            messages.reverse();
        }
        let next_before = (after.is_none() && more)
            .then(|| messages.first().map(|m| m.seq))
            .flatten();
        let next_after = (if after.is_some() {
            more
        } else {
            before.is_some()
        })
        .then(|| messages.last().map(|m| m.seq))
        .flatten();
        return Ok(ChatRead {
            room,
            messages,
            next_before,
            next_after,
        });
    }
    Ok(ChatRead {
        room,
        messages,
        next_before: None,
        next_after: None,
    })
}

pub fn delete_room(
    tx: &Transaction<'_>,
    id: &str,
    validate: impl Fn(&Value) -> bool,
) -> Result<Value, CoordinationError> {
    let mut room = records::get(tx, "runtime_rooms", id)?
        .ok_or(CoordinationError::InvalidRequest("Unknown agent chat"))?;
    let changed = room["userHidden"] != true;
    room["userHidden"] = json!(true);
    records::validate("runtime_rooms", id, room.clone())?;
    let directory = RoomDirectory::load(tx)?;
    let context = directory.for_room(tx, &room, true)?;
    let projection = room_entity_view(&room, &context).filter(|v| {
        v["members"]
            .as_array()
            .is_some_and(|members| !members.is_empty())
    });
    if projection.as_ref().is_some_and(|v| !validate(v)) {
        return Err(records::invalid("runtime_rooms", id).into());
    }
    if changed {
        tx.execute(
            "UPDATE runtime_rooms SET record=?1 WHERE id=?2",
            rusqlite::params![room.to_string(), id],
        )?;
    }
    let deleted = projection.is_none();
    if !deleted || tx.query_row("SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='room' AND id=?1 AND deleted=0)", [id], |r| r.get::<_,bool>(0))? {
        SyncDocumentsRepository::put_entity(tx, "room", id, projection.unwrap_or(json!({})), deleted)?;
    }
    Ok(json!({"deleted":[id]}))
}

impl From<rusqlite::Error> for CoordinationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}

fn message_deliveries(
    connection: &rusqlite::Connection,
    id: &str,
    deliveries: &str,
) -> Result<BTreeMap<String, RecipientDelivery>, studio_runtime_store::StoreError> {
    let mut deliveries: BTreeMap<String, RecipientDelivery> = serde_json::from_str(deliveries)
        .map_err(|_| records::invalid("runtime_chat_messages", id))?;
    for (recipient, delivery) in &mut deliveries {
        if delivery.status != DeliveryStatus::Queued {
            continue;
        }
        if let Some(event) = connection
            .query_row(
                "SELECT status,error FROM runtime_events WHERE id=?1",
                [format!("chat:{id}:{recipient}")],
                |row| {
                    Ok((
                        row.get::<_, EventStatus>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?
        {
            let status = match event.0 {
                EventStatus::Delivered => DeliveryStatus::Delivered,
                EventStatus::Failed => DeliveryStatus::Failed,
                EventStatus::Cancelled => DeliveryStatus::Cancelled,
                EventStatus::StoredOnly => DeliveryStatus::StoredOnly,
                EventStatus::Pending
                | EventStatus::Reserved
                | EventStatus::Dispatching
                | EventStatus::Uncertain => continue,
            };
            let event_id = format!("chat:{id}:{recipient}");
            records::validate(
                "runtime_events",
                &event_id,
                Value::Object(serde_json::Map::from_iter([
                    ("id".into(), Value::String(event_id.clone())),
                    ("status".into(), Value::String(event.0.as_str().into())),
                    (
                        "error".into(),
                        event.1.clone().map_or(Value::Null, Value::String),
                    ),
                ])),
            )?;
            delivery.status = status;
            delivery.error = event.1;
        }
    }
    Ok(deliveries)
}

#[cfg(test)]
mod delivery_tests {
    use super::*;

    #[test]
    fn typed_deliveries_reconcile_terminal_errors_without_rewriting_active_events() {
        let db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch(
            "CREATE TABLE runtime_events(id TEXT,status TEXT,error TEXT);
            INSERT INTO runtime_events VALUES('chat:m:failed','failed','provider refused'),
            ('chat:m:active','reserved',NULL),('chat:m:done','delivered',NULL);",
        )
        .expect("events");
        let saved = json!({
            "failed":{"status":"queued","error":null},
            "active":{"status":"queued","error":null},
            "done":{"status":"accepted","error":"saved"}
        });
        let deliveries = message_deliveries(&db, "m", &saved.to_string()).expect("typed states");
        assert_eq!(deliveries["failed"].status, DeliveryStatus::Failed);
        assert_eq!(
            deliveries["failed"].error.as_deref(),
            Some("provider refused")
        );
        assert_eq!(deliveries["active"].status, DeliveryStatus::Queued);
        assert_eq!(deliveries["done"].status, DeliveryStatus::Accepted);
        assert_eq!(deliveries["done"].error.as_deref(), Some("saved"));
    }

    #[test]
    fn delivery_reader_rejects_flat_and_unknown_states() {
        let db = rusqlite::Connection::open_in_memory().expect("database");
        for saved in [
            json!({"a":"queued"}),
            json!({"a":{"status":"future","error":null}}),
        ] {
            assert!(message_deliveries(&db, "m", &saved.to_string()).is_err());
        }
    }
}
