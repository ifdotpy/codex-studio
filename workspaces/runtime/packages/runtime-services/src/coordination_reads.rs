//! Snapshot readers and user-controlled room visibility using canonical room views.
use crate::{
    coordination::CoordinationError,
    event_write::truthy,
    sync_views::{RoomViewContext, room_entity_view},
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
    let unique = ids.iter().collect::<BTreeSet<_>>();
    let mut items = Vec::new();
    if !unique.is_empty() {
        let placeholders = std::iter::repeat_n("?", unique.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id,status,error FROM runtime_events WHERE agent=? AND kind='user' AND id IN ({placeholders})"
        );
        let mut parameters = vec![agent.to_owned()];
        parameters.extend(unique.iter().map(|id| (*id).clone()));
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (id, status, error) = row?;
            records::validate(
                "runtime_events",
                &id,
                json!({"id":id,"status":status,"error":error}),
            )?;
            let metadata = records::get(connection, "runtime_event_meta", &id)?;
            let direct_id = format!("{agent}:{id}");
            let item_id = metadata
                .as_ref()
                .and_then(|m| m["transcriptItemId"].as_str())
                .unwrap_or(&direct_id);
            let item = connection
                .query_row(
                    "SELECT record FROM runtime_items WHERE id=?1 AND agent=?2",
                    rusqlite::params![item_id, agent],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            let item = item
                .map(|raw| records::decode("runtime_items", item_id, &raw))
                .transpose()?;
            let materialized = item.is_some_and(|item| {
                (item_id == direct_id && item["role"] == "user" && item.get("inputs").is_none())
                    || item["inputs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|input| input["id"] == id && input["kind"] == "user")
            });
            items.push(json!({"id":id,"status":status,"error":error,"materialized":materialized}));
        }
    }
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
        let ids = rooms
            .iter()
            .filter(|room| {
                let ids = room["members"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                (room["kind"] == "broadcast" && room["rootId"] == root_id)
                    || (room["kind"] != "broadcast"
                        && !ids.is_empty()
                        && ids.iter().all(|id| members.contains(id)))
                    || (room["kind"] == "private"
                        && ids.len() == 2
                        && context
                            .projects
                            .iter()
                            .flat_map(|p| p["peerTeams"].as_array().into_iter().flatten())
                            .any(|team| {
                                let peers = team["members"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(Value::as_str)
                                    .collect::<BTreeSet<_>>();
                                members.iter().any(|id| peers.contains(id))
                                    && ids.iter().all(|id| peers.contains(id))
                            }))
            })
            .filter_map(|r| r["id"].as_str().map(str::to_owned))
            .collect();
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
        if let Some(id) = person["id"].as_str() {
            names.insert(id.to_owned(), person["name"].clone());
        }
    }
    let mut messages = Vec::new();
    if !room_ids.is_empty() {
        let placeholders = std::iter::repeat_n("?", room_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let cursor_clause = if after.is_some() {
            "AND seq>?"
        } else {
            "AND (? IS NULL OR seq<?)"
        };
        let sql = format!(
            "SELECT seq,id,room,sender,text,created,deliveries FROM runtime_chat_messages WHERE room IN ({placeholders}) {cursor_clause} ORDER BY seq {} LIMIT ?",
            if after.is_some() { "ASC" } else { "DESC" }
        );
        let mut statement = connection.prepare(&sql)?;
        let mut params = room_ids
            .iter()
            .map(|id| rusqlite::types::Value::Text(id.clone()))
            .collect::<Vec<_>>();
        if let Some(cursor) = after {
            params.push(rusqlite::types::Value::Integer(cursor));
        } else {
            params.push(before.map_or(
                rusqlite::types::Value::Null,
                rusqlite::types::Value::Integer,
            ));
            params.push(before.map_or(
                rusqlite::types::Value::Null,
                rusqlite::types::Value::Integer,
            ));
        }
        params.push(rusqlite::types::Value::Integer((limit + 1) as i64));
        let rows = statement.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, f64>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        let mut page_bytes = 0usize;
        let mut more = false;
        for row in rows {
            let (seq, id, room, sender, text, created, deliveries) = row?;
            if messages.len() == limit {
                more = true;
                break;
            }
            page_bytes += text.len() + deliveries.len() + 256;
            if !messages.is_empty() && page_bytes > 1_000_000 {
                more = true;
                break;
            }
            let deliveries: Value = serde_json::from_str(&deliveries)
                .map_err(|_| records::invalid("runtime_chat_messages", &id))?;
            let mut message = records::validate(
                "runtime_chat_messages",
                &id,
                json!({"seq":seq,"id":id,"room":room,"sender":sender,"text":text,"created":created,"deliveries":deliveries}),
            )?;
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
        let next_before = if !messages.is_empty() && after.is_none() && more {
            messages
                .first()
                .and_then(|m| m.get("seq"))
                .cloned()
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let next_after = if after.is_some() {
            if more {
                messages
                    .last()
                    .and_then(|m| m.get("seq"))
                    .cloned()
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        } else if before.is_some() {
            messages
                .last()
                .and_then(|m| m.get("seq"))
                .cloned()
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
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
    tx.execute(
        "UPDATE runtime_rooms SET record=?1 WHERE id=?2",
        rusqlite::params![room.to_string(), id],
    )?;
    let deleted = projection.is_none();
    if !deleted || tx.query_row("SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='room' AND id=?1 AND deleted=0)", [id], |r| r.get::<_,bool>(0))? {
        SyncDocumentsRepository::put_entity(tx, "room", id, projection.unwrap_or(json!({})), deleted)?;
    }
    Ok(json!({"deleted":[id]}))
}

struct RoomDirectory {
    agents: Vec<Value>,
    projects: Vec<Value>,
    peers: Vec<Value>,
}
impl RoomDirectory {
    fn load(tx: &Transaction<'_>) -> Result<Self, CoordinationError> {
        let agents = records::all(tx, "runtime_agents")?;
        let projects = records::all(tx, "runtime_projects")?;
        let mut peers = Vec::new();
        for project in &projects {
            let Some(path) = project["path"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            let path = crate::system_reads::resolve_path(path).map_err(|_| {
                records::invalid(
                    "runtime_projects",
                    project["id"].as_str().unwrap_or_default(),
                )
            })?;
            for team in project["peerTeams"].as_array().into_iter().flatten() {
                let mut members = Vec::new();
                for id in team["members"].as_array().into_iter().flatten() {
                    if agents.iter().any(|a| {
                        a["id"] == *id
                            && a["rootId"] == *id
                            && a["isLead"] == true
                            && !truthy(a.get("deletedAt"))
                            && !truthy(a.get("parentId"))
                            && a["cwd"]
                                .as_str()
                                .and_then(|p| crate::system_reads::resolve_path(p).ok())
                                .as_ref()
                                == Some(&path)
                    }) && !members.contains(id)
                    {
                        members.push(id.clone());
                    }
                }
                if members.len() >= 2 {
                    let mut team = team.clone();
                    team["members"] = json!(members);
                    peers.push(team);
                }
            }
        }
        Ok(Self {
            agents,
            projects,
            peers,
        })
    }
    fn for_room(
        &self,
        tx: &Transaction<'_>,
        room: &Value,
        last: bool,
    ) -> Result<RoomViewContext, CoordinationError> {
        let id = room["id"].as_str().unwrap_or_default();
        let last_message = if last {
            let row=tx.query_row("SELECT id,seq,text,created,sender,deliveries FROM runtime_chat_messages WHERE room=?1 ORDER BY seq DESC LIMIT 1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,f64>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?))).optional()?;
            row.map(|(key,seq,text,created,sender,deliveries)|{
                let deliveries:Value=serde_json::from_str(&deliveries).map_err(|_|records::invalid("runtime_chat_messages",&key))?;
                records::validate("runtime_chat_messages",&key,json!({"id":key,"seq":seq,"room":id,"text":text,"created":created,"sender":sender,"deliveries":deliveries}))?;
                Ok::<Value,studio_runtime_store::StoreError>(json!({"seq":seq,"text":text.chars().take(180).collect::<String>(),"created":created,"sender":sender}))
            }).transpose()?
        } else {
            None
        };
        Ok(RoomViewContext {
            agents: self.agents.clone(),
            peer_teams: self.peers.clone(),
            last_message,
            federation: if room["kind"] == "federated" {
                records::get(tx, "runtime_federation_rooms", id)?
            } else {
                None
            },
        })
    }
}
impl From<rusqlite::Error> for CoordinationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}
