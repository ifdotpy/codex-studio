//! Read persisted transcript items without projecting live event state.

use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, ComplaintRecord, PlanRecord, RuntimeStore, StoreError, Table, TaskId,
    TranscriptItem, TranscriptRepository, TranscriptStoreError, WorkItemRecord,
    WorkspaceViewsRepository,
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
    #[error("invalid stored object {table}/{id}")]
    InvalidStoredObject { table: &'static str, id: String },
    #[error("transcript storage failed")]
    Store,
    #[error("this search result is unavailable")]
    SearchResultUnavailable,
    #[error("search query is invalid")]
    InvalidSearch,
    #[error("transcript operation requires {0}")]
    RequiresDependency(&'static str),
}

impl From<TranscriptStoreError> for TranscriptError {
    fn from(error: TranscriptStoreError) -> Self {
        match error {
            TranscriptStoreError::InvalidStoredObject { table, id } => {
                Self::InvalidStoredObject { table, id }
            }
            TranscriptStoreError::ActiveTurn => Self::RequiresDependency("transcript/event-driver"),
            TranscriptStoreError::Store(_) => Self::Store,
            TranscriptStoreError::Sqlite(_) => Self::Store,
        }
    }
}

impl From<StoreError> for TranscriptError {
    fn from(_: StoreError) -> Self {
        Self::Store
    }
}

pub fn pull_scope(
    store: &RuntimeStore,
    scope: &str,
    agent: &str,
    after: i64,
) -> Result<Value, TranscriptError> {
    store.transaction_with(|transaction| {
        TranscriptRepository::pull(transaction, scope, agent, after).map_err(TranscriptError::from)
    })
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
    if let Some(inputs) = stored_inputs(&row.record, &row.id)?.filter(|inputs| !inputs.is_empty()) {
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
        let candidates = if let Some(inputs) = stored_inputs(&row.record, &row.id)? {
            let mut candidates = Vec::with_capacity(inputs.len());
            for (index, input) in inputs.iter().enumerate() {
                let input_id = optional_string(input, "id", "runtime_items", &row.id)?;
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
                .ok_or_else(|| invalid_stored("runtime_items", &row.id))?;
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

pub fn search_item(store: &RuntimeStore, identity: &str) -> Result<Value, TranscriptError> {
    let connection = store.read().map_err(|_| TranscriptError::Store)?;
    if let Some(row) = TranscriptRepository::item_any_agent(&connection, identity)? {
        let actor = TranscriptRepository::agent(&connection, &row.agent)?
            .ok_or(TranscriptError::SearchResultUnavailable)?;
        if actor.get("deletedAt").is_some_and(is_truthy)
            || row.record.get("afterRestore").is_some_and(is_truthy)
        {
            return Err(TranscriptError::SearchResultUnavailable);
        }
        let text = TranscriptRepository::full_text(&connection, &row)?
            .or_else(|| {
                row.record
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .ok_or_else(|| invalid_stored("runtime_items", &row.id))?;
        let mut result = row
            .record
            .as_object()
            .cloned()
            .ok_or_else(|| invalid_stored("runtime_items", &row.id))?;
        result.insert("text".to_owned(), Value::String(text));
        result.insert("agent".to_owned(), Value::String(row.agent));
        result.insert("kind".to_owned(), Value::String("message".to_owned()));
        return Ok(Value::Object(result));
    }

    if let Some((id, room, sender, text, sequence, created, deliveries)) = connection
        .query_row(
            "SELECT id,room,sender,text,seq,created,deliveries FROM runtime_chat_messages WHERE id=?1",
            [identity],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, i64>(4)?, row.get::<_, f64>(5)?, row.get::<_, String>(6)?)),
        )
        .optional()
        .map_err(|_| TranscriptError::Store)?
    {
        let deliveries = serde_json::from_str::<Value>(&deliveries)
            .map_err(|_| invalid_stored("runtime_chat_messages", &id))?;
        return Ok(json!({"id":id,"room":room,"sender":sender,"text":text,
            "seq":sequence,"created":created,"deliveries":deliveries,
            "agent":sender,"kind":"room"}));
    }

    let result = if let Some(row) = Table::<WorkItemRecord>::new(&connection)
        .get(&TaskId::from(identity))
        .map_err(|_| TranscriptError::Store)?
    {
        Some((row.value().clone(), "work"))
    } else if let Some(row) = Table::<PlanRecord>::new(&connection)
        .get(&AgentId::from(identity))
        .map_err(|_| TranscriptError::Store)?
    {
        Some((row.value().clone(), "plan"))
    } else {
        Table::<ComplaintRecord>::new(&connection)
            .get(&String::from(identity))
            .map_err(|_| TranscriptError::Store)?
            .map(|row| (row.value().clone(), "complaint"))
    };
    let Some((mut record, kind)) = result else {
        return Err(TranscriptError::SearchResultUnavailable);
    };
    let owner = record
        .get("rootId")
        .or_else(|| record.get("leadId"))
        .or_else(|| record.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid_stored("runtime_search", identity))?;
    TranscriptRepository::agent(&connection, &owner)?
        .filter(|actor| !actor.get("deletedAt").is_some_and(is_truthy))
        .ok_or(TranscriptError::SearchResultUnavailable)?;
    let text = record
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or(serde_json::to_string_pretty(&record).map_err(|_| TranscriptError::Store)?);
    let object = record
        .as_object_mut()
        .ok_or_else(|| invalid_stored("runtime_search", identity))?;
    object.insert("agent".to_owned(), Value::String(owner));
    object.insert("kind".to_owned(), Value::String(kind.to_owned()));
    object.insert("text".to_owned(), Value::String(text));
    Ok(record)
}

pub fn search_work(
    store: &RuntimeStore,
    query: &str,
    agent_filter: Option<&str>,
    limit: i64,
) -> Result<Value, TranscriptError> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > 500 {
        return Err(TranscriptError::InvalidSearch);
    }
    let limit = limit.clamp(1, 100) as usize;
    let connection = store.read().map_err(|_| TranscriptError::Store)?;
    let agents = Table::<AgentRecord>::new(&connection)
        .list()
        .map_err(|_| TranscriptError::Store)?;
    let caller_root = if let Some(agent) = agent_filter {
        let caller = TranscriptRepository::agent(&connection, agent)?
            .ok_or(TranscriptError::SearchResultUnavailable)?;
        if caller.get("deletedAt").is_some_and(is_truthy) {
            return Err(TranscriptError::DeletedConversation);
        }
        caller
            .get("rootId")
            .and_then(Value::as_str)
            .map(str::to_owned)
    } else {
        None
    };
    let allowed = agents
        .iter()
        .filter(|agent| !agent.value().get("deletedAt").is_some_and(is_truthy))
        .filter(|agent| {
            caller_root
                .as_deref()
                .is_none_or(|root| agent.root_id.0 == root)
        })
        .map(|agent| agent.id().0.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let match_query = query
        .split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ");
    let mut found = Vec::new();
    for (id, agent, kind) in TranscriptRepository::search_hits(&connection, &match_query)? {
        if !allowed.contains(agent.as_str()) || agent_filter.is_some_and(|caller| caller != agent) {
            continue;
        }
        let Some(item) = TranscriptRepository::item(&connection, &agent, &id)? else {
            continue;
        };
        let body = TranscriptRepository::full_text(&connection, &item)?
            .or_else(|| {
                item.record
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        let excerpt = excerpt(&body, query);
        found.push(json!({"id":id,"agent":agent,"type":"message","kind":kind,"reference":id,"excerpt":excerpt,"text":excerpt}));
    }
    let needle = query.to_lowercase();
    for (table, kind, field) in [
        ("runtime_work", "work", "title"),
        ("runtime_complaints", "complaint", "title"),
        ("runtime_plans", "plan", "text"),
    ] {
        for stored in WorkspaceViewsRepository::records(&connection, table)
            .map_err(|_| TranscriptError::Store)?
        {
            let Some(encoded) = stored.record else {
                return Err(invalid_stored(table, &stored.id));
            };
            let record: Value =
                serde_json::from_str(&encoded).map_err(|_| invalid_stored(table, &stored.id))?;
            let root = record
                .get("rootId")
                .or_else(|| record.get("leadId"))
                .or_else(|| record.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_stored(table, &stored.id))?;
            if !allowed.contains(root) {
                continue;
            }
            let serialized = serde_json::to_string(&record).map_err(|_| TranscriptError::Store)?;
            if !serialized.to_lowercase().contains(&needle) {
                continue;
            }
            let excerpt = record
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect::<String>();
            found.push(json!({
                "id": stored.id, "agent": root, "type": kind, "kind": kind,
                "excerpt": excerpt, "reference": stored.id, "text": excerpt
            }));
        }
    }
    found.truncate(limit);
    Ok(json!({"results":found,"query":query,"limit":limit}))
}

fn excerpt(body: &str, query: &str) -> String {
    if body.chars().count() <= 500 {
        return body.to_owned();
    }
    let lower = body.to_lowercase();
    let position = lower.find(&query.to_lowercase()).unwrap_or(0);
    let start = body[..position.min(body.len())]
        .char_indices()
        .rev()
        .nth(250)
        .map_or(0, |(index, _)| index);
    let end = body[start..]
        .char_indices()
        .nth(500)
        .map_or(body.len(), |(offset, _)| start + offset);
    let mut result = String::new();
    if start > 0 {
        result.push_str(" … ");
    }
    result.push_str(&body[start..end]);
    if end < body.len() {
        result.push_str(" … ");
    }
    result
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
    optional_string(&row.record, "text", "runtime_items", &row.id)?;
    let input_is_user = input
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        == Some("user");
    let parent_role = optional_string(&row.record, "role", "runtime_items", &row.id)?;
    let role = if input_is_user {
        Value::String("user".to_owned())
    } else {
        Value::String(
            parent_role
                .ok_or_else(|| invalid_stored("runtime_items", &row.id))?
                .to_owned(),
        )
    };
    let mut projected = row
        .record
        .as_object()
        .cloned()
        .ok_or_else(|| invalid_stored("runtime_items", &row.id))?;
    if let Some(input) = input {
        let fields = input
            .as_object()
            .ok_or_else(|| invalid_stored("runtime_items", &row.id))?;
        optional_string(input, "text", "runtime_items", &row.id)?;
        optional_string(input, "kind", "runtime_items", &row.id)?;
        optional_string(input, "turnId", "runtime_items", &row.id)?;
        optional_string(input, "clientMessageId", "runtime_items", &row.id)?;
        projected.extend(fields.clone());
    }
    optional_string(&row.record, "clientMessageId", "runtime_items", &row.id)?;
    let turn_id = input
        .and_then(|value| value.get("turnId"))
        .or_else(|| row.record.get("turnId"));
    if let Some(turn_id) = turn_id
        && !turn_id.is_null()
        && !turn_id.is_string()
    {
        return Err(invalid_stored("runtime_items", &row.id));
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
    let truncated = validate_input(input, &row.id)?;
    let input_id = optional_string(input, "id", "runtime_items", &row.id)?;
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
        .or(optional_string(input, "text", "runtime_items", &row.id)?.map(str::to_owned))
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
        && let Some(inputs) = stored_inputs(&row.record, &row.id)?
    {
        let mut pieces = Vec::new();
        for input in inputs {
            validate_input(input, &row.id)?;
            let input_id = optional_string(input, "id", "runtime_items", &row.id)?;
            let event = input_id.map(&mut event_lookup).transpose()?.flatten();
            if let Some(event) = event {
                pieces.push(event);
            } else if input_id.is_none()
                && !optional_bool(input, "truncated", "runtime_items", &row.id)?.unwrap_or(false)
                && let Some(text) = optional_string(input, "text", "runtime_items", &row.id)?
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
        .or(optional_string(&row.record, "text", "runtime_items", &row.id)?.map(str::to_owned))
        .unwrap_or_default();
    project(row, None, &row.id, &row.id, agent, text, complete)
}

fn validate_parent(row: &TranscriptItem) -> Result<bool, TranscriptError> {
    match row.record.get("id") {
        Some(Value::String(id)) if id == &row.id => {}
        _ => return Err(invalid_stored("runtime_items", &row.id)),
    }
    optional_bool(&row.record, "truncated", "runtime_items", &row.id)
        .map(|value| value.unwrap_or(false))
}

fn optional_string<'a>(
    object: &'a Value,
    field: &str,
    stored_object: &'static str,
    id: &str,
) -> Result<Option<&'a str>, TranscriptError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(invalid_stored(stored_object, id)),
    }
}

fn stored_inputs<'a>(record: &'a Value, id: &str) -> Result<Option<&'a [Value]>, TranscriptError> {
    match record.get("inputs") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(inputs)) => Ok(Some(inputs)),
        _ => Err(invalid_stored("runtime_items", id)),
    }
}

fn validate_input(input: &Value, id: &str) -> Result<bool, TranscriptError> {
    if !input.is_object() {
        return Err(invalid_stored("runtime_items", id));
    }
    optional_string(input, "kind", "runtime_items", id)?;
    optional_string(input, "text", "runtime_items", id)?;
    optional_string(input, "id", "runtime_items", id)?;
    optional_string(input, "turnId", "runtime_items", id)?;
    optional_string(input, "clientMessageId", "runtime_items", id)?;
    Ok(optional_bool(input, "truncated", "runtime_items", id)?.unwrap_or(false))
}

fn optional_bool(
    object: &Value,
    field: &str,
    stored_object: &'static str,
    id: &str,
) -> Result<Option<bool>, TranscriptError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(invalid_stored(stored_object, id)),
    }
}

fn invalid_stored(table: &'static str, id: &str) -> TranscriptError {
    TranscriptError::InvalidStoredObject {
        table,
        id: id.to_owned(),
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

#[cfg(test)]
mod tests {
    use super::{hydrate_input, parent_projection};
    use serde_json::{Value, json};
    use studio_runtime_store::{TranscriptItem, TranscriptItemRecord};

    #[test]
    fn legacy_parent_text_is_hydrated_before_projection() {
        for text in [None, Some(Value::Null)] {
            let mut stored = json!({"id":"item-1","role":"assistant","truncated":true});
            if let Some(text) = text {
                stored["text"] = text;
            }
            let record = TranscriptItemRecord::from_value(stored)
                .expect("persisted transcript permits absent or null parent text");
            let row = TranscriptItem {
                id: record.id().as_ref().to_owned(),
                agent: "agent-1".to_owned(),
                record: record.value().clone(),
            };

            let projected = parent_projection(
                &row,
                "agent-1",
                Some("hydrated from fulltext".to_owned()),
                |_| Ok(None),
            )
            .expect("legacy content is hydrated before response projection");
            assert_eq!(projected["text"], "hydrated from fulltext");
            assert_eq!(projected["truncated"], false);
        }
    }

    #[test]
    fn legacy_transcript_inputs_allow_missing_and_null_optional_fields() {
        for input in [
            json!({"id":"input-1"}),
            json!({"id":"input-1","kind":null,"text":null,"truncated":null}),
        ] {
            let record = TranscriptItemRecord::from_value(json!({
                "id":"item-1", "role":"assistant", "text":null,
                "inputs":[input]
            }))
            .expect("persisted transcript permits absent or null optional input fields");
            let row = TranscriptItem {
                id: record.id().as_ref().to_owned(),
                agent: "agent-1".to_owned(),
                record: record.value().clone(),
            };
            let projected = hydrate_input(
                &row,
                &row.record["inputs"][0],
                0,
                "agent-1",
                Some("agent-1:input-1"),
                |_| Ok(Some("hydrated input text".to_owned())),
            )
            .expect("legacy input fields are hydrated")
            .expect("requested input is present");
            assert_eq!(projected["text"], "hydrated input text");
            assert_eq!(projected["role"], "assistant");
        }
    }
}
