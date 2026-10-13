use super::*;
use crate::submit_gates::{
    AuthorizationRecords, AuthorizationSnapshot, StoredEvent, authorize_events,
};
use std::collections::BTreeMap;
use studio_runtime_store::{AgentId, AgentRepository, SyncDocumentsRepository};

fn authorize(
    tx: &Transaction<'_>,
    recipient: &str,
    sender: &str,
    room: &Value,
) -> Result<(), CallbackError> {
    let agents = records::all(tx, "runtime_agents")?;
    let mut paths = BTreeMap::new();
    for actor in &agents {
        if let Some(path) = actor["cwd"].as_str() {
            paths.insert(path.into(), path.into());
        }
    }
    let projects = records::all(tx, "runtime_projects")?;
    for project in &projects {
        if let Some(path) = project["path"].as_str() {
            paths.insert(path.into(), path.into());
        }
    }
    let room_id = text(room, "id")?;
    let rows = AuthorizationRecords {
        recipient_id: recipient.into(),
        agents: agents
            .into_iter()
            .map(|a| Ok((text(&a, "id")?.to_owned(), a.to_string())))
            .collect::<Result<_, CallbackError>>()?,
        rooms: BTreeMap::from([(room_id.into(), room.to_string())]),
        complaints: BTreeMap::new(),
        projects: projects
            .into_iter()
            .map(|p| Ok((text(&p, "path")?.to_owned(), p.to_string())))
            .collect::<Result<_, CallbackError>>()?,
        event_meta: BTreeMap::new(),
        events: vec![StoredEvent {
            id: "callback-authority".into(),
            kind: "agent_message".into(),
            text: json!({"sender":sender,"room":room_id}).to_string(),
        }],
        resolved_paths: paths,
    };
    let snapshot = AuthorizationSnapshot::from_records(&rows)
        .map_err(|e| CallbackError::Stored(e.location))?;
    authorize_events(&snapshot).map_err(|_| {
        CallbackError::Invalid("Communication is limited to your team or user-grouped peer chat")
    })
}
pub(super) fn authorize_room(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    id: &str,
) -> Result<(), CallbackError> {
    if id.starts_with("feed:") {
        return Err(CallbackError::Invalid(
            "The combined feed is available only in the user interface",
        ));
    }
    let room = records::get(tx, "runtime_rooms", id)?
        .ok_or(CallbackError::Invalid("Unknown agent chat"))?;
    let sender = if room["kind"] == "private" {
        room["members"]
            .as_array()
            .and_then(|members| {
                members
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|id| *id != snapshot.context().agent)
            })
            .ok_or(CallbackError::Invalid("Invalid private room membership"))?
    } else {
        &snapshot.context().agent
    };
    authorize(tx, &snapshot.context().agent, sender, &room)
}
pub(super) fn send(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    args: &Value,
    id: &str,
    now: f64,
) -> Result<Value, CallbackError> {
    let actor = snapshot.actor();
    let who = &snapshot.context().agent;
    let message = text(args, "text")?.trim();
    if !(1..=12000).contains(&message.chars().count()) {
        return Err(CallbackError::Invalid(
            "Message must have 1 to 12000 characters",
        ));
    }
    let importance = args["importance"].as_str().unwrap_or("message");
    if !matches!(
        importance,
        "message" | "progress" | "question" | "blocker" | "result"
    ) {
        return Err(CallbackError::Invalid("Unknown message importance"));
    }
    if args.get("progress_key").is_some() || args.get("progress_version").is_some() {
        return Err(CallbackError::Dependency(
            "coordination/versioned-progress-delivery",
        ));
    }
    let target = match text(args, "target")? {
        "lead" => text(actor, "rootId")?,
        "parent" => text(actor, "parentId")?,
        target => target,
    };
    if target == "user" {
        return Err(CallbackError::Dependency("user-messages/agent-admission"));
    }
    if target == "all" {
        return Err(CallbackError::Invalid(
            "Communication is limited to one team; use broadcast for your team",
        ));
    }
    if target.starts_with("remote:") || target.starts_with("federated:") {
        return Err(CallbackError::Dependency("federation/agent-message"));
    }
    if target == who {
        return Err(CallbackError::Invalid("Select another agent"));
    }
    let agents = records::all(tx, "runtime_agents")?;
    let recipients = if target == "broadcast" {
        agents
            .into_iter()
            .filter(|a| {
                a["rootId"] == actor["rootId"]
                    && a["id"] != *who
                    && !studio_runtime_store::truthy(a.get("deletedAt"))
            })
            .collect::<Vec<_>>()
    } else {
        vec![
            agents
                .into_iter()
                .find(|a| a["id"] == target && !studio_runtime_store::truthy(a.get("deletedAt")))
                .ok_or(CallbackError::Invalid("Recipient conversation was deleted"))?,
        ]
    };
    let mut room = if target == "broadcast" {
        json!({"id":format!("broadcast:{}",text(actor,"rootId")?),"kind":"broadcast","rootId":actor["rootId"]})
    } else {
        let mut ids = vec![who.as_str(), target];
        ids.sort();
        json!({"id":format!("private:{}:{}",ids[0],ids[1]),"kind":"private","members":ids})
    };
    for recipient in &recipients {
        authorize(tx, text(recipient, "id")?, who, &room)?;
        if recipient["id"] != recipient["rootId"] {
            let root = event_write::checked_agent(tx, text(recipient, "rootId")?)?;
            event_write::assert_delegation(&root)?;
        }
        let radio: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_rooms WHERE json_extract(record,'$.radio.active.agentId')=?1 AND EXISTS(SELECT 1 FROM json_each(record,'$.members') WHERE value=?2))", params![who,text(recipient,"id")?], |row|row.get(0))?;
        if radio {
            return Err(CallbackError::Dependency("radio/active-turn-message-guard"));
        }
    }
    let room_id = text(&room, "id")?.to_owned();
    if let Some(mut previous) = records::get(tx, "runtime_rooms", &room_id)? {
        let fields = room
            .as_object()
            .ok_or(CallbackError::Invalid("Invalid room"))?;
        for (key, value) in fields {
            previous[key] = value.clone();
        }
        room = previous;
    }
    room["updated"] = json!(now);
    room["userHidden"] = json!(false);
    records::validate("runtime_rooms", &room_id, room.clone())?;
    tx.execute("INSERT INTO runtime_rooms(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![room_id,room.to_string()])?;
    let mut deliveries = serde_json::Map::new();
    for recipient in recipients {
        let recipient_id = text(&recipient, "id")?;
        let active = recipient["autoWake"] == true
            && !crate::sync_entity_context::agent_view_context(tx, &recipient, None)?.empty_lead
            && !(target == "broadcast"
                && (studio_runtime_store::truthy(recipient.get("nativeFailureHold"))
                    || !matches!(
                        recipient["status"].as_str(),
                        Some("queued" | "starting" | "running" | "waiting" | "approval")
                    )));
        if active {
            let typed = AgentRepository::get(tx, &AgentId::from(recipient_id))?
                .ok_or(CallbackError::Stale)?;
            let mut payload = json!({"room":room_id,"message_id":id,"sender":who,"sender_name":actor["name"],"text":message});
            if importance != "message" {
                payload["importance"] = json!(importance);
            }
            event_write::enqueue_event(
                tx,
                &typed,
                format!("chat:{id}:{recipient_id}"),
                "agent_message",
                payload.to_string(),
                now,
            )?;
        }
        deliveries.insert(
            recipient_id.into(),
            json!(if active { "queued" } else { "stored_only" }),
        );
    }
    tx.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?1,?2,?3,?4,?5,?6)",params![id,room_id,who,message,now,json!(deliveries).to_string()])?;
    let directory = crate::sync_entity_context::RoomDirectory::load(tx)?;
    let view = crate::sync_views::room_entity_view(&room, &directory.for_room(tx, &room, true)?)
        .ok_or_else(|| CallbackError::Stored(room_id.clone()))?;
    SyncDocumentsRepository::put_entity(tx, "room", &room_id, view, false)?;
    let result = json!({"id":id,"room":room_id,"deliveries":deliveries});
    event_write::save_receipt(tx, Some(id), &event_write::request_signature(args), &result)?;
    Ok(result)
}
pub(super) fn peers(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    args: &Value,
) -> Result<Value, CallbackError> {
    if args["scope"].as_str().is_some_and(|scope| scope != "team") {
        return Err(CallbackError::Invalid(
            "Agent discovery is limited to your team",
        ));
    }
    let actor = snapshot.actor();
    let root = text(actor, "rootId")?;
    let mut visible = Vec::new();
    for agent in records::all(tx, "runtime_agents")? {
        if studio_runtime_store::truthy(agent.get("deletedAt")) {
            continue;
        }
        let mut ids = vec![text(actor, "id")?, text(&agent, "id")?];
        ids.sort();
        let room = json!({"id":"peer-check","kind":"private","members":ids});
        let allowed = if agent["rootId"] == root {
            true
        } else {
            match authorize(tx, text(&agent, "id")?, text(actor, "id")?, &room) {
                Ok(()) => true,
                Err(CallbackError::Invalid(_)) => false,
                Err(error) => return Err(error),
            }
        };
        if allowed {
            visible.push(agent);
        }
    }
    let mut agents = visible
        .into_iter()
        .map(|a| {
            Value::Object(
                ["id", "name", "role", "rootId", "parentId", "status"]
                    .into_iter()
                    .map(|key| (key.into(), a[key].clone()))
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    agents.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let mut rooms = Vec::new();
    for room in records::all(tx, "runtime_rooms")? {
        let visible = match authorize_room(tx, snapshot, text(&room, "id")?) {
            Ok(()) => true,
            Err(CallbackError::Invalid(_)) => false,
            Err(error) => return Err(error),
        };
        if visible {
            rooms.push(Value::Object(
                [
                    "id",
                    "kind",
                    "members",
                    "rootId",
                    "peerTeamId",
                    "peerTeamName",
                    "federated",
                    "peerId",
                    "peerLabel",
                    "localMembers",
                ]
                .into_iter()
                .map(|key| (key.into(), room[key].clone()))
                .collect(),
            ));
        }
    }
    rooms.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let entries = agents
        .iter()
        .map(|a| json!({"entry":"agent","value":a}))
        .chain(rooms.iter().map(|r| json!({"entry":"room","value":r})))
        .collect::<Vec<_>>();
    let mut result = page(entries, args, json!([snapshot.context().agent, "team"]))?;
    let items = result["items"].as_array().cloned().unwrap_or_default();
    result["items"] = json!(
        items
            .iter()
            .filter(|r| r["entry"] == "agent")
            .map(|r| r["value"].clone())
            .collect::<Vec<_>>()
    );
    result["rooms"] = json!(
        items
            .iter()
            .filter(|r| r["entry"] == "room")
            .map(|r| r["value"].clone())
            .collect::<Vec<_>>()
    );
    result["self"] = actor["id"].clone();
    result["lead"] = actor["rootId"].clone();
    result["total"] = json!(agents.len());
    result["roomTotal"] = json!(rooms.len());
    result["totalRecords"] = json!(agents.len() + rooms.len());
    if !actor["parentId"].is_null() {
        result["parent"] = actor["parentId"].clone();
    }
    Ok(result)
}
