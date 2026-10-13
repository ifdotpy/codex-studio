//! Persisted agent rename behavior.

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, EventWriteRepository, StoreError, Transaction,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RenameError {
    #[error("Unknown managed agent")]
    UnknownAgent,
    #[error("This conversation was deleted")]
    Deleted,
    #[error("This request id has different content")]
    Conflict,
    #[error("A rename request id is required")]
    RequestIdRequired,
    #[error("Not implemented: requires provider-session/title-generation")]
    TitleGenerationUnavailable,
    #[error("invalid stored object {object}")]
    InvalidStoredObject { object: &'static str },
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn rename(
    transaction: &Transaction<'_>,
    id: &str,
    name: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
) -> Result<Value, RenameError> {
    if let Some(request_id) = request_id
        && let Some(receipt) = EventWriteRepository::receipt(transaction, request_id)?
    {
        if receipt.signature != signature {
            return Err(RenameError::Conflict);
        }
        let result: Value = serde_json::from_str(&receipt.result).map_err(|_| {
            RenameError::InvalidStoredObject {
                object: "runtime_operation_receipts",
            }
        })?;
        return Ok(result);
    }

    let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(id))? else {
        return Err(RenameError::UnknownAgent);
    };
    if agent.id.0 != id {
        return Err(RenameError::InvalidStoredObject {
            object: "runtime_agents",
        });
    }
    if truthy(agent.extra.get("deletedAt")) {
        return Err(RenameError::Deleted);
    }
    let Some(name) = name else {
        return Err(if request_id.is_some() {
            RenameError::TitleGenerationUnavailable
        } else {
            RenameError::RequestIdRequired
        });
    };

    agent
        .extra
        .insert("name".to_owned(), Value::String(name.to_owned()));
    agent
        .extra
        .insert("manualName".to_owned(), Value::Bool(true));
    agent
        .extra
        .insert("needsTitle".to_owned(), Value::Bool(false));
    AgentRepository::put(transaction, &agent)?;
    EventWriteRepository::set_agent_sync_name(transaction, id, name)?;

    let mut result = json!({"id": id, "name": name});
    if let Some(request_id) = request_id {
        result["request_id"] = Value::String(request_id.to_owned());
        result["status"] = Value::String("applied".to_owned());
        let result_text = serde_json::to_string(&result).map_err(StoreError::from)?;
        EventWriteRepository::put_receipt(transaction, request_id, signature, &result_text)?;
    }
    Ok(result)
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}
