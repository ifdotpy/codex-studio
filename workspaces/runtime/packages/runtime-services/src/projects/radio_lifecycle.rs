//! Scheduler, answer, notification and account-worker hooks for local radio.
use super::*;

/// One scheduler tick. Preparation happens outside the transaction; the owner
/// publishes changes and hands off returned interrupts only after commit.
pub fn tick(
    db: &Transaction<'_>,
    prepared: &PreparedHost,
    ingress: &mut dyn RadioIngress,
    now: f64,
) -> Result<Vec<Interrupt>, ProjectsError> {
    prepared.recheck(db)?;
    let all_agents = agents(db)?;
    let mut effects = Vec::new();
    for mut room in rooms(db)? {
        let before = room.materialize()?["radio"].clone();
        if let Some(active) = &room.radio.active
            && unsent_cancelled(db, active)?
        {
            let stopping = room.radio.status == Status::Stopping;
            room.radio.active = None;
            room.radio.speaker = None;
            room.radio.next.clear();
            room.radio.status = if stopping {
                Status::Idle
            } else {
                Status::Blocked
            };
            room.radio.error=(!stopping).then(||"The shared input was cancelled before delivery. Send a new message to continue.".to_owned());
        }
        let denial = validate(db, &room, prepared, true)?;
        if let Some(denial) = denial
            && room.radio.status != Status::Stopping
        {
            room.blocked(denial);
            if let Some(active) = &room.radio.active
                && cancel_pending(db, active)?
            {
                room.radio.active = None;
                room.radio.speaker = None;
            }
        } else if let Some(mut active) = room.radio.active.clone() {
            let agent = all_agents.get(&active.agent_id);
            let event = event(db, &active)?;
            if let Some(agent) = agent {
                bind(&mut active, agent);
                room.radio.active = Some(active.clone());
            }
            if agent.is_none_or(|a| {
                a["epoch"] != active.epoch || a["threadId"] != json!(active.thread_id)
            }) {
                room.blocked("The participant context changed during its reply.");
            } else if let (Some(agent), Some(event)) = (agent, event.as_ref()) {
                let turn_id = turn(agent, &active, Some(event));
                if let Some(turn_id) = &turn_id {
                    active.extra.insert("turnId".to_owned(), json!(turn_id));
                    room.radio.active = Some(active.clone());
                }
                if let Some(turn_id) = turn_id.as_deref()
                    && completed(db, &active.agent_id, turn_id)?
                {
                    let attempt = &agent["startAttempt"];
                    let awaiting = matches!(event.status.as_str(), "reserved" | "dispatching")
                        && attempt["events"] == json!([active.event_id])
                        && truthy(attempt.get("submitted"))
                        && [
                            attempt["turnId"].as_str(),
                            attempt["observedTurnId"].as_str(),
                        ]
                        .contains(&Some(turn_id));
                    if !awaiting {
                        if event.status != "delivered"
                            || event.turn_id.as_deref() != Some(turn_id)
                            || agent["lastCompletedTurn"] != turn_id
                        {
                            room.blocked("The shared reply outcome is unconfirmed.");
                        } else {
                            let success = agent["lastCompletedTurnStatus"] == "completed";
                            if denial.is_none() {
                                match reconcile(db, &room, agent, turn_id) {
                                    Ok(()) => {}
                                    Err(ProjectsError::Invalid(reason)) => {
                                        room.blocked(reason);
                                        save(db, &mut room, prepared, now)?;
                                        continue;
                                    }
                                    Err(error) => return Err(error),
                                }
                            }
                            let stopping = room.radio.status == Status::Stopping;
                            if success
                                && !stopping
                                && pending_question(db, &active.agent_id, turn_id)?
                            {
                                room.radio.status = Status::Waiting;
                                room.radio.error = None;
                            } else {
                                if success {
                                    room.radio.seen.insert(
                                        active.agent_id.clone(),
                                        json!({"identity":active.identity,"seq":active.through}),
                                    );
                                }
                                room.radio.active = None;
                                room.radio.speaker = None;
                                if stopping {
                                    room.radio.status = Status::Idle;
                                    room.radio.next.clear();
                                    room.radio.error = None;
                                } else if success {
                                    room.radio.status = if room.radio.next.is_empty() {
                                        Status::Idle
                                    } else {
                                        Status::Waiting
                                    };
                                    room.radio.error = None;
                                } else {
                                    room.blocked("The shared reply failed or was interrupted. Send a new message to continue.");
                                }
                            }
                        }
                    }
                } else if room.radio.status == Status::Stopping {
                    if let Some(turn_id) = turn_id {
                        let state = active
                            .extra
                            .get("interruptState")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        if state.is_none() {
                            active
                                .extra
                                .insert("interruptState".to_owned(), json!("pending"));
                            room.radio.active = Some(active.clone());
                        }
                        // Pending is a durable intent. If the process died before
                        // claiming it, the next scheduler tick reconstructs the effect.
                        if state.is_none() || state.as_deref() == Some("pending") {
                            effects.push(Interrupt {
                                request_id: format!("radio-interrupt:{}", active.event_id),
                                agent: agent.clone(),
                                turn_id,
                            });
                        }
                    }
                } else if matches!(event.status.as_str(), "uncertain" | "failed" | "cancelled") {
                    room.blocked(
                        "The shared input outcome is unknown or failed. It will not be sent again.",
                    );
                } else if room.radio.status != Status::Blocked {
                    room.radio.status = if turn_id.is_some() {
                        Status::Speaking
                    } else {
                        Status::Waiting
                    };
                }
            } else {
                room.blocked("The shared input receipt is missing.");
            }
        }
        if denial.is_none()
            && room.radio.active.is_none()
            && !room.radio.next.is_empty()
            && room.radio.status != Status::Blocked
        {
            let agent = all_agents
                .get(&room.radio.next[0])
                .ok_or_else(|| invalid(room.id()))?;
            let id = agent["id"].as_str().ok_or_else(|| invalid(room.id()))?;
            let busy = truthy(agent.get("inFlight")) || matches!(agent["status"].as_str(),Some("starting"|"running"|"approval")) || db.query_row("SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND status IN ('pending','reserved','dispatching','uncertain'))",[id],|r|r.get::<_,bool>(0))?;
            if !busy {
                let identity = identity(agent);
                let seen = room.radio.seen.get(id);
                let cursor = seen
                    .filter(|s| s["identity"] == identity)
                    .and_then(|s| s["seq"].as_u64())
                    .unwrap_or(0);
                let (count,characters)=db.query_row("SELECT count(*),coalesce(sum(length(text)),0) FROM runtime_chat_messages WHERE room=?1 AND seq>?2",params![room.id(),i64::try_from(cursor).map_err(|_|invalid(room.id()))?],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)))?;
                if count > MAX_INPUT as i64 || characters > MAX_INPUT as i64 {
                    room.blocked(
                        "Shared history exceeds the safe input size. No messages were omitted.",
                    );
                    room.radio.revision = room
                        .radio
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| invalid(room.id()))?;
                    save(db, &mut room, prepared, now)?;
                    continue;
                }
                let mut query = db.prepare("SELECT seq,sender,text,created FROM runtime_chat_messages WHERE room=?1 AND seq>?2 ORDER BY seq")?;
                let messages = query.query_map(params![room.id(),i64::try_from(cursor).map_err(|_|invalid(room.id()))?],|r| Ok(json!({"seq":r.get::<_,i64>(0)?,"sender":r.get::<_,String>(1)?,"text":r.get::<_,String>(2)?,"created":r.get::<_,f64>(3)?})))?.collect::<Result<Vec<_>,_>>()?;
                let participants = room.members().iter().map(|key| {
                    let a=all_agents.get(key).ok_or_else(||invalid(room.id()))?;
                    Ok(json!({"id":key,"name":a["name"],"model":a["model"],"provider":a.get("provider").unwrap_or(&json!("codex"))}))
                }).collect::<Result<Vec<_>,ProjectsError>>()?;
                let context = json!({"roomId":room.id(),"selfId":id,"participants":participants});
                let text = format!(
                    "This is a shared chat with two equal agents. It is your turn to reply. All your assistant text is visible to the user and the other participant. Reply normally here. Do not forward this conversation through messaging tools. Other independent work is not part of this room. Reply once, then yield.\nShared room (JSON):\n{}\nShared messages since your last successful reply (JSON):\n{}",
                    python_json(&context),
                    python_json(&json!(messages))
                );
                if text.chars().count() > MAX_INPUT {
                    room.blocked(
                        "Shared history exceeds the safe input size. No messages were omitted.",
                    );
                } else {
                    // Stable ID is bound to this room revision; rollback retries do not
                    // mint a different operation, and committed floors are never enqueued again.
                    let event_id = format!(
                        "radio-turn:{}",
                        Uuid::new_v5(
                            &Uuid::NAMESPACE_URL,
                            format!("{}:{}", room.id(), room.radio.revision).as_bytes()
                        )
                    );
                    ingress.enqueue(db, id, &event_id, &text, now)?;
                    room.radio.next.remove(0);
                    room.radio.status = Status::Waiting;
                    room.radio.speaker = Some(id.to_owned());
                    room.radio.active = Some(Active {
                        event_id,
                        agent_id: id.to_owned(),
                        epoch: agent["epoch"].as_u64().ok_or_else(|| invalid(room.id()))?,
                        thread_id: agent["threadId"].as_str().map(str::to_owned),
                        identity,
                        through: messages
                            .last()
                            .and_then(|m| m["seq"].as_u64())
                            .unwrap_or(cursor),
                        extra: Map::new(),
                    });
                }
            }
        }
        if room.materialize()?["radio"] != before {
            room.radio.revision = room
                .radio
                .revision
                .checked_add(1)
                .ok_or_else(|| invalid(room.id()))?;
            save(db, &mut room, prepared, now)?;
        }
    }
    Ok(effects)
}
fn identity(a: &Value) -> Value {
    json!([
        a["threadId"],
        a["epoch"],
        a.get("compactions").unwrap_or(&json!(0))
    ])
}
fn bind(active: &mut Active, a: &Value) {
    let attempt = &a["startAttempt"];
    if active.thread_id.is_none()
        && a["threadId"].as_str().is_some_and(|s| !s.is_empty())
        && attempt["events"] == json!([active.event_id])
        && attempt["epoch"] == active.epoch
        && a["epoch"] == active.epoch
        && (attempt["threadId"].is_null() || attempt["threadId"] == a["threadId"])
    {
        active.thread_id = a["threadId"].as_str().map(str::to_owned);
        active.identity = identity(a);
    }
}
fn reconcile(
    db: &Transaction<'_>,
    room: &Room,
    agent: &Value,
    turn: &str,
) -> Result<(), ProjectsError> {
    let active = room
        .radio
        .active
        .as_ref()
        .ok_or_else(|| invalid(room.id()))?;
    let mut query=db.prepare("SELECT id,record,created FROM runtime_items WHERE agent=?1 AND json_extract(record,'$.turnId')=?2 AND json_extract(record,'$.role')='assistant' ORDER BY created,id")?;
    let rows = query
        .query_map(
            params![agent["id"].as_str().unwrap_or_default(), turn],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, raw, created) in rows {
        let item: Value = serde_json::from_str(&raw).map_err(|_| invalid(&id))?;
        let stored =
            studio_runtime_store::TranscriptRepository::resolve_item(db, &active.agent_id, &id)
                .map_err(|_| invalid(&id))?
                .ok_or_else(|| invalid(&id))?;
        let text = studio_runtime_store::TranscriptRepository::full_text(db, &stored)
            .map_err(|_| invalid(&id))?
            .unwrap_or_default();
        if truthy(item.get("truncated")) && text.is_empty() {
            return Err(ProjectsError::Invalid(
                "The full shared reply is unavailable. No truncated reply was forwarded.",
            ));
        }
        message(
            db,
            room,
            &format!("{}:{}:{id}", room.id(), active.event_id),
            &active.agent_id,
            &text,
            created,
        )?;
    }
    Ok(())
}

/// P04 calls before native send, and with accepted=true only after acceptance.
/// It marks the question answered in this same transaction. No provider call here.
pub fn route_question_answer(
    db: &Transaction<'_>,
    prepared: &PreparedHost,
    question: &Value,
    text: &str,
    receipt: Option<&NativeAcceptanceReceipt>,
    now: f64,
) -> Result<bool, ProjectsError> {
    if question["method"] != "agent/asyncQuestion"
        || question["turnId"].as_str().is_none_or(str::is_empty)
    {
        return Ok(false);
    }
    prepared.recheck(db)?;
    for mut room in rooms(db)? {
        let Some(active) = room.radio.active.as_ref() else {
            continue;
        };
        if active.agent_id != question["agent"] {
            continue;
        }
        let Some(agent) = records::get(db, "runtime_agents", &active.agent_id)? else {
            continue;
        };
        let event = event(db, active)?;
        let turn = turn(&agent, active, event.as_ref());
        if turn.as_deref() != question["turnId"].as_str()
            || question["epoch"] != active.epoch
            || agent["epoch"] != active.epoch
            || agent["threadId"] != json!(active.thread_id)
        {
            continue;
        }
        if let Some(denial) = validate(db, &room, prepared, true)? {
            return Err(ProjectsError::Invalid(denial));
        }
        if matches!(room.radio.status, Status::Blocked | Status::Stopping) {
            return Err(ProjectsError::Invalid(
                "The shared reply is stopped or blocked",
            ));
        }
        let key = format!(
            "{}:answer",
            question["id"].as_str().ok_or_else(|| invalid(room.id()))?
        );
        if let Some(receipt) = receipt {
            if receipt.agent_id != active.agent_id
                || receipt.epoch != active.epoch
                || Some(receipt.turn_id.as_str()) != question["turnId"].as_str()
                || receipt.event_id != active.event_id
            {
                return Ok(false);
            }
            message(db, &room, &key, "user", text, now)?;
            save(db, &mut room, prepared, now)?;
            return Ok(false);
        }
        let turn = turn.as_deref().ok_or_else(|| invalid(room.id()))?;
        if completed(db, &active.agent_id, turn)? {
            if event
                .as_ref()
                .is_none_or(|e| e.status != "delivered" || e.turn_id.as_deref() != Some(turn))
                || agent["lastCompletedTurn"] != turn
                || agent["lastCompletedTurnStatus"] != "completed"
            {
                return Err(ProjectsError::Invalid(
                    "The shared question turn outcome is unconfirmed",
                ));
            }
            let old = db
                .query_row(
                    "SELECT text FROM runtime_chat_messages WHERE id=?1",
                    [&key],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            if old.as_ref().is_some_and(|s| s != text) {
                return Err(ProjectsError::Invalid(
                    "This question already has a different answer",
                ));
            }
            if old.is_none() {
                message(db, &room, &key, "user", text, now)?;
                let active = room
                    .radio
                    .active
                    .as_mut()
                    .ok_or(ProjectsError::Invalid("radio floor missing"))?;
                if !truthy(active.extra.get("questionContinuationPlanned")) {
                    room.radio.next.insert(0, active.agent_id.clone());
                    active
                        .extra
                        .insert("questionContinuationPlanned".to_owned(), json!(true));
                }
                room.radio.revision = room
                    .radio
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| invalid(room.id()))?;
                save(db, &mut room, prepared, now)?;
            }
            return Ok(true);
        }
        return Ok(false);
    }
    Ok(false)
}

/// Proof that the native worker accepted the exact radio turn. Fields stay
/// private so callers cannot substitute a boolean for the worker receipt.
pub struct NativeAcceptanceReceipt {
    agent_id: String,
    epoch: u64,
    turn_id: String,
    event_id: String,
}
impl NativeAcceptanceReceipt {
    pub fn from_native(receipt: &crate::turn_prepare::NativeReceipt) -> Option<Self> {
        let crate::turn_prepare::ReceiptOutcome::Accepted { turn_id } = &receipt.outcome else {
            return None;
        };
        let event_id = receipt.event_ids.first()?.clone();
        if receipt.event_ids.len() != 1 || turn_id.is_empty() {
            return None;
        }
        Some(Self {
            agent_id: receipt.actor_id.clone(),
            epoch: receipt.epoch.1,
            turn_id: turn_id.clone(),
            event_id,
        })
    }
}

/// Claim a persisted interrupt immediately before handing it to the native
/// transport. Once claimed, its outcome is Unknown and restart never resends it.
pub fn claim_interrupt(
    db: &Transaction<'_>,
    prepared: &PreparedHost,
    event_id: &str,
    now: f64,
) -> Result<bool, ProjectsError> {
    prepared.recheck(db)?;
    for mut room in rooms(db)? {
        let Some(active) = room
            .radio
            .active
            .as_mut()
            .filter(|active| active.event_id == event_id)
        else {
            continue;
        };
        if room.radio.status != Status::Stopping
            || active.extra.get("interruptState").and_then(Value::as_str) != Some("pending")
        {
            return Ok(false);
        }
        active
            .extra
            .insert("interruptState".to_owned(), json!("unknown"));
        save(db, &mut room, prepared, now)?;
        return Ok(true);
    }
    Ok(false)
}

pub fn holds_floor(db: &Connection, agent: &str) -> Result<bool, ProjectsError> {
    for room in rooms(db)? {
        if let Some(active) = room.radio.active.as_ref().filter(|a| a.agent_id == agent)
            && let Some(a) = records::get(db, "runtime_agents", agent)?
            && let Some(turn) = turn(&a, active, event(db, active)?.as_ref())
            && pending_question(db, agent, &turn)?
            && completed(db, agent, &turn)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Persist an authorized first-thread binding with the worker reservation.
pub fn bind_reserved(
    db: &Transaction<'_>,
    prepared: &PreparedHost,
    recipient: &str,
    event_id: &str,
    now: f64,
) -> Result<(), ProjectsError> {
    let mut matching = rooms(db)?
        .into_iter()
        .filter(|r| {
            r.radio
                .active
                .as_ref()
                .is_some_and(|a| a.event_id == event_id)
        })
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(ProjectsError::Invalid(
            "The shared turn has no unique active room receipt",
        ));
    }
    let mut room = matching.remove(0);
    match policy(db, &room, prepared, recipient)?.authorize_turn(event_id) {
        RadioTurnDecision::Deny(denial) => Err(ProjectsError::Invalid(denial_text(denial))),
        RadioTurnDecision::Allow { binding } => {
            if let Some(binding) = binding {
                let active = room
                    .radio
                    .active
                    .as_mut()
                    .ok_or(ProjectsError::Invalid("radio floor missing"))?;
                active.thread_id = Some(binding.thread_id.clone());
                active.identity = json!([binding.thread_id, binding.epoch, binding.compactions]);
                save(db, &mut room, prepared, now)?;
            }
            Ok(())
        }
    }
}

/// Mirror only output belonging to the exact shared turn; private work stays private.
pub struct ObservedItem<'a> {
    pub agent_id: &'a str,
    pub item_key: &'a str,
    pub role: &'a str,
    pub text: &'a str,
    pub metadata: &'a Value,
}
pub fn observe_item(
    db: &Transaction<'_>,
    prepared: &PreparedHost,
    item: ObservedItem<'_>,
    now: f64,
) -> Result<(), ProjectsError> {
    let ObservedItem {
        agent_id,
        item_key,
        role,
        text,
        metadata,
    } = item;
    if role != "assistant" || metadata["turnId"].as_str().is_none() {
        return Ok(());
    }
    let Some(agent) = records::get(db, "runtime_agents", agent_id)? else {
        return Ok(());
    };
    for mut room in rooms(db)? {
        if !room
            .radio
            .active
            .as_ref()
            .is_some_and(|a| a.agent_id == agent_id)
            || validate(db, &room, prepared, false)?.is_some()
        {
            continue;
        }
        let active = room
            .radio
            .active
            .as_mut()
            .ok_or(ProjectsError::Invalid("radio floor missing"))?;
        bind(active, &agent);
        if agent["epoch"] != active.epoch || agent["threadId"] != json!(active.thread_id) {
            continue;
        }
        if let Some(turn) = turn(&agent, active, event(db, active)?.as_ref())
            && metadata["turnId"] == turn
        {
            let key = format!(
                "{}:{}:{item_key}",
                room.id(),
                room.radio
                    .active
                    .as_ref()
                    .ok_or_else(|| invalid(room.id()))?
                    .event_id
            );
            message(db, &room, &key, agent_id, text, now)?;
            save(db, &mut room, prepared, now)?;
        }
    }
    Ok(())
}

/// P03 invokes before post_message_with_admission in the same transaction.
pub fn guard_message(db: &Connection, sender: &str, recipient: &str) -> Result<(), ProjectsError> {
    let Some(agent) = records::get(db, "runtime_agents", sender)? else {
        return Ok(());
    };
    for room in rooms(db)? {
        if let Some(active) = room.radio.active.as_ref().filter(|a| a.agent_id == sender)
            && room.members().iter().any(|m| m == recipient)
            && let Some(turn) = turn(&agent, active, event(db, active)?.as_ref())
            && agent["turnId"] == turn
            && truthy(agent.get("inFlight"))
        {
            return Err(ProjectsError::Invalid(
                "Your reply is shared automatically. Reply in this turn and let Studio pass the floor.",
            ));
        }
    }
    Ok(())
}

/// Account-worker queue selection: an exact radio input is always a single batch.
/// Ordinary inputs may enter a shared turn only with that turn's answer receipt.
pub fn select_pending(
    db: &Transaction<'_>,
    prepared: &PreparedHost,
    agent: &Value,
    rows: &[QueueEvent],
    now: f64,
) -> Result<Vec<QueueEvent>, ProjectsError> {
    let id = agent["id"]
        .as_str()
        .ok_or(ProjectsError::Invalid("agent ID missing"))?;
    for event in rows.iter().filter(|e| e.kind == "radio_turn") {
        match bind_reserved(db, prepared, id, &event.id.0, now) {
            Ok(()) => return Ok(vec![event.clone()]),
            Err(ProjectsError::Invalid(_)) => {}
            Err(error) => return Err(error),
        }
    }
    let ordinary = rows
        .iter()
        .filter(|e| e.kind != "radio_turn")
        .cloned()
        .collect::<Vec<_>>();
    for room in rooms(db)? {
        if let Some(active) = room.radio.active.as_ref().filter(|a| a.agent_id == id)
            && truthy(agent.get("inFlight"))
            && let Some(turn) = turn(agent, active, event(db, active)?.as_ref())
            && agent["turnId"] == turn
        {
            for row in &ordinary {
                if let Some(meta) = records::get(db, "runtime_event_meta", &row.id.0)?
                    && meta["radioAnswerTurnId"] == turn
                {
                    return Ok(vec![row.clone()]);
                }
            }
            return Ok(Vec::new());
        }
    }
    Ok(ordinary)
}
