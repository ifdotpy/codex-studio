use super::*;
use crate::submit_gates::{
    AgentAuthorityRow, AuthorizationRecords, AuthorizationSnapshot, ProjectAuthorityRow,
};
use std::collections::BTreeMap;
use studio_runtime_schema::delivery::DeliveryMoment;
use studio_runtime_schema::persistence::{DeliveryStatus, RecipientDelivery};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatSendReply {
    id: String,
    room: String,
    deliveries: BTreeMap<String, RecipientDelivery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refused: Option<BTreeMap<String, String>>,
}

fn saved_private_message(
    tx: &Transaction<'_>,
    id: &str,
    who: &str,
    room: &str,
    signature: &str,
    now_ms: i64,
) -> Result<Option<ChatSendReply>, CallbackError> {
    use studio_runtime_schema::persistence::{RequestIdentity, RequestReferenceKind};
    use studio_runtime_store::request_results::{RequestResultError, RequestResults};

    let raw = RequestResults::replay_reference(tx, id, signature, now_ms)?;
    if raw.is_none()
        && let Some(saved) =
            event_write::replay_request::<ChatSendReply>(tx, id, signature, now_ms)?
    {
        return Ok(Some(saved.reply));
    }
    let reference = match raw {
        Some(reference) => Some(reference),
        None => {
            let identity = RequestIdentity::new("agent_message", &[who], id, &[])
                .map_err(RequestResultError::from)?;
            RequestResults::replay_reference(tx, identity.id(), signature, now_ms)?
        }
    };
    let Some(reference) = reference else {
        return Ok(None);
    };
    if reference.kind != RequestReferenceKind::Message || reference.id != id {
        return Err(CallbackError::Stored("request_results/message".into()));
    }
    let (stored_room, sender, deliveries): (String, String, String) = tx.query_row(
        "SELECT room,sender,deliveries FROM runtime_chat_messages WHERE id=?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if stored_room != room || sender != who {
        return Err(CallbackError::Stored(
            "runtime_chat_messages/identity".into(),
        ));
    }
    let deliveries = serde_json::from_str(&deliveries)
        .map_err(|_| CallbackError::Stored("runtime_chat_messages/deliveries".into()))?;
    Ok(Some(ChatSendReply {
        id: id.to_owned(),
        room: stored_room,
        deliveries,
        refused: None,
    }))
}
#[derive(serde::Serialize, serde::Deserialize)]
enum UserRecipient {
    #[serde(rename = "user")]
    User,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UserSendReply {
    id: String,
    room: String,
    recipient: UserRecipient,
}

use studio_runtime_store::{AgentId, AgentRepository, SyncDocumentsRepository};

fn message_agents(
    tx: &Transaction<'_>,
    actor: &Value,
    target: &str,
    resolve_prefix: bool,
) -> Result<Vec<AgentAuthorityRow>, CallbackError> {
    let root = text(actor, "rootId")?;
    let rows = studio_runtime_store::AgentIndexedReads::message_records(
        tx,
        [
            Some(text(actor, "id")?),
            Some(root),
            actor["parentId"].as_str(),
            Some(target),
        ],
        (target == "broadcast").then_some(root),
        (resolve_prefix && target.chars().count() >= 8).then_some(target),
    )?;
    agent_rows(rows)
}

pub(super) fn agent_rows(
    rows: Vec<(String, String)>,
) -> Result<Vec<AgentAuthorityRow>, CallbackError> {
    rows.into_iter()
        .map(|(id, raw)| {
            let value: AgentAuthorityRow =
                serde_json::from_str(&raw).map_err(|_| records::invalid("runtime_agents", &id))?;
            if value.id.as_deref() != Some(id.as_str()) {
                return Err(records::invalid("runtime_agents", &id).into());
            }
            Ok(value)
        })
        .collect()
}

fn message_projects(
    tx: &Transaction<'_>,
    agents: &[AgentAuthorityRow],
    sender: &str,
) -> Result<Vec<ProjectAuthorityRow>, CallbackError> {
    let paths = agents
        .iter()
        .filter(|agent| agent.id.as_deref() == Some(sender))
        .filter_map(|agent| agent.cwd.as_deref())
        .filter_map(studio_runtime_store::path_identity::lexical_text)
        .collect::<std::collections::BTreeSet<_>>();
    let mut statement = tx.prepare_cached("SELECT id,CASE WHEN json_valid(record) THEN CASE WHEN json_type(record,'$.path')='text' THEN json_extract(record,'$.path') END END,record FROM runtime_projects ORDER BY rowid")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut projects = Vec::new();
    for row in rows {
        let (id, path, raw) = row?;
        if !studio_runtime_store::path_identity::lexical_text(path.as_deref().unwrap_or(&id))
            .is_some_and(|path| paths.contains(&path))
        {
            continue;
        }
        let project: ProjectAuthorityRow =
            serde_json::from_str(&raw).map_err(|_| records::invalid("runtime_projects", &id))?;
        if project.id() != Some(id.as_str()) {
            return Err(records::invalid("runtime_projects", &id).into());
        }
        projects.push(project);
    }
    Ok(projects)
}

pub(super) fn message_authority(
    agents: &[AgentAuthorityRow],
    projects: &[ProjectAuthorityRow],
    mut rows: AuthorizationRecords,
) -> Result<AuthorizationSnapshot, CallbackError> {
    for path in agents
        .iter()
        .filter_map(|agent| agent.cwd.as_deref())
        .chain(projects.iter().filter_map(|project| match project {
            ProjectAuthorityRow::Active { path, .. } => Some(path.as_str()),
            ProjectAuthorityRow::Removed { .. } => None,
        }))
    {
        if let Some(resolved) = studio_runtime_store::path_identity::lexical_text(path) {
            rows.resolved_paths
                .insert(path.to_owned(), resolved.to_string_lossy().into_owned());
        }
    }
    rows.agents = agents
        .iter()
        .map(|agent| {
            let id = agent
                .id
                .clone()
                .ok_or_else(|| CallbackError::Stored("runtime_agents/id".into()))?;
            Ok((id, Ok(agent.clone())))
        })
        .collect::<Result<_, CallbackError>>()?;
    let projects = projects
        .iter()
        .filter_map(|project| match project {
            ProjectAuthorityRow::Active { path, .. } => Some((path.clone(), Ok(project.clone()))),
            ProjectAuthorityRow::Removed { .. } => None,
        })
        .collect::<Vec<_>>();
    rows.projects = projects_by_resolved_path(projects, &rows.resolved_paths);
    AuthorizationSnapshot::from_typed(&rows).map_err(|error| CallbackError::Stored(error.location))
}

fn authorize(
    authority: &mut AuthorizationSnapshot,
    recipient: &str,
    sender: &str,
    room_id: &str,
    root: Option<&str>,
    members: &[String],
) -> Result<(), CallbackError> {
    authority
        .for_message(recipient, sender)
        .map_err(|error| CallbackError::Stored(error.location))?;
    authority
        .authorize_message(sender, room_id, root, members)
        .map_err(|_| {
            CallbackError::Invalid(
                "Communication is limited to your team or user-grouped peer chat",
            )
        })
}

fn resolve_message_target(
    supplied: &str,
    actor: &Value,
    who: &str,
    agents: &[AgentAuthorityRow],
    authority: &mut AuthorizationSnapshot,
) -> Result<String, CallbackError> {
    let mut visible = Vec::new();
    for agent in agents {
        let Some(id) = agent.id.as_deref().filter(|id| id.starts_with(supplied)) else {
            continue;
        };
        if agent_visible(actor, who, agent, authority)? {
            if id == supplied {
                return Ok(id.to_owned());
            }
            visible.push(id.to_owned());
        }
    }
    resolve_prefix(visible, supplied)
}

/// The gate keys a project by its resolved path. When two records name one
/// directory, the record whose stored path is already that form wins, so a
/// second spelling never replaces the authority of an exact record.
fn projects_by_resolved_path<T>(
    projects: impl IntoIterator<Item = (String, T)>,
    paths: &BTreeMap<String, String>,
) -> BTreeMap<String, T> {
    let mut exact = BTreeMap::new();
    let mut respelled = BTreeMap::new();
    for (path, project) in projects {
        match paths.get(&path) {
            Some(resolved) if resolved != &path => {
                respelled.entry(resolved.clone()).or_insert(project);
            }
            _ => {
                exact.insert(path, project);
            }
        }
    }
    for (key, project) in respelled {
        exact.entry(key).or_insert(project);
    }
    exact
}
pub(super) fn authorize_room(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    id: &str,
) -> Result<(), CallbackError> {
    let directory = crate::sync_entity_context::RoomDirectory::load(tx)?;
    let mut authority = message_authority(
        directory.agents(),
        directory.projects(),
        AuthorizationRecords::default(),
    )?;
    authorize_room_with(tx, snapshot, id, &mut authority, &directory)
}
fn authorize_room_with(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    id: &str,
    authority: &mut AuthorizationSnapshot,
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
    let root = match room["kind"].as_str() {
        Some("broadcast") => Some(room["rootId"].as_str().unwrap_or_default()),
        Some("private") => None,
        _ => {
            return Err(CallbackError::Invalid(
                "Communication is limited to your team or user-grouped peer chat",
            ));
        }
    };
    let members = room["members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    authorize(
        authority,
        &snapshot.context().agent,
        sender,
        text(&room, "id")?,
        root,
        &members,
    )
}
pub(super) fn send(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    args: &Value,
    id: &str,
    now: f64,
) -> Result<Box<serde_json::value::RawValue>, CallbackError> {
    send_as_actor(
        tx,
        snapshot.actor(),
        &snapshot.context().agent,
        args,
        id,
        now,
        true,
    )
}

pub(crate) fn send_remote_worker(
    tx: &Transaction<'_>,
    sender: &str,
    args: &Value,
    id: &str,
    now: f64,
) -> Result<Box<serde_json::value::RawValue>, CallbackError> {
    let actor = records::get(tx, "runtime_agents", sender)?
        .ok_or(CallbackError::Invalid("The remote worker is unavailable"))?;
    if actor["id"] != sender
        || actor.get("autoWake") != Some(&Value::Bool(true))
        || studio_runtime_store::truthy(actor.get("deletedAt"))
    {
        return Err(CallbackError::Invalid("The worker was stopped"));
    }
    send_as_actor(tx, &actor, sender, args, id, now, false)
}

fn send_as_actor(
    tx: &Transaction<'_>,
    actor: &Value,
    who: &str,
    args: &Value,
    id: &str,
    now: f64,
    resolve_direct: bool,
) -> Result<Box<serde_json::value::RawValue>, CallbackError> {
    let mut resolved_args = args.clone();
    let prepared = if let Some(supplied) = args["target"].as_str().filter(|target| {
        resolve_direct
            && !matches!(*target, "user" | "parent" | "lead" | "broadcast" | "all")
            && !target.starts_with("remote:")
            && !target.starts_with("federated:")
    }) {
        let exact_team = studio_runtime_store::AgentIndexedReads::message_exact_team(
            tx,
            supplied,
            text(actor, "rootId")?,
        )?;
        let agents = message_agents(tx, actor, supplied, !exact_team)?;
        let authority = if exact_team {
            None
        } else {
            let projects = message_projects(tx, &agents, who)?;
            let mut authority =
                message_authority(&agents, &projects, AuthorizationRecords::default())?;
            let target = resolve_message_target(supplied, actor, who, &agents, &mut authority)?;
            resolved_args["target"] = json!(target);
            Some((projects, authority))
        };
        Some((agents, authority))
    } else {
        None
    };
    let args = &resolved_args;
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
    let (agents, prepared_authority) = match prepared {
        Some(prepared) => prepared,
        None => (message_agents(tx, actor, target, false)?, None),
    };
    let (projects, mut authority) = match prepared_authority {
        Some(prepared) => prepared,
        None => {
            let projects = message_projects(tx, &agents, who)?;
            let authority = message_authority(&agents, &projects, AuthorizationRecords::default())?;
            (projects, authority)
        }
    };
    let recipients = if target == "broadcast" {
        agents
            .iter()
            .filter(|a| {
                a.root_id.as_deref() == actor["rootId"].as_str()
                    && a.id.as_deref() != Some(who)
                    && !a
                        .deleted_at
                        .as_ref()
                        .is_some_and(|deleted| deleted.is_deleted())
            })
            .cloned()
            .collect::<Vec<_>>()
    } else {
        vec![
            agents
                .iter()
                .find(|a| {
                    a.id.as_deref() == Some(target)
                        && !a
                            .deleted_at
                            .as_ref()
                            .is_some_and(|deleted| deleted.is_deleted())
                })
                .cloned()
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
    let directory = crate::sync_entity_context::RoomDirectory::resolve(agents, projects)?;
    let broadcast = target == "broadcast";
    let mut refused = BTreeMap::new();
    let mut first_refusal = None;
    let mut admitted = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        let recipient_id = recipient
            .id
            .as_deref()
            .ok_or_else(|| CallbackError::Stored("runtime_agents/id".into()))?;
        let members = if broadcast {
            Vec::new()
        } else {
            vec![who.to_owned(), target.to_owned()]
        };
        match authorize(
            &mut authority,
            recipient_id,
            who,
            text(&room, "id")?,
            broadcast.then(|| text(actor, "rootId")).transpose()?,
            &members,
        ) {
            Ok(()) => {}
            // One member record that the gate cannot check must not refuse
            // the broadcast to the other members. The result names it.
            Err(CallbackError::Stored(location)) if broadcast => {
                crate::diagnostics::log_stderr(format_args!(
                    "Broadcast {id} skips member {recipient_id}: invalid stored authority at {location}"
                ));
                refused.insert(
                    recipient_id.into(),
                    crate::managed_host_tools::Error::StoredAt(location.clone()).to_string(),
                );
                first_refusal.get_or_insert(location);
                continue;
            }
            Err(error) => return Err(error),
        }
        if recipient.id != recipient.root_id {
            let root = event_write::checked_agent(
                tx,
                recipient
                    .root_id
                    .as_deref()
                    .ok_or_else(|| CallbackError::Stored("runtime_agents/rootId".into()))?,
            )?;
            event_write::assert_delegation(&root)?;
        }
        // Python `codex_radio.guard_message`: only the sender's live shared
        // reply turn blocks a private message to a fellow room member.
        crate::radio_runtime::guard_message(tx, who, recipient_id).map_err(radio_guard_error)?;
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
    let signature = event_write::request_signature(args);
    let now_ms = chrono::Utc::now().timestamp_millis();
    if !broadcast
        && let Some(reply) = saved_private_message(tx, id, who, &room_id, &signature, now_ms)?
    {
        return serde_json::value::to_raw_value(&reply).map_err(Into::into);
    }
    let admitted = match event_write::begin_request::<ChatSendReply>(
        tx,
        event_write::RequestKey::new("agent_message", &[who], id),
        &signature,
        now_ms,
    )? {
        event_write::CallerAdmission::Replay(reply) => {
            return serde_json::value::to_raw_value(&reply).map_err(Into::into);
        }
        event_write::CallerAdmission::Fresh(admitted) => admitted,
    };
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
    if let Some(view) =
        crate::sync_views::room_entity_view(&room, &directory.for_room(tx, &room, true)?)
    {
        SyncDocumentsRepository::put_entity(tx, "room", &room_id, view, false)?;
    }
    let mut deliveries: BTreeMap<String, RecipientDelivery> = BTreeMap::new();
    for recipient in recipients {
        let recipient_id = recipient
            .id
            .as_deref()
            .ok_or_else(|| CallbackError::Stored("runtime_agents/id".into()))?;
        let recipient =
            records::get(tx, "runtime_agents", recipient_id)?.ok_or(CallbackError::Stale)?;
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
            RecipientDelivery {
                status: if active {
                    DeliveryStatus::Queued
                } else {
                    DeliveryStatus::StoredOnly
                },
                error: None,
            },
        );
    }
    tx.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?1,?2,?3,?4,?5,?6)",params![id,room_id,who,message,now,json!(deliveries).to_string()])?;
    let view = crate::sync_views::room_entity_view(&room, &directory.for_room(tx, &room, true)?)
        .ok_or_else(|| CallbackError::Stored(room_id.clone()))?;
    SyncDocumentsRepository::put_entity(tx, "room", &room_id, view, false)?;
    let result = ChatSendReply {
        id: id.to_owned(),
        room: room_id,
        deliveries,
        refused: (!refused.is_empty()).then_some(refused),
    };
    // A broadcast member that was not checked is named with its refusal.
    // The receipt keeps it, so a replay of this request reports it again.
    if broadcast {
        event_write::settle_request(tx, &admitted, Some(who), &result)?;
    } else {
        let revision: i64 = tx.query_row(
            "SELECT rev FROM runtime_chat_messages WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        studio_runtime_store::request_results::RequestResults::settle_reference(
            tx,
            &admitted,
            &studio_runtime_store::request_results::RequestReference {
                kind: studio_runtime_schema::persistence::RequestReferenceKind::Message,
                id: id.to_owned(),
                revision,
            },
        )?;
    }
    serde_json::value::to_raw_value(&result).map_err(Into::into)
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
) -> Result<Box<serde_json::value::RawValue>, CallbackError> {
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
    let admitted = match event_write::begin_request::<UserSendReply>(
        tx,
        event_write::RequestKey::new("user_message", &[who], key),
        &signature,
        chrono::Utc::now().timestamp_millis(),
    )? {
        event_write::CallerAdmission::Replay(reply) => {
            return serde_json::value::to_raw_value(&reply).map_err(Into::into);
        }
        event_write::CallerAdmission::Fresh(admitted) => admitted,
    };
    let message = crate::notification_dispatch::submit_user_message(tx, actor, key, text, now)
        .map_err(|error| match error {
            crate::notification_dispatch::NotificationDispatchError::Store(error) => {
                CallbackError::Store(error)
            }
            crate::notification_dispatch::NotificationDispatchError::RequestResult(error) => {
                error.into()
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
            crate::notification_dispatch::NotificationDispatchError::InvalidNotification {
                ..
            }
            | crate::notification_dispatch::NotificationDispatchError::InvalidAccountState {
                ..
            }
            | crate::notification_dispatch::NotificationDispatchError::StreamCapacity
            | crate::notification_dispatch::NotificationDispatchError::ConflictingDelivery
            | crate::notification_dispatch::NotificationDispatchError::UnauthorizedSubmission
            | crate::notification_dispatch::NotificationDispatchError::Move(_)
            | crate::notification_dispatch::NotificationDispatchError::Json(_) => {
                CallbackError::Invalid("A complaint needs an existing lead")
            }
        })?;
    let result = UserSendReply {
        id: super::text(&message, "id")?.to_owned(),
        room: format!("user:{who}"),
        recipient: UserRecipient::User,
    };
    event_write::settle_request(tx, &admitted, Some(who), &result)?;
    serde_json::value::to_raw_value(&result).map_err(Into::into)
}

fn radio_guard_error(error: crate::projects::ProjectsError) -> CallbackError {
    use crate::projects::ProjectsError as E;
    match error {
        E::Invalid(message) => CallbackError::Invalid(message),
        E::Store(error) => CallbackError::Store(error),
        E::Sqlite(error) => CallbackError::Store(error.into()),
        E::InvalidStored { table, id } => CallbackError::Stored(format!("{table}/{id}")),
        E::Conflict(_) | E::BadRequest(_) | E::Requires(_) | E::PreparationWorker => {
            CallbackError::Stored("runtime_rooms radio".into())
        }
    }
}

fn map_federation_error(
    error: crate::federation_messaging::FederationMessageError,
) -> CallbackError {
    use crate::federation_messaging::FederationMessageError as FederationError;
    match error {
        FederationError::Store(error) => CallbackError::Store(error),
        FederationError::RequestResult(error) => error.into(),
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
    let directory = crate::sync_entity_context::RoomDirectory::load(tx)?;
    let mut authority = message_authority(
        directory.agents(),
        directory.projects(),
        AuthorizationRecords::default(),
    )?;
    let mut agents = Vec::new();
    for agent in directory.agents() {
        if agent_visible(actor, &snapshot.context().agent, agent, &mut authority)? {
            let id = agent
                .id
                .as_deref()
                .ok_or_else(|| CallbackError::Stored("runtime_agents/id".into()))?;
            let summary = records::get(tx, "runtime_agents", id)?.ok_or(CallbackError::Stale)?;
            agents.push(Value::Object(
                ["id", "name", "role", "rootId", "parentId", "status"]
                    .into_iter()
                    .map(|key| (key.into(), summary[key].clone()))
                    .collect(),
            ));
        }
    }
    agents.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let mut rooms = Vec::new();
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

/// One visibility policy for discovery and scoped target resolution. Invalid
/// foreign authority hides that candidate without refusing the caller's request.
fn agent_visible(
    actor: &Value,
    who: &str,
    agent: &AgentAuthorityRow,
    authority: &mut AuthorizationSnapshot,
) -> Result<bool, CallbackError> {
    if agent
        .deleted_at
        .as_ref()
        .is_some_and(|deleted| deleted.is_deleted())
    {
        return Ok(false);
    }
    if agent.root_id.as_deref() == actor["rootId"].as_str() {
        return Ok(true);
    }
    let id = agent
        .id
        .as_deref()
        .ok_or_else(|| CallbackError::Stored("runtime_agents/id".into()))?;
    match authorize(
        authority,
        id,
        who,
        "peer-check",
        None,
        &[who.to_owned(), id.to_owned()],
    ) {
        Ok(()) => Ok(true),
        Err(CallbackError::Invalid(_)) => Ok(false),
        Err(CallbackError::Stored(location)) => {
            crate::diagnostics::log_stderr(format_args!(
                "Agent {id} is not visible to {who}: invalid stored authority at {location}"
            ));
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn resolve_target(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    supplied: &str,
) -> Result<String, CallbackError> {
    let actor = snapshot.actor();
    if studio_runtime_store::AgentIndexedReads::message_exact_team(
        tx,
        supplied,
        text(actor, "rootId")?,
    )? {
        return Ok(supplied.to_owned());
    }
    let agents = message_agents(tx, actor, supplied, true)?;
    let projects = message_projects(tx, &agents, &snapshot.context().agent)?;
    let mut authority = message_authority(&agents, &projects, AuthorizationRecords::default())?;
    resolve_message_target(
        supplied,
        actor,
        &snapshot.context().agent,
        &agents,
        &mut authority,
    )
}

pub(super) fn resolve_prefix(
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
    use super::{CallbackError, resolve_prefix};

    #[test]
    fn project_removal_rows_do_not_block_message_authority() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .unwrap();
        for id in ["/fixture", "/removed-b", "/removed-c", "/removed-d"] {
            let row = serde_json::json!({"id":id,"removal":{"deletedAt":1,"reason":"removed","requestId":"request","sequence":1}});
            db.execute(
                "INSERT INTO runtime_projects VALUES(?1,?2)",
                rusqlite::params![id, row.to_string()],
            )
            .unwrap();
        }
        let agent =
            serde_json::from_str(r#"{"id":"sender","rootId":"sender","cwd":"/fixture"}"#).unwrap();
        let tx = db.transaction().unwrap();
        let projects = super::message_projects(&tx, &[agent], "sender").unwrap();
        assert_eq!(projects.len(), 1);
        assert!(super::message_authority(&[], &projects, Default::default()).is_ok());
    }

    #[test]
    fn message_receipts_round_trip_existing_wire_shapes() {
        for raw in [
            r#"{"id":"m","room":"private:a:b","deliveries":{"b":{"status":"queued","error":null}}}"#,
            r#"{"id":"m","room":"broadcast:a","deliveries":{},"refused":{"b":"invalid"}}"#,
        ] {
            let reply: super::ChatSendReply = serde_json::from_str(raw).unwrap();
            let encoded = serde_json::to_string(&reply).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&encoded).unwrap(),
                serde_json::from_str::<serde_json::Value>(raw).unwrap()
            );
        }
        let raw = r#"{"id":"m","room":"user:a","recipient":"user"}"#;
        let reply: super::UserSendReply = serde_json::from_str(raw).unwrap();
        let encoded = serde_json::to_string(&reply).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }

    #[test]
    fn visible_agent_prefix_requires_eight_characters_and_unique_match() {
        let ids = || {
            ["12345678-alpha", "87654321-beta"]
                .into_iter()
                .map(str::to_owned)
        };
        assert_eq!(
            resolve_prefix(ids(), "12345678").expect("unique eight-character prefix resolves"),
            "12345678-alpha"
        );
        assert!(matches!(
            resolve_prefix(ids(), "1234567"),
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
            resolve_prefix(ids, "12345678"),
            Err(CallbackError::Invalid(
                "Ambiguous agent ID; use a full ID from orchestration_peers"
            ))
        ));
    }
}

#[cfg(test)]
mod receipt_tests {
    #[test]
    fn private_message_reference_replays_current_deliveries_and_rejects_conflict() {
        use studio_runtime_schema::persistence::{
            DeliveryStatus, RequestIdentity, RequestReferenceKind,
        };
        use studio_runtime_store::request_results::{Admission, RequestReference, RequestResults};
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE request_results(id TEXT PRIMARY KEY,signature TEXT NOT NULL,status TEXT,ref_kind TEXT,ref_id TEXT,ref_rev INTEGER,result TEXT,created INTEGER NOT NULL); CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE runtime_chat_messages(id TEXT PRIMARY KEY,room TEXT,sender TEXT,deliveries TEXT,rev INTEGER); ").unwrap();
        let id = RequestIdentity::issue_root();
        let identity = RequestIdentity::new("agent_message", &["sender"], &id, &[]).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let tx = db.transaction().unwrap();
        tx.execute(
            "INSERT INTO runtime_chat_messages VALUES(?1,'room','sender',?2,1)",
            rusqlite::params![id, r#"{"recipient":{"status":"queued","error":null}}"#],
        )
        .unwrap();
        let Admission::Fresh(admitted) = RequestResults::admit(&tx, &identity, "sig", now).unwrap()
        else {
            panic!("fresh")
        };
        RequestResults::settle_reference(
            &tx,
            &admitted,
            &RequestReference {
                kind: RequestReferenceKind::Message,
                id: id.clone(),
                revision: 1,
            },
        )
        .unwrap();
        tx.execute(
            "UPDATE runtime_chat_messages SET deliveries=?1,rev=2 WHERE id=?2",
            rusqlite::params![r#"{"recipient":{"status":"delivered","error":null}}"#, id],
        )
        .unwrap();
        let reply = super::saved_private_message(&tx, &id, "sender", "room", "sig", now)
            .unwrap()
            .unwrap();
        assert_eq!(
            reply.deliveries.get("recipient").unwrap().status,
            DeliveryStatus::Delivered
        );
        assert!(
            super::saved_private_message(&tx, &id, "sender", "room", "different", now).is_err()
        );
        let reference = RequestResults::replay_reference(&tx, identity.id(), "sig", now)
            .unwrap()
            .unwrap();
        assert_eq!(reference.revision, 1);
    }
}
