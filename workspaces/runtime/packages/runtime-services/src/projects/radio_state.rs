//! Checked radio records and transactional store projections.
use super::*;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Status {
    Idle,
    Waiting,
    Speaking,
    Blocked,
    Stopping,
}
#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct Active {
    pub(super) event_id: String,
    pub(super) agent_id: String,
    pub(super) epoch: u64,
    pub(super) thread_id: Option<String>,
    pub(super) identity: Value,
    pub(super) through: u64,
    #[serde(flatten)]
    pub(super) extra: Map<String, Value>,
}
#[derive(Clone, Deserialize, Serialize, PartialEq)]
pub(crate) struct Radio {
    #[serde(rename = "teamId")]
    pub(super) team_id: String,
    pub(super) revision: u64,
    pub(super) status: Status,
    pub(super) speaker: Option<String>,
    pub(super) next: Vec<String>,
    pub(super) direct: bool,
    #[serde(deserialize_with = "present_active")]
    pub(super) active: Option<Active>,
    pub(super) error: Option<String>,
    pub(super) seen: BTreeMap<String, Value>,
    #[serde(flatten)]
    pub(super) extra: Map<String, Value>,
}
impl Radio {
    pub(super) fn new(team_id: String, direct: bool) -> Self {
        Self {
            team_id,
            revision: 0,
            status: Status::Idle,
            speaker: None,
            next: Vec::new(),
            direct,
            active: None,
            error: None,
            seen: BTreeMap::new(),
            extra: Map::new(),
        }
    }
}
fn present_active<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Active>, D::Error> {
    Option::<Active>::deserialize(d)
}
#[derive(Deserialize, Serialize)]
pub(super) struct Room {
    pub(super) id: String,
    pub(super) radio: Radio,
    #[serde(flatten)]
    pub(super) fields: Map<String, Value>,
}
impl Room {
    pub(super) fn checked(id: &str, raw: &str) -> Result<Self, ProjectsError> {
        records::decode("runtime_rooms", id, raw)?;
        let room: Self = serde_json::from_str(raw).map_err(|_| invalid(id))?;
        if !id.starts_with("radio:") || room.radio.next.len() > 5 || room.radio.seen.len() > 2 {
            return Err(invalid(id));
        }
        Ok(room)
    }
    pub(super) fn id(&self) -> &str {
        &self.id
    }
    pub(super) fn members(&self) -> Vec<String> {
        self.fields
            .get("members")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    }
    pub(super) fn blocked(&mut self, text: &str) {
        self.radio.status = Status::Blocked;
        self.radio.error = Some(text.to_owned());
        self.radio.next.clear();
    }
}
pub(super) fn invalid(id: &str) -> ProjectsError {
    ProjectsError::InvalidStored {
        table: "runtime_rooms",
        id: id.to_owned(),
    }
}
pub(super) fn rooms(db: &Connection) -> Result<Vec<Room>, ProjectsError> {
    if !records::exists(db, "runtime_rooms")? {
        return Ok(Vec::new());
    }
    let mut stmt = db.prepare("SELECT id,record FROM runtime_rooms ORDER BY rowid")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut rooms = Vec::new();
    for row in rows {
        let (id, raw) = row?;
        let value = records::decode("runtime_rooms", &id, &raw)?;
        if id.starts_with("radio:") && value["radio"].is_object() {
            rooms.push(Room::checked(&id, &raw)?);
        }
    }
    Ok(rooms)
}
pub(super) fn agents(db: &Connection) -> Result<BTreeMap<String, Value>, ProjectsError> {
    records::all(db, "runtime_agents")?
        .into_iter()
        .map(|a| {
            let id = a["id"]
                .as_str()
                .ok_or(ProjectsError::Invalid("stored agent id is invalid"))?
                .to_owned();
            Ok((id, a))
        })
        .collect()
}
pub(super) fn policy(
    db: &Connection,
    room: &Room,
    prepared: &PreparedHost,
    recipient: &str,
) -> Result<RadioAuthorizationSnapshot, ProjectsError> {
    let value = serde_json::to_string(room).map_err(studio_runtime_store::StoreError::from)?;
    let inputs = RadioAuthorizationRecords {
        recipient_id: if recipient.is_empty() {
            room.members()
                .first()
                .cloned()
                .ok_or_else(|| invalid(room.id()))?
        } else {
            recipient.to_owned()
        },
        rooms: BTreeMap::from([(room.id().to_owned(), value)]),
        agents: agents(db)?
            .into_iter()
            .map(|(id, a)| (id, a.to_string()))
            .collect(),
        projects: ProjectsRepository::list(db)?
            .into_iter()
            .map(|(id, p)| (id, p.to_string()))
            .collect(),
        resolved_paths: prepared.resolved_paths().clone(),
    };
    RadioAuthorizationSnapshot::from_records(&inputs).map_err(|e| ProjectsError::InvalidStored {
        table: "radio",
        id: e.location,
    })
}
pub(super) fn denial_text(denial: RadioDenial) -> &'static str {
    match denial {
        RadioDenial::ParticipantsNoLongerAvailable => {
            "The shared chat participants are no longer available in this project."
        }
        RadioDenial::MembershipChanged => {
            "The shared chat membership changed. Its history stays with its original members."
        }
        RadioDenial::ParticipantUnavailable => {
            "A participant is being transferred or its native chat is unavailable."
        }
        RadioDenial::ParticipantStopped => {
            "A participant is stopped. Resume that chat before sending."
        }
        RadioDenial::NoActiveRoomReceipt => "The shared turn has no active room receipt",
        RadioDenial::AmbiguousRoomReceipt => "The shared turn has multiple room receipts",
        RadioDenial::TurnNoLongerAuthorized => "The shared turn is no longer authorized",
    }
}
pub(super) fn validate(
    db: &Connection,
    room: &Room,
    prepared: &PreparedHost,
    ready: bool,
) -> Result<Option<&'static str>, ProjectsError> {
    Ok(policy(db, room, prepared, "")?
        .validate_room(room.id(), ready)
        .map(denial_text))
}
pub(super) fn save(
    db: &Transaction<'_>,
    room: &mut Room,
    prepared: &PreparedHost,
    now: f64,
) -> Result<(), ProjectsError> {
    room.fields.insert("updated".to_owned(), json!(now));
    let raw = serde_json::to_string(&*room).map_err(studio_runtime_store::StoreError::from)?;
    records::decode("runtime_rooms", room.id(), &raw)?;
    db.execute("INSERT INTO runtime_rooms(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![room.id(),raw])?;
    let view = studio_runtime_store::projection_value(room);
    publish_room(db, &view, prepared)
}
fn agent_projection(
    value: &Value,
) -> Result<crate::submit_gates::AgentAuthorityRow, ProjectsError> {
    use crate::submit_gates::{AgentAuthorityRow, Deletion, StoredField};
    let invalid = || ProjectsError::InvalidStored {
        table: "runtime_agents",
        id: value["id"].as_str().unwrap_or_default().to_owned(),
    };
    let string = |key: &str| match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        _ => Err(invalid()),
    };
    let deleted_at = match value.get("deletedAt") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(value)) => Some(Deletion::Bool(*value)),
        Some(Value::Number(value)) => Some(Deletion::Number(value.as_f64().ok_or_else(invalid)?)),
        _ => return Err(invalid()),
    };
    let is_lead = match value.get("isLead") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(value)) => Some(*value),
        _ => return Err(invalid()),
    };
    let name = match value.get("name") {
        None => StoredField::Missing,
        Some(Value::Null) => StoredField::Null,
        Some(Value::String(name)) => StoredField::Value(name.clone()),
        _ => return Err(invalid()),
    };
    Ok(AgentAuthorityRow {
        id: string("id")?,
        root_id: string("rootId")?,
        parent_id: string("parentId")?,
        is_lead,
        deleted_at,
        cwd: string("cwd")?,
        name,
        shared_room_id: string("sharedRoomId")?,
        execution_mode: string("executionMode")?,
    })
}
fn team_projection(value: &Value) -> Result<crate::submit_gates::PeerTeamRow, ProjectsError> {
    let invalid = || ProjectsError::InvalidStored {
        table: "runtime_projects",
        id: value["id"].as_str().unwrap_or_default().to_owned(),
    };
    Ok(crate::submit_gates::PeerTeamRow {
        id: value["id"].as_str().ok_or_else(invalid)?.to_owned(),
        name: value["name"].as_str().ok_or_else(invalid)?.to_owned(),
        members: value["members"]
            .as_array()
            .ok_or_else(invalid)?
            .iter()
            .map(|v| v.as_str().map(str::to_owned).ok_or_else(invalid))
            .collect::<Result<_, _>>()?,
    })
}
pub(super) fn publish_room(
    db: &Transaction<'_>,
    value: &Value,
    prepared: &PreparedHost,
) -> Result<(), ProjectsError> {
    let id = value["id"].as_str().ok_or_else(|| invalid("room/id"))?;
    let all_agents = project_agents(db)?;
    let mut teams = Vec::new();
    for (_, p) in ProjectsRepository::list(db)? {
        if let (Some(path), Some(raw)) = (p["path"].as_str(), p["peerTeams"].as_array()) {
            teams.extend(live_teams(
                raw,
                &all_agents,
                prepared.resolve(path)?,
                prepared,
            )?);
        }
    }
    let last = db.query_row("SELECT seq,text,created,sender FROM runtime_chat_messages WHERE room=?1 ORDER BY seq DESC LIMIT 1",[id],|r| Ok(json!({"seq":r.get::<_,i64>(0)?,"text":r.get::<_,String>(1)?.chars().take(180).collect::<String>(),"created":r.get::<_,f64>(2)?,"sender":r.get::<_,String>(3)?}))).optional()?;
    let agents = all_agents
        .into_values()
        .map(|value| agent_projection(&value))
        .collect::<Result<_, _>>()?;
    let teams = teams
        .into_iter()
        .map(|value| team_projection(&value))
        .collect::<Result<_, _>>()?;
    let mut context = RoomViewContext::new(agents, teams);
    context.last_message = last;
    let view = room_entity_view(value, &context);
    SyncDocumentsRepository::put_entity(
        db,
        "room",
        id,
        view.clone().unwrap_or(json!({})),
        view.is_none(),
    )?;
    Ok(())
}
pub(super) fn message(
    db: &Transaction<'_>,
    room: &Room,
    key: &str,
    sender: &str,
    text: &str,
    now: f64,
) -> Result<(), ProjectsError> {
    db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?1,?2,?3,?4,?5,'{}') ON CONFLICT(id) DO UPDATE SET text=excluded.text",params![key,room.id(),sender,text,now])?;
    Ok(())
}
pub(super) fn event(db: &Connection, active: &Active) -> Result<Option<QueueEvent>, ProjectsError> {
    Ok(QueueEventRepository::get(
        db,
        &QueueEventId::from(active.event_id.as_str()),
    )?)
}
pub(super) fn turn(agent: &Value, active: &Active, event: Option<&QueueEvent>) -> Option<String> {
    let event = event?;
    if event.agent.0 != agent["id"] || u64::try_from(event.epoch).ok() != Some(active.epoch) {
        return None;
    }
    if let Some(id) = &event.turn_id {
        return Some(id.clone());
    }
    let attempt = &agent["startAttempt"];
    if attempt["events"] == json!([active.event_id])
        && attempt["epoch"] == active.epoch
        && truthy(attempt.get("submitted"))
        && attempt["threadId"] == agent["threadId"]
    {
        return attempt["turnId"]
            .as_str()
            .or_else(|| attempt["observedTurnId"].as_str())
            .map(str::to_owned);
    }
    None
}
pub(super) fn completed(db: &Connection, agent: &str, turn: &str) -> Result<bool, ProjectsError> {
    Ok(studio_runtime_store::domain::turn_completed(
        db, agent, turn,
    )?)
}
pub(super) fn pending_question(
    db: &Connection,
    agent: &str,
    turn: &str,
) -> Result<bool, ProjectsError> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM approvals WHERE agent=?1 AND turn=?2 AND kind='agent_question' AND state='pending' AND hidden_at IS NULL)",params![agent,turn],|r|r.get(0))?)
}
pub(super) fn unsent_cancelled(db: &Connection, active: &Active) -> Result<bool, ProjectsError> {
    let Some(event) = event(db, active)? else {
        return Ok(false);
    };
    if event.status != "cancelled" || event.turn_id.is_some() || truthy(active.extra.get("turnId"))
    {
        return Ok(false);
    }
    let a = records::get(db, "runtime_agents", &active.agent_id)?.unwrap_or(Value::Null);
    let attempt = &a["startAttempt"];
    Ok(!attempt["events"]
        .as_array()
        .is_some_and(|ids| ids.contains(&json!(active.event_id)))
        || !["submitted", "observedTurnId", "turnId"]
            .iter()
            .any(|k| truthy(attempt.get(k))))
}
pub(super) fn cancel_pending(db: &Transaction<'_>, active: &Active) -> Result<bool, ProjectsError> {
    db.execute("UPDATE runtime_events SET status='cancelled',error='Shared chat stopped' WHERE id=?1 AND status='pending'",[&active.event_id])?;
    unsent_cancelled(db, active)
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    #[test]
    fn radio_requires_canonical_direct_and_active_fields() {
        let canonical = r#"{"teamId":"t","revision":0,"status":"idle","speaker":null,"next":[],"active":null,"error":null,"seen":{},"direct":false}"#;
        assert!(serde_json::from_str::<Radio>(canonical).is_ok());
        assert!(
            serde_json::from_str::<Radio>(&canonical.replace(",\"direct\":false", "")).is_err()
        );
        assert!(serde_json::from_str::<Radio>(&canonical.replace("\"active\":null,", "")).is_err());
        for (radio, valid) in [
            (canonical.to_owned(), true),
            (canonical.replace(",\"direct\":false", ""), false),
            (canonical.replace("\"active\":null,", ""), false),
        ] {
            let room: Value = serde_json::from_str(&format!(
                r#"{{"id":"radio:t","kind":"private","radio":{radio}}}"#
            ))
            .expect("radio schema fixture");
            assert_eq!(
                studio_runtime_schema::validate("PersistedRoomRecord", &room)
                    .expect("radio schema fixture"),
                valid
            );
        }
    }

    #[test]
    fn projection_rows_match_typed_decoding_and_reject_invalid_fields() {
        for raw in [
            r#"{"id":"a","name":null,"deletedAt":0,"cwd":"/p"}"#,
            r#"{"id":"b","name":"B","isLead":true,"deletedAt":false}"#,
        ] {
            let value: Value = serde_json::from_str(raw).expect("projection JSON");
            let expected: crate::submit_gates::AgentAuthorityRow =
                serde_json::from_str(raw).expect("typed agent");
            assert_eq!(
                agent_projection(&value).expect("agent projection"),
                expected
            );
        }
        assert!(agent_projection(&json!({"id":"a","isLead":"yes"})).is_err());
        let raw = r#"{"id":"t","name":"Team","members":["a","b"]}"#;
        let value: Value = serde_json::from_str(raw).expect("projection JSON");
        let expected: crate::submit_gates::PeerTeamRow =
            serde_json::from_str(raw).expect("typed team");
        assert_eq!(team_projection(&value).expect("team projection"), expected);
        assert!(team_projection(&json!({"id":"t","name":"T","members":[null]})).is_err());
    }
}
