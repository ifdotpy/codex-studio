//! Checked radio-turn admission decisions for the account worker.
//!
//! The caller loads a consistent view of radio rooms, their participant agents,
//! and the projects that own peer-team membership. This module has no store,
//! clock, filesystem, or provider dependency. A permitted first-thread binding
//! is returned as an intent for the caller to commit with its reservation.
//! Queue construction, pass/stop commands, floor holding, answer routing, and
//! completion/reconciliation remain in their stateful runtime owners; they need
//! event, question, transcript, and completed-turn records outside this input.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

const RADIO_ROOM_PREFIX: &str = "radio:";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RadioAuthorizationRecords {
    pub recipient_id: String,
    pub rooms: BTreeMap<String, String>,
    pub agents: BTreeMap<String, String>,
    pub projects: BTreeMap<String, String>,
    /// The caller's expanduser/resolve results, keyed by the stored path.
    pub resolved_paths: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("invalid stored radio input at {location}")]
pub struct RadioSnapshotError {
    pub location: String,
}

fn invalid(location: impl Into<String>) -> RadioSnapshotError {
    RadioSnapshotError {
        location: location.into(),
    }
}

fn parse_object(raw: &str, location: &str) -> Result<Value, RadioSnapshotError> {
    let value: Value = serde_json::from_str(raw).map_err(|_| invalid(location))?;
    if !value.is_object() {
        return Err(invalid(location));
    }
    Ok(value)
}

fn required_string(
    object: &Value,
    field: &str,
    location: &str,
) -> Result<String, RadioSnapshotError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid(format!("{location}/{field}")))
}

fn optional_string(
    object: &Value,
    field: &str,
    location: &str,
) -> Result<Option<String>, RadioSnapshotError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(invalid(format!("{location}/{field}"))),
    }
}

fn required_nullable_string(
    object: &Value,
    field: &str,
    location: &str,
) -> Result<Option<String>, RadioSnapshotError> {
    match object.get(field) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(invalid(format!("{location}/{field}"))),
    }
}

fn optional_bool(
    object: &Value,
    field: &str,
    location: &str,
    default: bool,
) -> Result<bool, RadioSnapshotError> {
    match object.get(field) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(invalid(format!("{location}/{field}"))),
    }
}

fn nullable_bool(
    object: &Value,
    field: &str,
    location: &str,
    default: bool,
) -> Result<bool, RadioSnapshotError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(invalid(format!("{location}/{field}"))),
    }
}

fn optional_u64(
    object: &Value,
    field: &str,
    location: &str,
) -> Result<Option<u64>, RadioSnapshotError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid(format!("{location}/{field}"))),
        _ => Err(invalid(format!("{location}/{field}"))),
    }
}

fn string_list(
    object: &Value,
    field: &str,
    location: &str,
) -> Result<Vec<String>, RadioSnapshotError> {
    let Some(Value::Array(values)) = object.get(field) else {
        return Err(invalid(format!("{location}/{field}")));
    };
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| invalid(format!("{location}/{field}/{index}")))
        })
        .collect()
}

fn resolved_path(
    stored: Option<String>,
    paths: &BTreeMap<String, String>,
    location: &str,
) -> Result<Option<String>, RadioSnapshotError> {
    let Some(stored) = stored.filter(|path| !path.trim().is_empty()) else {
        return Ok(None);
    };
    let resolved = paths
        .get(&stored)
        .filter(|path| {
            let path = std::path::Path::new(path);
            path.is_absolute()
                && path
                    .components()
                    .collect::<std::path::PathBuf>()
                    .as_os_str()
                    == path.as_os_str()
                && !path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
        })
        .ok_or_else(|| invalid(location))?;
    Ok(Some(resolved.clone()))
}

#[derive(Clone, Debug)]
struct Agent {
    id: String,
    root_id: Option<String>,
    parent_id: Option<String>,
    is_lead: bool,
    deleted: bool,
    cwd: Option<String>,
    shared_room_id: Option<String>,
    account_transfer: Option<String>,
    native_thread_blocked: bool,
    native_failure_held: bool,
    auto_wake: bool,
    thread_id: Option<String>,
    epoch: Option<u64>,
    compactions: Option<Option<u64>>,
    start_attempt: Option<StartAttempt>,
}

#[derive(Clone, Debug)]
struct StartAttempt {
    events: Vec<String>,
    epoch: Option<u64>,
    thread_id: Option<String>,
}

#[derive(Clone, Debug)]
struct Room {
    id: String,
    members: Vec<String>,
    project_path: String,
    direct: bool,
    team_id: String,
    status: RadioStatus,
    active: Option<ActiveTurn>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RadioStatus {
    Idle,
    Waiting,
    Speaking,
    Stopping,
    Blocked,
}

#[derive(Clone, Debug)]
struct ActiveTurn {
    event_id: String,
    agent_id: String,
    epoch: u64,
    thread_id: Option<String>,
}

#[derive(Clone, Debug)]
struct PeerTeam {
    id: String,
    members: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct Project {
    path: String,
    teams: Vec<PeerTeam>,
}

/// The validated, private-field view used for radio-turn admission.
#[derive(Clone, Debug)]
pub struct RadioAuthorizationSnapshot {
    recipient_id: String,
    rooms: Vec<Room>,
    agents: BTreeMap<String, Agent>,
    projects: Vec<Project>,
}

impl RadioAuthorizationSnapshot {
    /// Validate the stored records needed by `codex_radio.validate_event`.
    pub fn from_records(records: &RadioAuthorizationRecords) -> Result<Self, RadioSnapshotError> {
        if records.recipient_id.is_empty() {
            return Err(invalid("recipient_id"));
        }
        let mut agents = BTreeMap::new();
        for (key, raw) in &records.agents {
            let location = format!("agents/{key}");
            let row = parse_object(raw, &location)?;
            let id = required_string(&row, "id", &location)?;
            if id != *key {
                return Err(invalid(format!("{location}/id")));
            }
            let start_attempt = match row.get("startAttempt") {
                None | Some(Value::Null) => None,
                Some(Value::Object(attempt)) => {
                    let value = Value::Object(attempt.clone());
                    Some(StartAttempt {
                        events: string_list(&value, "events", &format!("{location}/startAttempt"))?,
                        epoch: optional_u64(&value, "epoch", &format!("{location}/startAttempt"))?,
                        thread_id: optional_string(
                            &value,
                            "threadId",
                            &format!("{location}/startAttempt"),
                        )?,
                    })
                }
                _ => return Err(invalid(format!("{location}/startAttempt"))),
            };
            let deleted_at = optional_string(&row, "deletedAt", &location)?;
            let native_thread_blocked = match row.get("nativeThreadBlock") {
                None | Some(Value::Null) => false,
                Some(Value::Object(value)) => !value.is_empty(),
                _ => return Err(invalid(format!("{location}/nativeThreadBlock"))),
            };
            let native_failure_held = match row.get("nativeFailureHold") {
                None | Some(Value::Null) | Some(Value::Bool(false)) => false,
                Some(Value::Bool(true)) => true,
                Some(Value::Object(value)) => !value.is_empty(),
                _ => return Err(invalid(format!("{location}/nativeFailureHold"))),
            };
            let cwd = resolved_path(
                optional_string(&row, "cwd", &location)?,
                &records.resolved_paths,
                &format!("{location}/cwd"),
            )?;
            agents.insert(
                key.clone(),
                Agent {
                    id,
                    root_id: optional_string(&row, "rootId", &location)?,
                    parent_id: optional_string(&row, "parentId", &location)?,
                    is_lead: optional_bool(&row, "isLead", &location, false)?,
                    deleted: deleted_at.is_some_and(|value| !value.is_empty()),
                    cwd,
                    shared_room_id: optional_string(&row, "sharedRoomId", &location)?,
                    account_transfer: optional_string(&row, "accountTransferId", &location)?,
                    native_thread_blocked,
                    native_failure_held,
                    auto_wake: optional_bool(&row, "autoWake", &location, false)?,
                    thread_id: optional_string(&row, "threadId", &location)?,
                    epoch: optional_u64(&row, "epoch", &location)?,
                    compactions: match row.get("compactions") {
                        None => None,
                        Some(Value::Null) => Some(None),
                        Some(Value::Number(value)) => Some(Some(
                            value
                                .as_u64()
                                .ok_or_else(|| invalid(format!("{location}/compactions")))?,
                        )),
                        _ => return Err(invalid(format!("{location}/compactions"))),
                    },
                    start_attempt,
                },
            );
        }

        let mut rooms = Vec::new();
        for (key, raw) in &records.rooms {
            if !key.starts_with(RADIO_ROOM_PREFIX) {
                return Err(invalid(format!("rooms/{key}/id")));
            }
            let location = format!("rooms/{key}");
            let row = parse_object(raw, &location)?;
            let id = required_string(&row, "id", &location)?;
            if id != *key || row.get("kind").and_then(Value::as_str) != Some("private") {
                return Err(invalid(format!("{location}/id")));
            }
            let members = string_list(&row, "members", &location)?;
            let project_path = required_string(&row, "projectPath", &location)?;
            let radio = row
                .get("radio")
                .filter(|value| value.is_object())
                .ok_or_else(|| invalid(format!("{location}/radio")))?;
            let direct = nullable_bool(radio, "direct", &format!("{location}/radio"), false)?;
            let team_id = required_string(radio, "teamId", &format!("{location}/radio"))?;
            let status = serde_json::from_value::<RadioStatus>(
                radio
                    .get("status")
                    .cloned()
                    .ok_or_else(|| invalid(format!("{location}/radio/status")))?,
            )
            .map_err(|_| invalid(format!("{location}/radio/status")))?;
            let active = match radio.get("active") {
                None | Some(Value::Null) => None,
                Some(value) if value.is_object() => Some(ActiveTurn {
                    event_id: required_string(
                        value,
                        "eventId",
                        &format!("{location}/radio/active"),
                    )?,
                    agent_id: required_string(
                        value,
                        "agentId",
                        &format!("{location}/radio/active"),
                    )?,
                    epoch: optional_u64(value, "epoch", &format!("{location}/radio/active"))?
                        .ok_or_else(|| invalid(format!("{location}/radio/active/epoch")))?,
                    thread_id: required_nullable_string(
                        value,
                        "threadId",
                        &format!("{location}/radio/active"),
                    )?,
                }),
                _ => return Err(invalid(format!("{location}/radio/active"))),
            };
            rooms.push(Room {
                id,
                members,
                project_path,
                direct,
                team_id,
                status,
                active,
            });
        }

        let mut projects = Vec::new();
        for (key, raw) in &records.projects {
            let location = format!("projects/{key}");
            let row = parse_object(raw, &location)?;
            let path = resolved_path(
                optional_string(&row, "path", &location)?,
                &records.resolved_paths,
                &format!("{location}/path"),
            )?
            .ok_or_else(|| invalid(format!("{location}/path")))?;
            if path != *key {
                continue;
            }
            let mut teams = Vec::new();
            match row.get("peerTeams") {
                None => {}
                Some(Value::Array(values)) => {
                    for (index, value) in values.iter().enumerate() {
                        let at = format!("{location}/peerTeams/{index}");
                        if !value.is_object() {
                            return Err(invalid(&at));
                        }
                        let id = required_string(value, "id", &at)?;
                        let _name = required_string(value, "name", &at)?;
                        let members = string_list(value, "members", &at)?;
                        let members = members
                            .into_iter()
                            .filter(|member| {
                                agents
                                    .get(member)
                                    .is_some_and(|agent| agent.is_lead_for(&path))
                            })
                            .collect::<BTreeSet<_>>();
                        if members.len() >= 2 {
                            teams.push(PeerTeam { id, members });
                        }
                    }
                }
                _ => return Err(invalid(format!("{location}/peerTeams"))),
            }
            projects.push(Project { path, teams });
        }

        Ok(Self {
            recipient_id: records.recipient_id.clone(),
            rooms,
            agents,
            projects,
        })
    }

    /// Decide whether this queued `radio_turn` is bound to a live, ready room.
    pub fn authorize_turn(&self, event_id: &str) -> RadioTurnDecision {
        let mut matching = self.rooms.iter().filter_map(|room| {
            room.active
                .as_ref()
                .filter(|active| active.event_id == event_id)
                .map(|active| (room, active))
        });
        let Some((room, active)) = matching.next() else {
            return RadioTurnDecision::Deny(RadioDenial::NoActiveRoomReceipt);
        };
        if matching.next().is_some() {
            return RadioTurnDecision::Deny(RadioDenial::AmbiguousRoomReceipt);
        }

        if let Some(denial) = self.room_denial(room, true) {
            return RadioTurnDecision::Deny(denial);
        }
        let Some(agent) = self.agents.get(&self.recipient_id) else {
            return RadioTurnDecision::Deny(RadioDenial::ParticipantUnavailable);
        };
        let binding = bind_thread(room, active, agent, event_id);
        let bound_thread = binding
            .as_ref()
            .map(|intent| Some(intent.thread_id.clone()))
            .unwrap_or_else(|| active.thread_id.clone());
        if matches!(room.status, RadioStatus::Blocked | RadioStatus::Stopping)
            || active.agent_id != self.recipient_id
            || agent.epoch != Some(active.epoch)
            || bound_thread != agent.thread_id
        {
            return RadioTurnDecision::Deny(RadioDenial::TurnNoLongerAuthorized);
        }
        RadioTurnDecision::Allow { binding }
    }

    /// Validate current participants without inventing an active turn receipt.
    pub fn validate_room(&self, id: &str, ready: bool) -> Option<RadioDenial> {
        self.rooms
            .iter()
            .find(|room| room.id == id)
            .map_or(Some(RadioDenial::NoActiveRoomReceipt), |room| {
                self.room_denial(room, ready)
            })
    }

    fn room_denial(&self, room: &Room, ready: bool) -> Option<RadioDenial> {
        if room.direct {
            if room.members.len() != 2
                || room.members.iter().collect::<BTreeSet<_>>().len() != 2
                || room.members.iter().any(|member| {
                    !self.agents.get(member).is_some_and(|agent| {
                        agent.is_lead_for(&room.project_path)
                            && agent.shared_room_id.as_deref() == Some(&room.id)
                    })
                })
            {
                return Some(RadioDenial::ParticipantsNoLongerAvailable);
            }
        } else {
            let members = room.members.iter().collect::<BTreeSet<_>>();
            let team = self
                .projects
                .iter()
                .filter(|project| project.path == room.project_path)
                .flat_map(|project| &project.teams)
                .find(|team| team.id == room.team_id);
            if !team.is_some_and(|team| {
                team.members.len() == 2 && team.members.iter().eq(members.iter().copied())
            }) {
                return Some(RadioDenial::MembershipChanged);
            }
        }
        for member in &room.members {
            let Some(agent) = self.agents.get(member) else {
                return Some(RadioDenial::ParticipantsNoLongerAvailable);
            };
            if agent
                .account_transfer
                .as_ref()
                .is_some_and(|value| !value.is_empty())
                || agent.native_thread_blocked
            {
                return Some(RadioDenial::ParticipantUnavailable);
            }
            if ready && (!agent.auto_wake || agent.native_failure_held) {
                return Some(RadioDenial::ParticipantStopped);
            }
        }
        None
    }
}

impl Agent {
    fn is_lead_for(&self, path: &str) -> bool {
        self.is_lead
            && !self.deleted
            && self.parent_id.is_none()
            && self.root_id.as_deref() == Some(&self.id)
            && self.cwd.as_deref() == Some(path)
    }
}

fn bind_thread(
    room: &Room,
    active: &ActiveTurn,
    agent: &Agent,
    event_id: &str,
) -> Option<RadioThreadBinding> {
    if active.thread_id.is_some() {
        return None;
    }
    let thread_id = agent.thread_id.as_ref()?;
    let attempt = agent.start_attempt.as_ref()?;
    if attempt.events.as_slice() != [event_id]
        || attempt.epoch != Some(active.epoch)
        || agent.epoch != Some(active.epoch)
        || attempt
            .thread_id
            .as_ref()
            .is_some_and(|attempt_thread| attempt_thread != thread_id)
    {
        return None;
    }
    Some(RadioThreadBinding {
        room_id: room.id.clone(),
        event_id: event_id.to_owned(),
        agent_id: agent.id.clone(),
        thread_id: thread_id.clone(),
        epoch: active.epoch,
        compactions: agent.compactions.unwrap_or(Some(0)),
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RadioDenial {
    NoActiveRoomReceipt,
    AmbiguousRoomReceipt,
    ParticipantsNoLongerAvailable,
    MembershipChanged,
    ParticipantUnavailable,
    ParticipantStopped,
    TurnNoLongerAuthorized,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RadioThreadBinding {
    pub room_id: String,
    pub event_id: String,
    pub agent_id: String,
    pub thread_id: String,
    pub epoch: u64,
    pub compactions: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum RadioTurnDecision {
    Allow { binding: Option<RadioThreadBinding> },
    Deny(RadioDenial),
}
