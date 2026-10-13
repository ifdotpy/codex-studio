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
#[derive(Clone, Deserialize, Serialize)]
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
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Radio {
    #[serde(rename = "teamId")]
    pub(super) team_id: String,
    pub(super) revision: u64,
    pub(super) status: Status,
    pub(super) speaker: Option<String>,
    pub(super) next: Vec<String>,
    pub(super) active: Option<Active>,
    pub(super) error: Option<String>,
    pub(super) seen: BTreeMap<String, Value>,
    #[serde(flatten)]
    pub(super) extra: Map<String, Value>,
}
pub(super) struct Room {
    pub(super) value: Value,
    pub(super) radio: Radio,
}
impl Room {
    pub(super) fn checked(value: Value) -> Result<Self, ProjectsError> {
        let id = value.get("id").and_then(Value::as_str).unwrap_or_default();
        let value = records::validate("runtime_rooms", id, value.clone())?;
        let radio: Radio =
            serde_json::from_value(value["radio"].clone()).map_err(|_| invalid(id))?;
        if !id.starts_with("radio:") || radio.next.len() > 5 || radio.seen.len() > 2 {
            return Err(invalid(id));
        }
        Ok(Self { value, radio })
    }
    pub(super) fn id(&self) -> &str {
        self.value["id"].as_str().unwrap_or_default()
    }
    pub(super) fn members(&self) -> Vec<String> {
        self.value["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    }
    pub(super) fn materialize(&self) -> Result<Value, ProjectsError> {
        let mut value = self.value.clone();
        value["radio"] = serde_json::to_value(&self.radio).map_err(|_| invalid(self.id()))?;
        Ok(value)
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
    records::all(db, "runtime_rooms")?
        .into_iter()
        .filter(|row| {
            row["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("radio:"))
                && row["radio"].is_object()
        })
        .map(Room::checked)
        .collect()
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
    let value = room.materialize()?;
    let inputs = RadioAuthorizationRecords {
        recipient_id: if recipient.is_empty() {
            room.members()
                .first()
                .cloned()
                .ok_or_else(|| invalid(room.id()))?
        } else {
            recipient.to_owned()
        },
        rooms: BTreeMap::from([(room.id().to_owned(), value.to_string())]),
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
    room.value = room.materialize()?;
    room.value["updated"] = json!(now);
    records::validate("runtime_rooms", room.id(), room.value.clone())?;
    db.execute("INSERT INTO runtime_rooms(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![room.id(),room.value.to_string()])?;
    publish_room(db, &room.value, prepared)
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
    let context = RoomViewContext {
        agents: all_agents.into_values().collect(),
        peer_teams: teams,
        last_message: last,
        federation: None,
    };
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
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_completed_turns WHERE id=?1)",
        [format!("{agent}:{turn}")],
        |r| r.get(0),
    )?)
}
pub(super) fn pending_question(
    db: &Connection,
    agent: &str,
    turn: &str,
) -> Result<bool, ProjectsError> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM runtime_requests WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.turnId')=?2 AND json_extract(record,'$.method')='agent/asyncQuestion' AND json_extract(record,'$.status')='pending')",params![agent,turn],|r|r.get(0))?)
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
