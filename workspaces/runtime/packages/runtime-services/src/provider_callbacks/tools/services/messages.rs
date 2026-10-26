use super::*;
use crate::submit_gates::{
    AuthorizationRecords, AuthorizationSnapshot, StoredEvent, authorize_events,
};
use std::collections::BTreeMap;
use studio_runtime_schema::delivery::DeliveryMoment;
use studio_runtime_store::{AgentId, AgentRepository, SyncDocumentsRepository};

pub(super) fn authorization_records(
    tx: &Transaction<'_>,
) -> Result<AuthorizationRecords, CallbackError> {
    // The gate reads only resolved paths. This read does no filesystem I/O,
    // so the resolution is `path_identity::lexical`: an equivalent
    // spelling, such as a trailing separator, gets the same identity. A path
    // without such a form stays unresolved, and the gate refuses its record.
    let agents = records::all(tx, "runtime_agents")?;
    let mut paths = BTreeMap::new();
    for actor in &agents {
        if let Some(path) = actor["cwd"].as_str()
            && let Some(resolved) = studio_runtime_store::path_identity::lexical_text(path)
        {
            paths.insert(path.into(), resolved.to_string_lossy().into_owned());
        }
    }
    let projects = records::all(tx, "runtime_projects")?;
    for project in &projects {
        if let Some(path) = project["path"].as_str()
            && let Some(resolved) = studio_runtime_store::path_identity::lexical_text(path)
        {
            paths.insert(path.into(), resolved.to_string_lossy().into_owned());
        }
    }
    Ok(AuthorizationRecords {
        recipient_id: String::new(),
        agents: agents
            .into_iter()
            .map(|a| Ok((text(&a, "id")?.to_owned(), a.to_string())))
            .collect::<Result<_, CallbackError>>()?,
        rooms: BTreeMap::new(),
        complaints: BTreeMap::new(),
        projects: projects_by_resolved_path(projects, &paths)?,
        event_meta: BTreeMap::new(),
        events: Vec::new(),
        resolved_paths: paths,
    })
}
/// The gate keys a project by its resolved path. When two records name one
/// directory, the record whose stored path is already that form wins, so a
/// second spelling never replaces the authority of an exact record.
fn projects_by_resolved_path(
    projects: Vec<Value>,
    paths: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, CallbackError> {
    let mut exact = BTreeMap::new();
    let mut respelled = BTreeMap::new();
    for project in projects {
        let path = text(&project, "path")?;
        match paths.get(path) {
            Some(resolved) if resolved != path => {
                respelled
                    .entry(resolved.clone())
                    .or_insert_with(|| project.to_string());
            }
            _ => {
                exact.insert(path.to_owned(), project.to_string());
            }
        }
    }
    for (key, project) in respelled {
        exact.entry(key).or_insert(project);
    }
    Ok(exact)
}
fn authorize(
    rows: &mut AuthorizationRecords,
    recipient: &str,
    sender: &str,
    room: &Value,
) -> Result<(), CallbackError> {
    let room_id = text(room, "id")?;
    rows.recipient_id = recipient.into();
    rows.rooms = BTreeMap::from([(room_id.into(), room.to_string())]);
    rows.events = vec![StoredEvent {
        id: "callback-authority".into(),
        kind: "agent_message".into(),
        text: json!({"sender":sender,"room":room_id}).to_string(),
    }];
    let snapshot =
        AuthorizationSnapshot::from_records(rows).map_err(|e| CallbackError::Stored(e.location))?;
    authorize_events(&snapshot).map_err(|_| {
        CallbackError::Invalid("Communication is limited to your team or user-grouped peer chat")
    })
}
pub(super) fn authorize_room(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    id: &str,
) -> Result<(), CallbackError> {
    let mut rows = authorization_records(tx)?;
    let directory = crate::sync_entity_context::RoomDirectory::load(tx)?;
    authorize_room_with(tx, snapshot, id, &mut rows, &directory)
}
fn authorize_room_with(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    id: &str,
    rows: &mut AuthorizationRecords,
    directory: &crate::sync_entity_context::RoomDirectory,
) -> Result<(), CallbackError> {
    if id.starts_with("feed:") {
        return Err(CallbackError::Invalid(
            "The combined feed is available only in the user interface",
        ));
    }
    let room = records::get(tx, "runtime_rooms", id)?
        .ok_or(CallbackError::Invalid("Unknown agent chat"))?;
    let view = crate::sync_views::room_entity_view(&room, &directory.for_room(tx, &room, false)?)
        .ok_or(CallbackError::Invalid("Chat room is no longer visible"))?;
    if !view["members"]
        .as_array()
        .is_some_and(|members| members.iter().any(|id| id == &snapshot.context().agent))
    {
        return Err(CallbackError::Invalid("Chat room is no longer visible"));
    }
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
    authorize(rows, &snapshot.context().agent, sender, &room)
}
pub(super) fn send(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    args: &Value,
    id: &str,
    now: f64,
) -> Result<Value, CallbackError> {
    send_as_actor(
        tx,
        snapshot.actor(),
        &snapshot.context().agent,
        args,
        id,
        now,
    )
}

pub(crate) fn send_remote_worker(
    tx: &Transaction<'_>,
    sender: &str,
    args: &Value,
    id: &str,
    now: f64,
) -> Result<Value, CallbackError> {
    let actor = records::get(tx, "runtime_agents", sender)?
        .ok_or(CallbackError::Invalid("The remote worker is unavailable"))?;
    if actor["id"] != sender
        || actor.get("autoWake") != Some(&Value::Bool(true))
        || studio_runtime_store::truthy(actor.get("deletedAt"))
    {
        return Err(CallbackError::Invalid("The worker was stopped"));
    }
    send_as_actor(tx, &actor, sender, args, id, now)
}

fn send_as_actor(
    tx: &Transaction<'_>,
    actor: &Value,
    who: &str,
    args: &Value,
    id: &str,
    now: f64,
) -> Result<Value, CallbackError> {
    let message = crate::event_write::python_strip(text(args, "text")?);
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
    let delivery = DeliveryMoment::for_message(args)
        .map_err(|_| CallbackError::Invalid("Invalid delivery moment"))?;
    let progress_key = args.get("progress_key").filter(|value| !value.is_null());
    let progress_version = args
        .get("progress_version")
        .filter(|value| !value.is_null());
    let versioned_progress = progress_key.is_some() || progress_version.is_some();
    if versioned_progress
        && (importance != "progress"
            || !progress_key
                .and_then(Value::as_str)
                .is_some_and(|key| (1..=200).contains(&key.chars().count()))
            || !progress_version.and_then(Value::as_u64).is_some())
    {
        return Err(CallbackError::Invalid(
            "Versioned progress requires a progress_key and nonnegative integer progress_version",
        ));
    }
    let target = match text(args, "target")? {
        "lead" => text(actor, "rootId")?,
        "parent" => text(actor, "parentId")?,
        target => target,
    };
    if target == "user" {
        return send_to_user(tx, actor, who, message, id, now);
    }
    if target == "all" {
        return Err(CallbackError::Invalid(
            "Communication is limited to one team; use broadcast for your team",
        ));
    }
    if studio_runtime_store::truthy(actor.get("remoteOrigin"))
        && (target.starts_with("remote:") || target.starts_with("federated:"))
    {
        return Err(CallbackError::Dependency("federation/agent-message"));
    }
    if target.starts_with("remote:") || target.starts_with("federated:") {
        let room = if let Some(peer) = target.strip_prefix("remote:") {
            let mut statement = tx.prepare_cached(
                "SELECT id FROM runtime_federation_rooms WHERE json_extract(record,'$.peerId')=?1 AND json_extract(record,'$.status')='approved' AND json_extract(record,'$.localApproved')=1 AND json_extract(record,'$.remoteApproved')=1 AND EXISTS(SELECT 1 FROM json_each(record,'$.localMembers') WHERE value=?2) ORDER BY id LIMIT 2",
            )?;
            let rooms = statement
                .query_map(params![peer, who], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            if rooms.len() != 1 {
                return Err(CallbackError::Invalid(
                    "Select an explicit remote room; this peer has zero or multiple approved rooms",
                ));
            }
            rooms[0].clone()
        } else {
            target.to_owned()
        };
        return crate::federation_delivery::send_message_in_transaction(
            tx, who, &room, message, id, delivery, now,
        )
        .map_err(map_federation_error);
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
        let mut ids = vec![who, target];
        ids.sort();
        json!({"id":format!("private:{}:{}",ids[0],ids[1]),"kind":"private","members":ids})
    };
    let mut authority = authorization_records(tx)?;
    let broadcast = target == "broadcast";
    let mut refused = serde_json::Map::new();
    let mut first_refusal = None;
    let mut admitted = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        match authorize(&mut authority, text(&recipient, "id")?, who, &room) {
            Ok(()) => {}
            // One member record that the gate cannot check must not refuse
            // the broadcast to the other members. The result names it.
            Err(CallbackError::Stored(location)) if broadcast => {
                let recipient_id = text(&recipient, "id")?;
                crate::diagnostics::log_stderr(format_args!(
                    "Broadcast {id} skips member {recipient_id}: invalid stored authority at {location}"
                ));
                refused.insert(
                    recipient_id.into(),
                    json!(crate::managed_host_tools::Error::StoredAt(location.clone()).to_string()),
                );
                first_refusal.get_or_insert(location);
                continue;
            }
            Err(error) => return Err(error),
        }
        if recipient["id"] != recipient["rootId"] {
            let root = event_write::checked_agent(tx, text(&recipient, "rootId")?)?;
            event_write::assert_delegation(&root)?;
        }
        // Python `codex_radio.guard_message`: only the sender's live shared
        // reply turn blocks a private message to a fellow room member.
        crate::radio_runtime::guard_message(tx, who, text(&recipient, "id")?)
            .map_err(radio_guard_error)?;
        admitted.push(recipient);
    }
    // When no member can be checked, nothing is sent.
    if admitted.is_empty()
        && let Some(location) = first_refusal
    {
        return Err(CallbackError::Stored(location));
    }
    let recipients = admitted;
    let room_id = text(&room, "id")?.to_owned();
    if let Some(mut previous) = records::get(tx, "runtime_rooms", &room_id)? {
        if studio_runtime_store::ProjectsRepository::room_retired(&previous) {
            return Err(CallbackError::Invalid(
                "This project chat was permanently deleted",
            ));
        }
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
    // Python projects the room immediately after writing runtime_rooms, before
    // it enqueues recipient events. Keep the shared global sync sequence in
    // that order; the room is projected again below after the chat message
    // supplies its new lastMessage.
    let directory = crate::sync_entity_context::RoomDirectory::load(tx)?;
    if let Some(view) =
        crate::sync_views::room_entity_view(&room, &directory.for_room(tx, &room, true)?)
    {
        SyncDocumentsRepository::put_entity(tx, "room", &room_id, view, false)?;
    }
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
            // Python `chat_message`: the version travels with the input, and
            // turn preparation keeps the latest version per sender, room and key.
            if let (Some(key), Some(version)) = (progress_key, progress_version) {
                payload["progress_key"] = key.clone();
                payload["progress_version"] = version.clone();
            }
            let event_id = format!("chat:{id}:{recipient_id}");
            studio_runtime_store::QueueWriteRepository::put_metadata(
                tx,
                &event_id,
                &json!({"delivery":delivery}),
            )?;
            event_write::enqueue_event(
                tx,
                &typed,
                event_id,
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
    let mut result = json!({"id":id,"room":room_id,"deliveries":deliveries});
    // A broadcast member that was not checked is named with its refusal.
    // The receipt keeps it, so a replay of this request reports it again.
    if !refused.is_empty() {
        result["refused"] = Value::Object(refused);
    }
    event_write::save_receipt(tx, Some(id), &event_write::request_signature(args), &result)?;
    Ok(result)
}

/// Python `codex_user_messages.send_to_user`: a lead's message to the user is a
/// user-addressed complaint record of source type `message`, saved with the
/// request receipt in the caller's transaction. `text` is already stripped and
/// bounded by `send_as_actor`.
fn send_to_user(
    tx: &Transaction<'_>,
    actor: &Value,
    who: &str,
    text: &str,
    id: &str,
    now: f64,
) -> Result<Value, CallbackError> {
    let key = crate::event_write::python_strip(id);
    if id.chars().count() > 200 || key.is_empty() {
        return Err(CallbackError::Invalid(
            "Supply a request id with 1 to 200 characters",
        ));
    }
    if actor["isLead"] != true
        || actor["rootId"] != who
        || studio_runtime_store::truthy(actor.get("parentId"))
    {
        return Err(CallbackError::Invalid(
            "Only the lead can contact the user. Send your message to your lead.",
        ));
    }
    let signature =
        event_write::request_signature(&json!({"sender":who,"target":"user","text":text}));
    if let Some(saved) = event_write::read_receipt(
        tx,
        Some(key),
        &signature,
        "runtime_operation_receipts",
        key,
        |saved| saved["recipient"] == "user" && saved["id"].is_string(),
    )? {
        return Ok(saved);
    }
    let message = crate::notification_dispatch::submit_user_message(tx, actor, key, text, now)
        .map_err(|error| match error {
            crate::notification_dispatch::NotificationDispatchError::Store(error) => {
                CallbackError::Store(error)
            }
            crate::notification_dispatch::NotificationDispatchError::ConflictingInput => {
                CallbackError::Invalid("This complaint id has different content")
            }
            crate::notification_dispatch::NotificationDispatchError::InvalidStoredObject {
                table,
                id,
            } => CallbackError::Stored(format!("{table}/{id}")),
            crate::notification_dispatch::NotificationDispatchError::Dependency(name) => {
                CallbackError::Dependency(name)
            }
            _ => CallbackError::Invalid("A complaint needs an existing lead"),
        })?;
    let result = json!({"id":message["id"],"room":format!("user:{who}"),"recipient":"user"});
    event_write::save_receipt(tx, Some(key), &signature, &result)?;
    Ok(result)
}

fn radio_guard_error(error: crate::projects::ProjectsError) -> CallbackError {
    use crate::projects::ProjectsError as E;
    match error {
        E::Invalid(message) => CallbackError::Invalid(message),
        E::Store(error) => CallbackError::Store(error),
        E::Sqlite(error) => CallbackError::Store(error.into()),
        E::InvalidStored { table, id } => CallbackError::Stored(format!("{table}/{id}")),
        _ => CallbackError::Stored("runtime_rooms radio".into()),
    }
}

fn map_federation_error(
    error: crate::federation_messaging::FederationMessageError,
) -> CallbackError {
    use crate::federation_messaging::FederationMessageError as FederationError;
    match error {
        FederationError::Store(error) => CallbackError::Store(error),
        FederationError::Unavailable(dependency) => CallbackError::Dependency(dependency),
        FederationError::Invalid(_)
        | FederationError::Forbidden(_)
        | FederationError::Conflict(_) => {
            CallbackError::Invalid("The remote room message was refused")
        }
        FederationError::Transport(_) | FederationError::Event(_) => {
            CallbackError::Dependency("federation-driver/rooms")
        }
    }
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
    let visible = visible_agents(tx, snapshot)?;
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
    let directory = crate::sync_entity_context::RoomDirectory::load(tx)?;
    let mut authority = authorization_records(tx)?;
    for room in records::all(tx, "runtime_rooms")? {
        let visible =
            match authorize_room_with(tx, snapshot, text(&room, "id")?, &mut authority, &directory)
            {
                Ok(()) => true,
                Err(CallbackError::Invalid(_)) => false,
                // As for agents, a room that the gate cannot check is not visible.
                Err(CallbackError::Stored(location)) => {
                    crate::diagnostics::log_stderr(format_args!(
                        "Room {} is not visible to {}: invalid stored authority at {location}",
                        text(&room, "id")?,
                        snapshot.context().agent
                    ));
                    false
                }
                Err(error) => return Err(error),
            };
        if visible {
            let Some(room) =
                crate::sync_views::room_entity_view(&room, &directory.for_room(tx, &room, false)?)
            else {
                continue;
            };
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

/// Visibility is shared by discovery and prefix resolution; hidden agents never
/// create ambiguity or become candidates merely because their IDs match.
fn visible_agents(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
) -> Result<Vec<Value>, CallbackError> {
    let mut visible = Vec::new();
    let mut authority = None;
    for agent in records::all(tx, "runtime_agents")? {
        if agent_visible(tx, snapshot, &agent, &mut authority)? {
            visible.push(agent);
        }
    }
    Ok(visible)
}

/// Whether the caller may see `agent`. An agent of another root is visible
/// only when the room gate admits a private room with it. A stored record
/// that the gate cannot check is not visible. That refusal is about one
/// record, so it must not refuse the caller's whole request. `authority` is
/// loaded at the first check that needs it.
fn agent_visible(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    agent: &Value,
    authority: &mut Option<AuthorizationRecords>,
) -> Result<bool, CallbackError> {
    if studio_runtime_store::truthy(agent.get("deletedAt")) {
        return Ok(false);
    }
    let actor = snapshot.actor();
    if agent["rootId"] == actor["rootId"] {
        return Ok(true);
    }
    let rows = match authority {
        Some(rows) => rows,
        None => authority.insert(authorization_records(tx)?),
    };
    let mut ids = vec![text(actor, "id")?, text(agent, "id")?];
    ids.sort();
    let room = json!({"id":"peer-check","kind":"private","members":ids});
    match authorize(rows, text(agent, "id")?, text(actor, "id")?, &room) {
        Ok(()) => Ok(true),
        Err(CallbackError::Invalid(_)) => Ok(false),
        Err(CallbackError::Stored(location)) => {
            crate::diagnostics::log_stderr(format_args!(
                "Agent {} is not visible to {}: invalid stored authority at {location}",
                text(agent, "id")?,
                text(actor, "id")?
            ));
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// An exact full ID is checked alone, so the records of other agents cannot
/// refuse it. Other text resolves as a unique prefix among visible agents.
pub(super) fn resolve_agent(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    supplied: &str,
) -> Result<String, CallbackError> {
    if let Some(agent) = records::get(tx, "runtime_agents", supplied)?
        && agent["id"] == supplied
        && agent_visible(tx, snapshot, &agent, &mut None)?
    {
        return Ok(supplied.to_owned());
    }
    resolve_visible_agent_prefix(
        visible_agents(tx, snapshot)?
            .into_iter()
            .filter_map(|agent| agent["id"].as_str().map(str::to_owned)),
        supplied,
    )
}

pub(super) fn resolve_visible_agent_prefix(
    visible_ids: impl IntoIterator<Item = String>,
    supplied: &str,
) -> Result<String, CallbackError> {
    if supplied.chars().count() < 8 {
        return Err(CallbackError::Invalid(
            "Too short agent ID; use at least 8 characters",
        ));
    }
    let mut matches = visible_ids
        .into_iter()
        .filter(|id| id.starts_with(supplied));
    let first = matches.next().ok_or(CallbackError::Invalid(
        "Unknown agent ID in this caller's scope",
    ))?;
    if matches.next().is_some() {
        return Err(CallbackError::Invalid(
            "Ambiguous agent ID; use a full ID from orchestration_peers",
        ));
    }
    Ok(first)
}

#[cfg(test)]
mod prefix_tests {
    use super::{CallbackError, resolve_visible_agent_prefix};

    #[test]
    fn visible_agent_prefix_requires_eight_characters_and_unique_match() {
        let ids = || {
            ["12345678-alpha", "87654321-beta"]
                .into_iter()
                .map(str::to_owned)
        };
        assert_eq!(
            resolve_visible_agent_prefix(ids(), "12345678")
                .expect("unique eight-character prefix resolves"),
            "12345678-alpha"
        );
        assert!(matches!(
            resolve_visible_agent_prefix(ids(), "1234567"),
            Err(CallbackError::Invalid(
                "Too short agent ID; use at least 8 characters"
            ))
        ));
    }

    #[test]
    fn visible_agent_prefix_refuses_ambiguous_match() {
        let ids = ["12345678-alpha", "12345678-beta"]
            .into_iter()
            .map(str::to_owned);
        assert!(matches!(
            resolve_visible_agent_prefix(ids, "12345678"),
            Err(CallbackError::Invalid(
                "Ambiguous agent ID; use a full ID from orchestration_peers"
            ))
        ));
    }
}
