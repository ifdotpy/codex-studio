//! Bounded model projections with the same authority as their full-text reader.
use super::*;
use crate::submit_gates::{
    AuthorizationScope, AuthorizationSnapshot, EventDenial, StoredEvent, authorize_events,
};
use rusqlite::OptionalExtension;
use serde_json::Map;
use studio_runtime_store::{TranscriptItem, TranscriptRepository, TranscriptStoreError};

pub(super) fn chat(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    args: &Value,
) -> Result<Value, CallbackError> {
    let room = text(args, "room_id")?;
    messages::authorize_room(tx, snapshot, room)?;
    let page = crate::coordination_reads::chat_read(tx, room, args["before"].as_i64(), None, 100)?;
    project_chat(&page)
}

pub(crate) fn project_chat(page: &Value) -> Result<Value, CallbackError> {
    let room = Value::Object(
        ["id", "kind", "rootId", "name"]
            .into_iter()
            .filter_map(|k| page["room"].get(k).map(|v| (k.into(), v.clone())))
            .collect(),
    );
    let messages = page["messages"]
        .as_array()
        .ok_or(CallbackError::Stored("chat messages".into()))?;
    let mut selected = Vec::new();
    for message in messages.iter().rev() {
        let mut brief = message
            .as_object()
            .cloned()
            .ok_or(CallbackError::Stored("chat message".into()))?;
        brief.remove("deliveries");
        let body = text(message, "text")?;
        if body.len() > 3000 {
            let end = body.floor_char_boundary(2400.min(body.len()));
            brief.insert("text".into(), json!(&body[..end]));
            brief.insert("truncated".into(), json!(true));
            brief.insert("textChars".into(), json!(body.chars().count()));
            brief.insert("read".into(), json!({"tool":"orchestration_read","output_ref":format!("chat-message:{}",text(message,"id")?)}));
        }
        let mut candidate = vec![Value::Object(brief)];
        candidate.extend(selected.iter().cloned());
        // Conservative byte bound includes the model's escaped text envelope.
        let bytes = json!([{"type":"inputText","text":json!({"room":room,"messages":candidate}).to_string()}]).to_string().len();
        if !selected.is_empty() && bytes > 13000 {
            break;
        }
        selected = candidate;
    }
    let next = if !page["nextBefore"].is_null() || selected.len() < messages.len() {
        selected
            .first()
            .map(|m| m["seq"].clone())
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    Ok(json!({"room":room,"messages":selected,"nextBefore":next}))
}

pub(super) fn read(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    args: &Value,
    now: f64,
    state_root: &std::path::Path,
) -> Result<Value, CallbackError> {
    let key = text(args, "output_ref")?;
    if let Some(hash) = key.strip_prefix("studio-input:") {
        return studio_input(tx, snapshot, key, hash, args, state_root);
    }
    if let Some(id) = key.strip_prefix("chat-message:") {
        let message = tx.query_row(
            "SELECT id,room,sender,seq,created,text FROM runtime_chat_messages WHERE id=?1", [id],
            |r| Ok(json!({"id":r.get::<_,String>(0)?,"room":r.get::<_,String>(1)?,"sender":r.get::<_,String>(2)?,"seq":r.get::<_,i64>(3)?,"created":r.get::<_,f64>(4)?,"text":r.get::<_,String>(5)?})),
        ).optional()?.ok_or(CallbackError::Invalid("Chat message is unavailable or you are not a participant"))?;
        messages::authorize_room(tx, snapshot, text(&message, "room")?)?;
        let mut identity = message.clone();
        identity
            .as_object_mut()
            .ok_or(CallbackError::Stored(id.into()))?
            .remove("text");
        return excerpt(
            key,
            text(&message, "text")?,
            args,
            json!({"source":"chat-message","identity":identity}),
            Map::new(),
        );
    }
    if let Some(id) = key.strip_prefix("event:") {
        return event(tx, snapshot, key, id, args);
    }
    let agent = snapshot.context().agent.as_str();
    let mut receipt = receipt(tx, agent, key, now, state_root)?;
    // Native item ids start with the stable Studio agent id, also after an
    // account transfer. A caller-supplied thread id never authorizes a read.
    if receipt["stage"] == "not_found"
        && let Some(call) = key
            .strip_prefix(agent)
            .and_then(|rest| rest.strip_prefix(':'))
        && let Some(item) = TranscriptRepository::item(tx, agent, key).map_err(transcript_error)?
    {
        match item.record["title"].as_str() {
            Some("commandExecution") => return command_output(tx, key, &item, args),
            Some("dynamicToolCall") => {
                let aliases = tx
                    .prepare_cached(
                        "SELECT request FROM runtime_tool_request_aliases WHERE agent=?1 AND alias=?2 LIMIT 2",
                    )?
                    .query_map([agent, call], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                if aliases.len() > 1 {
                    return Err(CallbackError::Invalid(
                        "Output reference is ambiguous. Use the canonical request id.",
                    ));
                }
                if let Some(request) = aliases.first() {
                    receipt = self::receipt(tx, agent, request, now, state_root)?;
                }
            }
            _ => {}
        }
    }
    if receipt["agent"] != agent || receipt["stage"] == "ambiguous" {
        return Err(CallbackError::Invalid(
            "Output reference is not owned by this agent",
        ));
    }
    let id = text(&receipt, "id")?;
    let mut result = records::get(tx, "runtime_tool_results", id)?
        .or_else(|| receipt.get("result").cloned())
        .ok_or(CallbackError::Invalid(
            "Result is not available yet. Do not repeat the operation.",
        ))?;
    if studio_runtime_store::truthy(result.get("_payloadBlobs")) {
        result = crate::payload_blobs::resolve_record(state_root, &result)
            .map_err(|_| CallbackError::Stored(format!("runtime_tool_results/{id} payload")))?;
    }
    tool_requests::expire(&mut result, now);
    let body = result["contentItems"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["type"] == "inputText")
        .map(|v| v["text"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let mut page = Map::new();
    if let Some(expiry) = result.get("serverOutputExpiresAt") {
        page.insert("outputExpiresAt".into(), expiry.clone());
    }
    excerpt(
        id,
        &body,
        args,
        json!({"success":result["success"],"outcome":receipt["outcome"]}),
        page,
    )
}

/// Read an immutable Studio input spill only through an event owned by this
/// actor. The blob hash identifies content, never permission to read it.
fn studio_input(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    key: &str,
    hash: &str,
    args: &Value,
    state_root: &std::path::Path,
) -> Result<Value, CallbackError> {
    let actor = snapshot.context().agent.as_str();
    let saved = tx.query_row(
        "SELECT e.id,e.kind,e.text,m.record FROM runtime_events e JOIN runtime_event_meta m ON e.id=m.id
         WHERE e.agent=?1 AND json_extract(m.record,'$.studioInput.sha256')=?2 ORDER BY e.created,e.id LIMIT 1",
        [actor, hash], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)),
    ).optional()?.ok_or(CallbackError::Invalid("Studio input reference is not owned by this agent"))?;
    authorize_event(
        tx,
        actor,
        StoredEvent {
            id: saved.0,
            kind: saved.1,
            text: saved.2,
        },
        Some(&saved.3),
    )?;
    let metadata: Value = serde_json::from_str(&saved.3)?;
    let bytes = crate::payload_blobs::load_blob(state_root, &metadata["studioInput"])
        .map_err(|_| CallbackError::Stored("Studio input payload".into()))?;
    let body =
        String::from_utf8(bytes).map_err(|_| CallbackError::Stored("Studio input text".into()))?;
    excerpt(
        key,
        &body,
        args,
        json!({"source":"studio-input"}),
        Map::new(),
    )
}

/// The receipt service's canonical alias resolution, with saved payloads
/// resolved from the state directory. Python's `tool_request` reads them too.
fn receipt(
    tx: &Transaction<'_>,
    agent: &str,
    key: &str,
    now: f64,
    state_root: &std::path::Path,
) -> Result<Value, CallbackError> {
    Ok(tool_requests::execute_http(
        tx,
        agent,
        tool_requests::Action::Get(key),
        now,
        state_root,
    )?)
}

/// Python's `model_saved_message` for an `event:` reference: only the
/// recipient reads its event, and only inside the current team boundary.
fn event(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    key: &str,
    id: &str,
    args: &Value,
) -> Result<Value, CallbackError> {
    let agent = snapshot.context().agent.as_str();
    let (kind, created, epoch, turn, body) = tx
        .query_row(
            "SELECT kind,created,epoch,turn_id,text FROM runtime_events WHERE id=?1 AND agent=?2",
            [id, agent],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?
        .ok_or(CallbackError::Invalid(
            "Event reference is not owned by this agent",
        ))?;
    let meta = if records::exists(tx, "runtime_event_meta")? {
        tx.query_row(
            "SELECT record FROM runtime_event_meta WHERE id=?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    } else {
        None
    };
    authorize_event(
        tx,
        agent,
        StoredEvent {
            id: id.into(),
            kind: kind.clone(),
            text: body.clone(),
        },
        meta.as_deref(),
    )?;
    let mut metadata = match meta {
        Some(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(metadata)) => metadata,
            _ => return Err(CallbackError::Stored(format!("runtime_event_meta/{id}"))),
        },
        None => Map::new(),
    };
    metadata.remove("contextManifest");
    let identity = json!({"id":id,"agent":agent,"kind":kind,"created":created,"epoch":epoch,"turn_id":turn,"metadata":metadata});
    excerpt(
        key,
        &body,
        args,
        json!({"source":"event","identity":identity}),
        Map::new(),
    )
}

/// Python's `codex_team_isolation.assert_events` for one stored event. Only
/// agent communication kinds cross a team boundary; other kinds need no rows.
fn authorize_event(
    tx: &Transaction<'_>,
    agent: &str,
    event: StoredEvent,
    meta: Option<&str>,
) -> Result<(), CallbackError> {
    if !matches!(
        event.kind.as_str(),
        "agent_message" | "chat_review" | "complaint_response" | "followup" | "radio_turn"
    ) {
        return Ok(());
    }
    let mut rows = messages::authorization_records(tx)?;
    rows.recipient_id = agent.into();
    if let Some(meta) = meta {
        rows.event_meta.insert(event.id.clone(), meta.into());
    }
    rows.events = vec![event];
    let scope = AuthorizationScope::of_events(agent, &rows.events, &rows.event_meta);
    for id in &scope.rooms {
        if let Some(room) = records::get(tx, "runtime_rooms", id)? {
            rows.rooms.insert(id.clone(), room.to_string());
        }
    }
    for id in &scope.complaints {
        if let Some(complaint) = records::get(tx, "runtime_complaints", id)? {
            rows.complaints.insert(id.clone(), complaint.to_string());
        }
    }
    let snapshot = AuthorizationSnapshot::from_records(&rows)
        .map_err(|e| CallbackError::Stored(e.location))?;
    authorize_events(&snapshot).map_err(event_denial)
}

/// The reason Python's `validate_event` gives for each denial.
fn event_denial(denial: EventDenial) -> CallbackError {
    CallbackError::Invalid(match denial {
        EventDenial::RecipientUnavailable => {
            "Agent communication is allowed only within one team: the recipient team is unavailable"
        }
        EventDenial::RoomUnavailable => {
            "Agent communication is allowed only within one team: the message room is unavailable"
        }
        EventDenial::BroadcastDisabled => {
            "Agent communication is allowed only within one team: global and foreign broadcasts are disabled"
        }
        EventDenial::RoomBoundary => {
            "Agent communication is allowed only within one team: the message room crosses team boundaries"
        }
        EventDenial::ComplaintUnavailable => {
            "Agent communication is allowed only within one team: the complaint is unavailable"
        }
        EventDenial::ComplaintRecipient => {
            "Agent communication is allowed only within one team: the complaint recipient does not match"
        }
        EventDenial::ComplaintResponderUnavailable => {
            "Agent communication is allowed only within one team: the complaint responder is unavailable"
        }
        EventDenial::ComplaintResponder => {
            "Agent communication is allowed only within one team: the complaint responder does not match"
        }
        EventDenial::TeamBoundary => "Agent communication is allowed only within one team",
        EventDenial::ChatReviewRemoved => "Scheduled chat reviews have been removed",
        // Python checks a radio turn against the radio policy, which has no
        // Rust owner for reads yet.
        EventDenial::RadioUnsupported => {
            return CallbackError::Dependency("radio-policy/event-read");
        }
    })
}

/// Python's saved native command output: the transcript item of this agent's
/// own command, with its complete text when the item was truncated.
fn command_output(
    tx: &Transaction<'_>,
    key: &str,
    item: &TranscriptItem,
    args: &Value,
) -> Result<Value, CallbackError> {
    let unreadable = || {
        CallbackError::Invalid(
            "Saved command output is truncated or unreadable. Do not repeat the command.",
        )
    };
    let item_truncated = studio_runtime_store::truthy(item.record.get("truncated"));
    let full = if item_truncated {
        TranscriptRepository::full_text(tx, item)
            .map_err(transcript_error)?
            .filter(|text| !text.is_empty())
    } else {
        None
    };
    let raw = match &full {
        Some(full) => full.as_str(),
        None => item.record["text"].as_str().ok_or_else(unreadable)?,
    };
    let payload: Value = serde_json::from_str(raw).map_err(|_| unreadable())?;
    let output = payload["aggregatedOutput"]
        .as_str()
        .ok_or(CallbackError::Invalid(
            "Saved command output is unavailable. Do not repeat the command.",
        ))?;
    let truncated = studio_runtime_store::truthy(payload.get("outputTruncated"))
        || (item_truncated && full.is_none());
    let success = payload["status"] == "completed" && payload["exitCode"] == 0;
    let mut page = Map::new();
    page.insert("commandStatus".into(), payload["status"].clone());
    page.insert("exitCode".into(), payload["exitCode"].clone());
    excerpt(
        key,
        output,
        args,
        json!({"success":success,"outcome":"unknown","source":"saved_native_output","truncated":truncated}),
        page,
    )
}

fn transcript_error(error: TranscriptStoreError) -> CallbackError {
    match error {
        TranscriptStoreError::Store(error) => CallbackError::Store(error),
        TranscriptStoreError::Sqlite(error) => CallbackError::Store(error.into()),
        TranscriptStoreError::InvalidStoredObject { table, id } => {
            CallbackError::Stored(format!("{table}/{id}"))
        }
        TranscriptStoreError::ActiveTurn => CallbackError::Stored("runtime_items".into()),
    }
}

/// One page of saved text. `page` holds the fields that only a found page has.
fn excerpt(
    key: &str,
    body: &str,
    args: &Value,
    mut value: Value,
    page: Map<String, Value>,
) -> Result<Value, CallbackError> {
    let chars = body.chars().collect::<Vec<_>>();
    let mut offset = match args.get("offset") {
        None => 0,
        Some(v) => v
            .as_u64()
            .filter(|v| *v <= chars.len() as u64)
            .ok_or(CallbackError::Invalid(
                "offset must be within the saved text",
            ))? as usize,
    };
    value["outputRef"] = json!(key);
    if let Some(needle) = args.get("contains").filter(|v| !v.is_null()) {
        let needle = needle
            .as_str()
            .ok_or(CallbackError::Invalid("contains must be text"))?;
        let rest: String = chars[offset..].iter().collect();
        let Some(found) = rest.find(needle) else {
            value["found"] = json!(false);
            return Ok(value);
        };
        offset = offset.max((offset + rest[..found].chars().count()).saturating_sub(200));
    }
    let end = (offset + 30000).min(chars.len());
    value["offset"] = json!(offset);
    value["nextOffset"] = if end < chars.len() {
        json!(end)
    } else {
        Value::Null
    };
    value["totalChars"] = json!(chars.len());
    value["text"] = json!(chars[offset..end].iter().collect::<String>());
    if let Some(fields) = value.as_object_mut() {
        fields.extend(page);
    }
    Ok(value)
}

#[cfg(test)]
mod studio_input_tests {
    use super::*;
    use crate::provider_callbacks::CallbackContext;

    #[test]
    fn spilled_input_is_durable_paged_and_agent_owned() {
        let root = tempfile::tempdir().unwrap();
        let body = format!("{}END", "complaint 🦀\n".repeat(10_000));
        let hash = studio_runtime_store::sha256_hex(body.as_bytes());
        crate::payload_blobs::write_blobs(
            root.path(),
            &[crate::payload_blobs::BlobWrite {
                sha256: hash.clone(),
                bytes: body.as_bytes().to_vec(),
                preview: Value::Null,
            }],
        )
        .unwrap();
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        let tx = connection.transaction().unwrap();
        tx.execute_batch(
            "CREATE TABLE runtime_events(id TEXT,agent TEXT,kind TEXT,text TEXT,created REAL);
            CREATE TABLE runtime_event_meta(id TEXT,record TEXT);",
        )
        .unwrap();
        tx.execute(
            "INSERT INTO runtime_events VALUES('wake','lead','user','continue',1)",
            [],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO runtime_event_meta VALUES('wake',?1)",
            [json!({"studioInput":{"sha256":hash,"size":body.len()}}).to_string()],
        )
        .unwrap();
        let snapshot = |id: &str| {
            CallbackSnapshot::from_record(
                &json!({"id":id,"rootId":id,"epoch":1,"status":"running","autoWake":true,
                "accountKey":"default","threadId":"thread","turnId":"turn","name":id,"cwd":"/work"})
                .to_string(),
                &CallbackContext {
                    agent: id.into(),
                    account: "default".into(),
                    connection: "connection".into(),
                    generation: 1,
                    epoch: 1,
                    thread: "thread".into(),
                    turn: "turn".into(),
                },
            )
            .unwrap()
        };
        let key = format!("studio-input:{hash}");
        let mut recovered = String::new();
        let mut offset = 0;
        loop {
            let page = read(
                &tx,
                &snapshot("lead"),
                &json!({"output_ref":key,"offset":offset}),
                1.0,
                root.path(),
            )
            .unwrap();
            recovered.push_str(page["text"].as_str().unwrap());
            assert_eq!(page["outputRef"], key);
            let Some(next) = page["nextOffset"].as_u64() else {
                break;
            };
            offset = next;
        }
        assert_eq!(recovered, body);
        assert!(
            read(
                &tx,
                &snapshot("other"),
                &json!({"output_ref":key}),
                1.0,
                root.path()
            )
            .is_err()
        );
        let tail = read(
            &tx,
            &snapshot("lead"),
            &json!({"output_ref":key,"contains":"END"}),
            1.0,
            root.path(),
        )
        .unwrap();
        assert!(tail["text"].as_str().unwrap().ends_with("END"));
    }
}
