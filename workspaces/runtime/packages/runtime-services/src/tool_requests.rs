//! Actor-scoped reads and local cancellation for durable tool receipts.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, Transaction, params};
use serde::Serialize;
use serde_json::{Value, json};
use studio_runtime_store::{Connection, StoreError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolRequestError {
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("Not implemented: requires {0}")]
    RequiresDependency(&'static str),
    #[error("invalid stored object {object}/{id}")]
    InvalidStored { object: &'static str, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<rusqlite::Error> for ToolRequestError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(StoreError::from(error))
    }
}

impl From<serde_json::Error> for ToolRequestError {
    fn from(error: serde_json::Error) -> Self {
        Self::Store(StoreError::from(error))
    }
}

pub fn read(
    connection: &Connection,
    actor_id: &str,
    request_id: Option<&str>,
) -> Result<Value, ToolRequestError> {
    checked_actor(connection, actor_id)?;
    if let Some(id) = request_id {
        validate_request_id(id)?;
        let Some((_id, record)) = resolve(connection, actor_id, id)? else {
            return Ok(missing(id));
        };
        if pending_payload(connection, id, &record)? {
            return Err(ToolRequestError::RequiresDependency(
                "payload-store/tool-request-list",
            ));
        }
        let mut result = record;
        if let Some(object) = result.as_object_mut() {
            object.remove("signature");
            object.remove("_payloadBlobs");
        }
        return Ok(result);
    }
    let mut statement = connection.prepare(
        "SELECT id,record FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=?1 ORDER BY json_extract(record,'$.updated') DESC LIMIT 50",
    )?;
    let rows = statement.query_map([actor_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut requests = Vec::new();
    for row in rows {
        let (id, raw) = row?;
        let mut record = parse_record(&raw, &id)?;
        if pending_payload(connection, id.as_str(), &record)? {
            return Err(ToolRequestError::RequiresDependency(
                "payload-store/tool-request-list",
            ));
        }
        if let Some(object) = record.as_object_mut() {
            object.remove("result");
            object.remove("signature");
            object.remove("_payloadBlobs");
        }
        requests.push(record);
    }
    Ok(json!({"requests":requests}))
}

pub fn cancel(
    transaction: &Transaction<'_>,
    actor_id: &str,
    request_id: &str,
) -> Result<Value, ToolRequestError> {
    checked_actor(transaction, actor_id)?;
    validate_request_id(request_id)?;
    let Some((canonical_id, mut record)) = resolve(transaction, actor_id, request_id)? else {
        return Ok(missing(request_id));
    };
    if record.get("stage").and_then(Value::as_str) == Some("ambiguous") {
        return Ok(record);
    }
    if pending_payload(transaction, &canonical_id, &record)? {
        return Err(ToolRequestError::RequiresDependency(
            "payload-store/tool-request-list",
        ));
    }
    match record.get("stage").and_then(Value::as_str) {
        Some("queued") => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ToolRequestError::InvalidRequest("The system clock is unavailable"))?
                .as_secs_f64();
            record["stage"] = Value::String("cancelled".to_owned());
            record["outcome"] = Value::String("not_applied".to_owned());
            record["cancelRequested"] = Value::Bool(true);
            record["updated"] = json!(now);
            record["finished"] = json!(now);
            record["result"] = cancel_result(&canonical_id);
            let serialized =
                serde_json::to_string(&record).map_err(|_| ToolRequestError::InvalidStored {
                    object: "runtime_tool_requests",
                    id: canonical_id.clone(),
                })?;
            transaction.execute(
                "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
                params![serialized, canonical_id],
            )?;
        }
        Some("running") => {
            return Err(ToolRequestError::RequiresDependency(
                "tool-driver/request-lifecycle",
            ));
        }
        Some("cancelled" | "completed" | "interrupted" | "failed" | "unknown") => {}
        _ => {
            return Err(ToolRequestError::InvalidStored {
                object: "runtime_tool_requests",
                id: canonical_id,
            });
        }
    }
    let mut response = record;
    if let Some(object) = response.as_object_mut() {
        object.remove("signature");
        object.remove("_payloadBlobs");
    }
    Ok(response)
}

fn checked_actor(
    connection: &rusqlite::Connection,
    actor_id: &str,
) -> Result<Value, ToolRequestError> {
    let raw = connection
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [actor_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(ToolRequestError::InvalidRequest("Invalid request"))?;
    let actor: Value = serde_json::from_str(&raw).map_err(|_| ToolRequestError::InvalidStored {
        object: "runtime_agents",
        id: actor_id.to_owned(),
    })?;
    if truthy(actor.get("deletedAt")) {
        return Err(ToolRequestError::InvalidRequest("This agent was deleted"));
    }
    Ok(actor)
}

fn resolve(
    connection: &rusqlite::Connection,
    actor: &str,
    id: &str,
) -> Result<Option<(String, Value)>, ToolRequestError> {
    let direct = connection
        .query_row(
            "SELECT record FROM runtime_tool_requests WHERE id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(raw) = direct {
        let record = parse_record(&raw, id)?;
        if record.get("agent").and_then(Value::as_str) == Some(actor) {
            return Ok(Some((id.to_owned(), record)));
        }
    }
    let alias_table:bool=connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_tool_request_aliases')",[],|row|row.get(0))?;
    if !alias_table {
        return Ok(None);
    }
    let mut statement = connection.prepare(
        "SELECT request FROM runtime_tool_request_aliases WHERE agent=?1 AND alias=?2 LIMIT 2",
    )?;
    let aliases = statement
        .query_map(params![actor, id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if aliases.len() > 1 {
        return Ok(Some((
            id.to_owned(),
            json!({"id":id,"stage":"ambiguous","outcome":"unknown",
            "message":"Use the canonical request id from list; this short id names several requests."}),
        )));
    }
    let Some(canonical) = aliases.first() else {
        return Ok(None);
    };
    let raw = connection
        .query_row(
            "SELECT record FROM runtime_tool_requests WHERE id=?1",
            [canonical],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    raw.map(|raw| parse_record(&raw, canonical).map(|record| (canonical.clone(), record)))
        .transpose()
}

fn parse_record(raw: &str, id: &str) -> Result<Value, ToolRequestError> {
    let record: Value = serde_json::from_str(raw).map_err(|_| ToolRequestError::InvalidStored {
        object: "runtime_tool_requests",
        id: id.to_owned(),
    })?;
    if !record.is_object() || record.get("id").and_then(Value::as_str) != Some(id) {
        return Err(ToolRequestError::InvalidStored {
            object: "runtime_tool_requests",
            id: id.to_owned(),
        });
    }
    Ok(record)
}

fn pending_payload(
    connection: &rusqlite::Connection,
    id: &str,
    record: &Value,
) -> Result<bool, ToolRequestError> {
    if matches!(
        record.get("outcome").and_then(Value::as_str),
        Some("applied" | "not_applied")
    ) {
        return Ok(false);
    }
    if has_payload_refs(record.get("_payloadBlobs")) {
        return Ok(true);
    }
    let has_results: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_tool_results')",
        [],
        |row| row.get(0),
    )?;
    if !has_results {
        return Ok(false);
    }
    let result = connection
        .query_row(
            "SELECT result FROM runtime_tool_results WHERE id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    result
        .map(|raw| {
            serde_json::from_str::<Value>(&raw)
                .map(|value| has_payload_refs(value.get("_payloadBlobs")))
                .map_err(|_| ToolRequestError::InvalidStored {
                    object: "runtime_tool_results",
                    id: id.to_owned(),
                })
        })
        .transpose()
        .map(|value| value.unwrap_or(false))
}

fn has_payload_refs(value: Option<&Value>) -> bool {
    value.is_some_and(|value| {
        !value.is_null() && !value.as_object().is_some_and(|items| items.is_empty())
    })
}

fn validate_request_id(id: &str) -> Result<(), ToolRequestError> {
    if id.is_empty() || id.chars().count() > 1000 {
        return Err(ToolRequestError::InvalidRequest("Supply request_id"));
    }
    Ok(())
}

fn missing(id: &str) -> Value {
    json!({"id":id,"stage":"not_found","outcome":"unknown",
        "message":"No receipt found. This does not prove that the operation did not execute."})
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CancelEvidence<'a> {
    request_id: &'a str,
    stage: &'static str,
    outcome: &'static str,
    cancel_requested: bool,
    message: &'static str,
}

fn cancel_result(id: &str) -> Value {
    let text = serde_json::to_string(&CancelEvidence {
        request_id: id,
        stage: "cancelled",
        outcome: "not_applied",
        cancel_requested: true,
        message: "Cancelled before execution",
    })
    .unwrap_or_default();
    json!({"success":false,"contentItems":[{"type":"inputText","text":text}]})
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}
