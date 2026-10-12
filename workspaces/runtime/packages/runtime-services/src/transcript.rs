//! Read persisted transcript items without projecting live event state.

use serde_json::{Value, json};
use studio_runtime_store::{
    RuntimeStore, TranscriptItem, TranscriptRepository, TranscriptStoreError,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TranscriptError {
    #[error("unknown message in this conversation")]
    UnknownMessage,
    #[error("this conversation was deleted")]
    DeletedConversation,
    #[error("full transcript content is unavailable")]
    MissingContent,
    #[error("invalid stored object {0}")]
    InvalidStoredObject(&'static str),
    #[error("transcript storage failed")]
    Store,
}

impl From<TranscriptStoreError> for TranscriptError {
    fn from(error: TranscriptStoreError) -> Self {
        match error {
            TranscriptStoreError::InvalidStoredObject(object) => Self::InvalidStoredObject(object),
            TranscriptStoreError::Sqlite(_) => Self::Store,
        }
    }
}

pub fn item(store: &RuntimeStore, agent: &str, identity: &str) -> Result<Value, TranscriptError> {
    let connection = store.read().map_err(|_| TranscriptError::Store)?;
    let actor =
        TranscriptRepository::agent(&connection, agent)?.ok_or(TranscriptError::UnknownMessage)?;
    if actor.get("deletedAt").is_some_and(is_truthy) {
        return Err(TranscriptError::DeletedConversation);
    }
    let row = TranscriptRepository::resolve_item(&connection, agent, identity)?
        .ok_or(TranscriptError::UnknownMessage)?;
    validate_parent(&row)?;
    if let Some(inputs) = stored_inputs(&row.record)?.filter(|inputs| !inputs.is_empty()) {
        for (index, input) in inputs.iter().enumerate() {
            let candidate = hydrate_input(&row, input, index, agent, Some(identity), |id| {
                Ok(TranscriptRepository::event_text(&connection, agent, id)?)
            })?;
            if let Some(candidate) = candidate {
                return Ok(candidate);
            }
        }
    }
    let full = TranscriptRepository::full_text(&connection, &row)?;
    parent_projection(&row, agent, full, |id| {
        Ok(TranscriptRepository::event_text(&connection, agent, id)?)
    })
}

pub fn search(
    store: &RuntimeStore,
    agent: &str,
    query: &str,
    limit: i64,
) -> Result<Value, TranscriptError> {
    let query = query.trim();
    let limit = limit.clamp(1, 500) as usize;
    let connection = store.read().map_err(|_| TranscriptError::Store)?;
    let actor =
        TranscriptRepository::agent(&connection, agent)?.ok_or(TranscriptError::UnknownMessage)?;
    if actor.get("deletedAt").is_some_and(is_truthy) {
        return Err(TranscriptError::DeletedConversation);
    }
    if query.is_empty() {
        return Ok(json!({"results": [], "truncated": false}));
    }
    let mut results = Vec::new();
    for row in TranscriptRepository::items_for_agent(&connection, agent)? {
        let candidates = if let Some(inputs) = stored_inputs(&row.record)? {
            let mut candidates = Vec::with_capacity(inputs.len());
            for (index, input) in inputs.iter().enumerate() {
                let input_id = optional_string(input, "id", "runtime_items")?;
                let Some(mut candidate) = hydrate_input(&row, input, index, agent, None, |id| {
                    Ok(TranscriptRepository::event_text(&connection, agent, id)?)
                })?
                else {
                    continue;
                };
                candidate["clientMessageId"] =
                    input_id.map_or(Value::Null, |id| Value::String(id.to_owned()));
                candidates.push(candidate);
            }
            candidates
        } else {
            let full = TranscriptRepository::full_text(&connection, &row)?;
            vec![parent_projection(&row, agent, full, |id| {
                Ok(TranscriptRepository::event_text(&connection, agent, id)?)
            })?]
        };
        for candidate in candidates {
            let text = candidate["text"]
                .as_str()
                .ok_or(TranscriptError::InvalidStoredObject("runtime_items"))?;
            let Some(position) = find_lowercase(text, query) else {
                continue;
            };
            let characters = text.chars().collect::<Vec<_>>();
            let start = position.saturating_sub(80).min(characters.len());
            let end = position
                .saturating_add(query.chars().count())
                .saturating_add(160)
                .min(characters.len());
            let excerpt = characters[start.min(end)..end].iter().collect::<String>();
            results.push(json!({
                "id": candidate["id"],
                "sourceId": candidate["sourceId"],
                "clientMessageId": candidate["clientMessageId"],
                "role": candidate["role"],
                "turnId": candidate["turnId"],
                "text": candidate["text"],
                "agent": agent,
                "excerpt": excerpt,
            }));
            if results.len() > limit {
                results.truncate(limit);
                return Ok(json!({"results": results, "truncated": true}));
            }
        }
    }
    Ok(json!({"results": results, "truncated": false}))
}

fn project(
    row: &TranscriptItem,
    input: Option<&Value>,
    source_id: &str,
    identity: &str,
    agent: &str,
    text: String,
    complete: bool,
) -> Result<Value, TranscriptError> {
    validate_parent(row)?;
    optional_string(&row.record, "text", "runtime_items")?;
    let input_is_user = input
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        == Some("user");
    let parent_role = optional_string(&row.record, "role", "runtime_items")?;
    let role = if input_is_user {
        Value::String("user".to_owned())
    } else {
        Value::String(
            parent_role
                .ok_or(TranscriptError::InvalidStoredObject("runtime_items"))?
                .to_owned(),
        )
    };
    let mut projected = row
        .record
        .as_object()
        .cloned()
        .ok_or(TranscriptError::InvalidStoredObject("runtime_items"))?;
    if let Some(input) = input {
        let fields = input
            .as_object()
            .ok_or(TranscriptError::InvalidStoredObject("runtime_items"))?;
        optional_string(input, "text", "runtime_items")?;
        optional_string(input, "kind", "runtime_items")?;
        optional_string(input, "turnId", "runtime_items")?;
        optional_string(input, "clientMessageId", "runtime_items")?;
        projected.extend(fields.clone());
    }
    optional_string(&row.record, "clientMessageId", "runtime_items")?;
    let turn_id = input
        .and_then(|value| value.get("turnId"))
        .or_else(|| row.record.get("turnId"));
    if let Some(turn_id) = turn_id
        && !turn_id.is_null()
        && !turn_id.is_string()
    {
        return Err(TranscriptError::InvalidStoredObject("runtime_items"));
    }
    projected.insert("id".to_owned(), Value::String(identity.to_owned()));
    projected.insert("sourceId".to_owned(), Value::String(source_id.to_owned()));
    projected.insert("agent".to_owned(), Value::String(agent.to_owned()));
    projected.insert("role".to_owned(), role);
    projected.insert("text".to_owned(), Value::String(text));
    projected.insert("truncated".to_owned(), Value::Bool(!complete));
    Ok(Value::Object(projected))
}

fn hydrate_input<F>(
    row: &TranscriptItem,
    input: &Value,
    index: usize,
    agent: &str,
    requested_identity: Option<&str>,
    event_lookup: F,
) -> Result<Option<Value>, TranscriptError>
where
    F: FnOnce(&str) -> Result<Option<String>, TranscriptError>,
{
    let truncated = validate_input(input)?;
    let input_id = optional_string(input, "id", "runtime_items")?;
    let identity = input_id.map_or_else(
        || format!("{}:{index}", row.id),
        |id| format!("{agent}:{id}"),
    );
    if requested_identity.is_some_and(|requested| requested != identity) {
        return Ok(None);
    }
    let event = input_id.map(event_lookup).transpose()?.flatten();
    if truncated && event.is_none() {
        return Err(TranscriptError::MissingContent);
    }
    let complete = !truncated || event.is_some();
    let text = event
        .or(optional_string(input, "text", "runtime_items")?.map(str::to_owned))
        .unwrap_or_default();
    project(row, Some(input), &row.id, &identity, agent, text, complete).map(Some)
}

fn parent_projection<F>(
    row: &TranscriptItem,
    agent: &str,
    mut full: Option<String>,
    mut event_lookup: F,
) -> Result<Value, TranscriptError>
where
    F: FnMut(&str) -> Result<Option<String>, TranscriptError>,
{
    let truncated = validate_parent(row)?;
    if full.is_none()
        && let Some(inputs) = stored_inputs(&row.record)?
    {
        let mut pieces = Vec::new();
        for input in inputs {
            validate_input(input)?;
            let input_id = optional_string(input, "id", "runtime_items")?;
            let event = input_id.map(&mut event_lookup).transpose()?.flatten();
            if let Some(event) = event {
                pieces.push(event);
            } else if input_id.is_none()
                && !optional_bool(input, "truncated", "runtime_items")?.unwrap_or(false)
                && let Some(text) = optional_string(input, "text", "runtime_items")?
            {
                pieces.push(text.to_owned());
            }
        }
        if !pieces.is_empty() {
            full = Some(pieces.join("\n\n"));
        }
    }
    if truncated && full.as_deref().is_none_or(str::is_empty) {
        return Err(TranscriptError::MissingContent);
    }
    let complete = !truncated || full.is_some();
    let text = full
        .or(optional_string(&row.record, "text", "runtime_items")?.map(str::to_owned))
        .unwrap_or_default();
    project(row, None, &row.id, &row.id, agent, text, complete)
}

fn validate_parent(row: &TranscriptItem) -> Result<bool, TranscriptError> {
    match row.record.get("id") {
        Some(Value::String(id)) if id == &row.id => {}
        _ => return Err(TranscriptError::InvalidStoredObject("runtime_items")),
    }
    optional_bool(&row.record, "truncated", "runtime_items").map(|value| value.unwrap_or(false))
}

fn optional_string<'a>(
    object: &'a Value,
    field: &str,
    stored_object: &'static str,
) -> Result<Option<&'a str>, TranscriptError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(TranscriptError::InvalidStoredObject(stored_object)),
    }
}

fn stored_inputs(record: &Value) -> Result<Option<&[Value]>, TranscriptError> {
    match record.get("inputs") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(inputs)) => Ok(Some(inputs)),
        _ => Err(TranscriptError::InvalidStoredObject("runtime_items")),
    }
}

fn validate_input(input: &Value) -> Result<bool, TranscriptError> {
    if !input.is_object()
        || optional_string(input, "kind", "runtime_items")?.is_none()
        || optional_string(input, "text", "runtime_items")?.is_none()
    {
        return Err(TranscriptError::InvalidStoredObject("runtime_items"));
    }
    optional_string(input, "id", "runtime_items")?;
    optional_string(input, "turnId", "runtime_items")?;
    optional_string(input, "clientMessageId", "runtime_items")?;
    match input.get("truncated") {
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(TranscriptError::InvalidStoredObject("runtime_items")),
    }
}

fn optional_bool(
    object: &Value,
    field: &str,
    stored_object: &'static str,
) -> Result<Option<bool>, TranscriptError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(TranscriptError::InvalidStoredObject(stored_object)),
    }
}

fn find_lowercase(text: &str, query: &str) -> Option<usize> {
    let lowered_text = text.to_lowercase();
    let lowered_query = query.to_lowercase();
    let start = lowered_text.find(&lowered_query)?;
    Some(lowered_text[..start].chars().count())
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}
