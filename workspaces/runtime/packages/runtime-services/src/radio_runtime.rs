//! Durable local radio commands and completion. The caller owns each transaction.
//!
//! Requests are user-authorized at HTTP. `radio:<request_id>` binds the complete
//! body; an exact replay returns the committed result, changed content fails.
//! Event/floor, binding, answer and sync rows commit together. Provider effects
//! run only after commit; an interrupt marked requested is never sent again.
use crate::event_write::{self, python_json, truthy};
use crate::host_project_runtime::PreparedHost;
use crate::projects::operations::{live_teams, project_agents, replay, save_operation_receipt};
use crate::projects::{ProjectsError, project_key};
use crate::radio::{
    RadioAuthorizationRecords, RadioAuthorizationSnapshot, RadioDenial, RadioTurnDecision,
};
use crate::sync_views::{RoomViewContext, room_entity_view};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, ProjectsRepository, QueueEvent, QueueEventId,
    QueueEventRepository, RuntimeStore, SyncDocumentsRepository, system_reads as records,
};
use uuid::Uuid;

const MAX_INPUT: usize = 200_000;
const MAX_TEXT: usize = 32_000;

#[path = "projects/radio_state.rs"]
mod state;
use state::*;

#[path = "projects/radio_lifecycle.rs"]
mod lifecycle;
pub use lifecycle::{
    NativeAcceptanceReceipt, ObservedItem, bind_reserved, claim_interrupt, guard_message,
    holds_floor, observe_item, route_question_answer, select_pending, tick,
};

pub(crate) fn publish_room(
    db: &Transaction<'_>,
    value: &Value,
    prepared: &PreparedHost,
) -> Result<(), ProjectsError> {
    state::publish_room(db, value, prepared)
}

/// HTTP may expose only operations with a composed consumer. P01 replaces this
/// call with manage once its retained scheduler drives tick and Interrupt effects.
pub fn manage_http(store: &RuntimeStore, body: &Value) -> Result<Value, ProjectsError> {
    if matches!(body["radio_action"].as_str(), Some("send" | "pass")) {
        return Err(ProjectsError::Requires("workspace-state/peer-radio"));
    }
    manage_command(store, body, false)
}

/// Local open/send/pass/stop; direct creation is injected separately by P02.
pub fn manage(store: &RuntimeStore, body: &Value) -> Result<Value, ProjectsError> {
    manage_command(store, body, true)
}
fn manage_command(store: &RuntimeStore, body: &Value, live: bool) -> Result<Value, ProjectsError> {
    let action = body["radio_action"].as_str().unwrap_or_default();
    if action == "create" {
        return Err(ProjectsError::Requires("host-project/radio-create"));
    }
    if !matches!(action, "open" | "send" | "pass" | "stop") {
        return Err(ProjectsError::Invalid("Unknown shared chat action"));
    }
    let key = request_key(body)?;
    let signature = event_write::request_signature(&json!({"operation":"radio","body":body}));
    if let Some(result) = store.transaction_with(|db| replay(db, &key, &signature, "radio"))? {
        return Ok(result);
    }
    let path = project_key(body.get("path"), false)?;
    let team = Uuid::parse_str(body["team_id"].as_str().unwrap_or_default())
        .map_err(|_| ProjectsError::Invalid("peer team ID is invalid"))?
        .to_string();
    let prepared = PreparedHost::load(store)?;
    prepared.recheck_paths()?;
    store.transaction_with(|db| {
        if let Some(prior) = replay(db,&key,&signature,"radio")? { return Ok(prior); }
        prepared.recheck(db)?;
        let id = format!("radio:{team}");
        let now = crate::projects::now_seconds();
        let mut room = if let Some(value) = records::get(db,"runtime_rooms",&id)? { Room::checked(value)? } else {
            if action != "open" { return Err(ProjectsError::Invalid("Open the shared chat first")); }
            let project = ProjectsRepository::get(db,&path)?.ok_or(ProjectsError::Invalid("Select a team with exactly two chats in this project"))?.1;
            let all_agents = project_agents(db)?;
            let teams = live_teams(project["peerTeams"].as_array().map(Vec::as_slice).unwrap_or_default(),&all_agents,&path,&prepared)?;
            let team_row = teams.iter().find(|t| t["id"] == team && t["members"].as_array().is_some_and(|m|m.len()==2)).ok_or(ProjectsError::Invalid("Select a team with exactly two chats in this project"))?;
            Room::checked(json!({"id":id,"kind":"private","members":team_row["members"],"projectPath":path,"customName":format!("{} · Shared chat",team_row["name"].as_str().unwrap_or_default()),"created":now,"userHidden":false,"radio":{"teamId":team,"revision":0,"status":"idle","speaker":null,"next":[],"active":null,"error":null,"seen":{}}}))?
        };
        if !live && action=="stop" && room.radio.active.is_some() {return Err(ProjectsError::Requires("workspace-state/peer-radio"));}
        if room.value["projectPath"] != path { return Err(ProjectsError::Invalid("The shared chat belongs to another project")); }
        if let Some(denial) = validate(db,&room,&prepared,matches!(action,"send"|"pass"))? && action != "stop" { return Err(ProjectsError::Invalid(denial)); }
        if action != "open" {
            if body["expected_revision"].as_u64() != Some(room.radio.revision) { return Err(ProjectsError::Invalid("The shared chat changed. Reload before sending")); }
            room.radio.revision = room.radio.revision.checked_add(1).ok_or_else(||invalid(room.id()))?;
            if matches!(action,"send"|"pass") {
                let members = room.members();
                let target = body["target"].as_str().unwrap_or("both");
                if target != "both" && !members.iter().any(|id|id==target) { return Err(ProjectsError::Invalid("Select a participant in this shared chat")); }
                let rounds = body.get("rounds").map_or(Some(1),Value::as_u64).filter(|n|[1,2].contains(n)).ok_or(ProjectsError::Invalid("Select one or two rounds"))?;
                let text = if action == "send" { bounded_text(&body["text"],MAX_TEXT)? } else {
                    if target == "both" { return Err(ProjectsError::Invalid("Select the next speaker")); }
                    let a = records::get(db,"runtime_agents",target)?.ok_or(ProjectsError::Invalid("participant missing"))?;
                    format!("Please give the next reply: {}",a["name"].as_str().unwrap_or_default())
                };
                message(db,&room,&key,"user",&text,now)?;
                room.radio.next = if target == "both" { (0..rounds).flat_map(|_|members.clone()).collect() } else { vec![target.to_owned()] };
                if room.radio.active.is_none() { room.radio.status=Status::Waiting; room.radio.error=None; }
                else if !matches!(room.radio.status,Status::Blocked|Status::Stopping) { room.radio.error=None; }
            } else {
                room.radio.next.clear();
                if let Some(active) = &room.radio.active && !cancel_pending(db,active)? { room.radio.status=Status::Stopping; room.radio.error=None; }
                else { room.radio.active=None; room.radio.speaker=None; room.radio.status=Status::Idle; room.radio.error=None; }
            }
        }
        save(db,&mut room,&prepared,now)?;
        let result = json!({"room":room.value});
        save_operation_receipt(db,&key,&signature,&result)?;
        Ok(result)
    })
}
fn request_key(body: &Value) -> Result<String, ProjectsError> {
    Ok(format!("radio:{}", bounded_text(&body["request_id"], 255)?))
}
fn bounded_text(value: &Value, bound: usize) -> Result<String, ProjectsError> {
    value
        .as_str()
        .map(event_write::python_strip)
        .filter(|v| !v.is_empty() && v.chars().count() <= bound)
        .map(str::to_owned)
        .ok_or(ProjectsError::Invalid("text is empty or exceeds its limit"))
}

/// The admission owner supplies a port holding preparation loaded before BEGIN.
/// It must enqueue exactly this ID in `db`, or fail without an external effect.
pub trait RadioIngress {
    fn enqueue(
        &mut self,
        db: &Transaction<'_>,
        agent: &str,
        id: &str,
        text: &str,
        now: f64,
    ) -> Result<(), ProjectsError>;
}
/// A committed interrupt intent. Hand it to the retained worker only after commit.
/// Losing the hand-off receipt never clears interruptRequested or reissues it.
pub struct Interrupt {
    pub request_id: String,
    pub agent: Value,
    pub turn_id: String,
}

/// P02 owns draft creation, provider defaults and account-identity revalidation.
/// Preparation may read files. Recheck must only read store/in-memory account
/// snapshots, and may not perform any filesystem or provider work in `db`.
pub trait RadioCreation {
    fn prepare(
        &mut self,
        body: &Value,
        path: &std::path::Path,
        ids: [AgentId; 2],
    ) -> Result<[AgentRecord; 2], ProjectsError>;
    fn recheck(
        &mut self,
        db: &Transaction<'_>,
        drafts: &[AgentRecord; 2],
    ) -> Result<(), ProjectsError>;
}

pub fn create_with(
    store: &RuntimeStore,
    body: &Value,
    creation: &mut dyn RadioCreation,
) -> Result<Value, ProjectsError> {
    let key = request_key(body)?;
    let signature = event_write::request_signature(&json!({"operation":"radio","body":body}));
    if let Some(result) = store.transaction_with(|db| replay(db, &key, &signature, "radio"))? {
        return Ok(result);
    }
    let path = project_key(body.get("path"), true)?;
    let name = bounded_text(&body["name"], 80)?;
    let request = bounded_text(&body["request_id"], 255)?;
    let namespace = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("codex-studio:radio:{request}").as_bytes(),
    );
    let room_id = format!("radio:{namespace}");
    let ids = [
        Uuid::new_v5(&namespace, b"0").to_string(),
        Uuid::new_v5(&namespace, b"1").to_string(),
    ];
    let ids = ids.map(AgentId::from);
    let project_path = std::path::Path::new(&path);
    let mut drafts = creation.prepare(body, project_path, ids.clone())?;
    for (draft, id) in drafts.iter_mut().zip(&ids) {
        if draft.id != *id
            || draft.root_id != *id
            || draft.extra.get("cwd").and_then(Value::as_str) != Some(path.as_str())
            || draft.extra.get("isLead").and_then(Value::as_bool) != Some(true)
        {
            return Err(ProjectsError::Invalid(
                "prepared shared participant identity changed",
            ));
        }
        draft
            .extra
            .insert("sharedRoomId".to_owned(), json!(room_id));
        draft.extra.insert("needsTitle".to_owned(), json!(false));
        records::validate("runtime_agents", id.as_ref(), draft.value())?;
    }
    let prepared = PreparedHost::load(store)?;
    prepared.recheck_paths()?;
    store.transaction_with(|db| {
        if let Some(result)=replay(db,&key,&signature,"radio")? {return Ok(result);}
        prepared.recheck(db)?;
        creation.recheck(db,&drafts)?;
        if records::get(db,"runtime_rooms",&room_id)?.is_some() || ids.iter().map(|id|prepared_agent_exists(db,id.as_ref())).collect::<Result<Vec<_>,_>>()?.into_iter().any(|exists|exists) {return Err(ProjectsError::Invalid("shared identity already exists without its receipt"));}
        // Project creation is delegated to P02's commit recheck when necessary.
        if ProjectsRepository::get(db,&path)?.is_none() {return Err(ProjectsError::Requires("host-project/radio-create-project"));}
        for draft in &drafts {
            let id = draft.id.as_ref();
            let value = draft.value();
            AgentRepository::put(db, draft)?;
            let context=crate::sync_entity_context::agent_view_context(db,&value,prepared.server().ok())?;
            let view=crate::sync_views::agent_entity_view(&value,&context).ok_or(ProjectsError::Invalid("draft projection unavailable"))?;
            SyncDocumentsRepository::put_entity(db,"agent",id,view,false)?;
        }
        let now=crate::projects::now_seconds();
        let mut room=Room::checked(json!({"id":room_id,"kind":"private","members":ids,"projectPath":path,"customName":name,"created":now,"userHidden":false,"radio":{"direct":true,"teamId":namespace.to_string(),"revision":0,"status":"idle","speaker":null,"next":[],"active":null,"error":null,"seen":{}}}))?;
        save(db,&mut room,&prepared,now)?;
        let result=json!({"room":room.value});save_operation_receipt(db,&key,&signature,&result)?;Ok(result)
    })
}
fn prepared_agent_exists(db: &Connection, id: &str) -> Result<bool, ProjectsError> {
    Ok(records::get(db, "runtime_agents", id)?.is_some())
}
