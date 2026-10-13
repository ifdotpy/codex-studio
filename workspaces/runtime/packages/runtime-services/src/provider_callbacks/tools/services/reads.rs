//! Bounded model projections with the same authority as their full-text reader.
use super::*;
use rusqlite::OptionalExtension;

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
) -> Result<Value, CallbackError> {
    let key = text(args, "output_ref")?;
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
        );
    }
    if key.starts_with("event:") || key.starts_with(&format!("{}:", snapshot.context().agent)) {
        return Err(CallbackError::Dependency(
            "model-output/native-and-event-payload-recovery",
        ));
    }
    // Reuse canonical alias resolution and payload expiry from the receipt service.
    let receipt = tool_requests::execute(
        tx,
        &snapshot.context().agent,
        tool_requests::Action::Get(key),
        now,
    )?;
    if receipt["agent"] != snapshot.context().agent || receipt["stage"] == "ambiguous" {
        return Err(CallbackError::Invalid(
            "Output reference is not owned by this agent",
        ));
    }
    let result = receipt
        .get("result")
        .cloned()
        .or(records::get(
            tx,
            "runtime_tool_results",
            text(&receipt, "id")?,
        )?)
        .ok_or(CallbackError::Invalid(
            "Result is not available yet. Do not repeat the operation.",
        ))?;
    if studio_runtime_store::truthy(result.get("_payloadBlobs")) {
        return Err(CallbackError::Dependency(
            "payload-store/tool-request-recovery",
        ));
    }
    let body = result["contentItems"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["type"] == "inputText")
        .filter_map(|v| v["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    excerpt(
        text(&receipt, "id")?,
        &body,
        args,
        json!({"success":result["success"],"outcome":receipt["outcome"]}),
    )
}

fn excerpt(key: &str, body: &str, args: &Value, mut value: Value) -> Result<Value, CallbackError> {
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
    Ok(value)
}
