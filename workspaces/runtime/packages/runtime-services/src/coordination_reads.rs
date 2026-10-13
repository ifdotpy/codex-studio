//! Snapshot readers and user-controlled room visibility using canonical room views.
use crate::{
    coordination::CoordinationError, event_write::truthy, sync_entity_context::RoomDirectory,
    sync_views::room_entity_view,
};
use rusqlite::{OptionalExtension, Transaction};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
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
    crate::event_write::checked_agent(connection, agent).map_err(|error| match error {
        crate::event_write::EventWriteError::Store(error) => CoordinationError::Store(error),
        _ => CoordinationError::InvalidRequest("Unknown or deleted managed agent"),
    })?;
    let mut events = connection.prepare(
        "SELECT e.id,e.status,e.error,m.record FROM runtime_events e
        LEFT JOIN runtime_event_meta m ON m.id=e.id
        WHERE e.agent=?1 AND e.kind='user' AND e.id IN (SELECT value FROM json_each(?2))",
    )?;
    let selected = events
        .query_map(rusqlite::params![agent, json!(ids).to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .map(|row| {
            let (id, status, error, raw) = row?;
            let event = records::validate(
                "runtime_events",
                &id,
                json!({"id":id,"status":status,"error":error}),
            )?;
            let metadata = raw
                .map(|raw| records::decode("runtime_event_meta", &id, &raw))
                .transpose()?;
            let item_id = metadata
                .as_ref()
                .and_then(|m| m["transcriptItemId"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{agent}:{id}"));
            Ok::<_, studio_runtime_store::StoreError>((event, item_id))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let ids = selected.iter().map(|(_, id)| id).collect::<Vec<_>>();
    let mut statement = connection.prepare("SELECT id,record FROM runtime_items WHERE agent=?1 AND id IN (SELECT value FROM json_each(?2))")?;
    let items = statement
        .query_map(rusqlite::params![agent, json!(ids).to_string()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .map(|row| {
            let (id, raw) = row?;
            let item = records::decode("runtime_items", &id, &raw)?;
            Ok((id, item))
        })
        .collect::<Result<BTreeMap<_, _>, studio_runtime_store::StoreError>>()?;
    let items = selected
        .into_iter()
        .map(|(mut event, item_id)| {
            let materialized = items.get(&item_id).is_some_and(|item| {
                (item_id == format!("{agent}:{}", event["id"].as_str().unwrap_or_default())
                    && item["role"] == "user"
                    && item.get("inputs").is_none())
                    || item["inputs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|input| input["id"] == event["id"] && input["kind"] == "user")
            });
            event["materialized"] = json!(materialized);
            event
        })
        .collect::<Vec<_>>();
    Ok(json!({"agent":agent,"items":items}))
}

pub fn chat_read(
    connection: &Transaction<'_>,
    room_id: &str,
    before: Option<i64>,
    after: Option<i64>,
    requested_limit: i64,
) -> Result<Value, CoordinationError> {
    if before.is_some() && after.is_some() {
        return Err(CoordinationError::InvalidRequest("Use one message cursor"));
    }
    let limit = requested_limit.clamp(1, 100) as usize;
    let context = RoomDirectory::load(connection)?;
    let (room, room_ids) = if let Some(root_id) = room_id.strip_prefix("feed:") {
        let root = context
            .agents
            .iter()
            .find(|a| a["id"] == root_id && a["isLead"] == true && !truthy(a.get("deletedAt")))
            .ok_or(CoordinationError::InvalidRequest("Team is unavailable"))?;
        let members = context
            .agents
            .iter()
            .filter(|a| a["rootId"] == root_id && !truthy(a.get("deletedAt")))
            .filter_map(|a| a["id"].as_str())
            .collect::<BTreeSet<_>>();
        let rooms = records::all(connection, "runtime_rooms")?;
        let mut selection = connection.prepare("WITH
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
                    json!(members).to_string(),
                    json!(context.projects).to_string(),
                    json!(rooms).to_string(),
                    root_id
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        (
            json!({"id":room_id,"name":root["name"],"members":members}),
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
        .agents
        .iter()
        .filter_map(|a| Some((a["id"].as_str()?.to_owned(), a["name"].clone())))
        .collect::<BTreeMap<_, _>>();
    for person in room["remoteMembers"].as_array().into_iter().flatten() {
        if let Some(id) = person["id"].as_str().filter(|_| truthy(person.get("name"))) {
            names.insert(id.to_owned(), person["name"].clone());
        }
    }
    let mut messages = Vec::new();
    if !room_ids.is_empty() {
        let mut statement = connection.prepare(&format!(
            "SELECT seq,id,room,sender,text,created,deliveries FROM runtime_chat_messages
             WHERE room IN (SELECT value FROM json_each(?1))
             AND (?2 IS NULL OR seq<?2) AND (?3 IS NULL OR seq>?3)
             ORDER BY seq {} LIMIT ?4",
            if after.is_some() { "ASC" } else { "DESC" }
        ))?;
        let rows = statement.query_map(
            rusqlite::params![
                json!(room_ids).to_string(),
                before,
                after,
                (limit + 1) as i64
            ],
            |row| {
                Ok(
                    json!({"seq":row.get::<_,i64>(0)?,"id":row.get::<_,String>(1)?,
                "room":row.get::<_,String>(2)?,"sender":row.get::<_,String>(3)?,
                "text":row.get::<_,String>(4)?,"created":row.get::<_,f64>(5)?,
                "deliveries":row.get::<_,String>(6)?}),
                )
            },
        )?;
        let mut page_bytes = 0usize;
        let mut more = false;
        for row in rows {
            let mut message = row?;
            let id = message["id"].as_str().unwrap_or_default().to_owned();
            let sender = message["sender"].as_str().unwrap_or_default().to_owned();
            let deliveries = message["deliveries"].as_str().unwrap_or_default();
            if messages.len() == limit {
                more = true;
                break;
            }
            page_bytes +=
                message["text"].as_str().unwrap_or_default().len() + deliveries.len() + 256;
            if !messages.is_empty() && page_bytes > 1_000_000 {
                more = true;
                break;
            }
            message["deliveries"] = serde_json::from_str(deliveries)
                .map_err(|_| records::invalid("runtime_chat_messages", &id))?;
            let mut message = records::validate("runtime_chat_messages", &id, message)?;
            let deliveries = &mut message["deliveries"];
            if let Some(states) = deliveries.as_object_mut() {
                for (recipient, status) in states.iter_mut() {
                    if status.as_str() != Some("queued") {
                        continue;
                    }
                    if let Some(event) = connection
                        .query_row(
                            "SELECT status FROM runtime_events WHERE id=?1",
                            [format!("chat:{id}:{recipient}")],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?
                        && matches!(
                            event.as_str(),
                            "delivered" | "failed" | "cancelled" | "stored_only"
                        )
                    {
                        let event_id = format!("chat:{id}:{recipient}");
                        records::validate(
                            "runtime_events",
                            &event_id,
                            json!({"id":event_id,"status":event}),
                        )?;
                        *status = Value::String(event);
                    }
                }
            }
            message["senderName"] = names.get(&sender).cloned().unwrap_or(json!(sender));
            messages.push(message);
        }
        if after.is_none() {
            messages.reverse();
        }
        let next_before = (after.is_none() && more)
            .then(|| messages.first().map(|m| &m["seq"]))
            .flatten();
        let next_after = (if after.is_some() {
            more
        } else {
            before.is_some()
        })
        .then(|| messages.last().map(|m| &m["seq"]))
        .flatten();
        return Ok(
            json!({"room":room,"messages":messages,"nextBefore":next_before,"nextAfter":next_after}),
        );
    }
    Ok(json!({"room":room,"messages":[],"nextBefore":null,"nextAfter":null}))
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
