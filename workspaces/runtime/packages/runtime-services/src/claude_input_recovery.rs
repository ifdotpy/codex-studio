//! Frozen Claude input: the exact input and configuration of a Claude turn start,
//! saved beside its Submitted receipt. This module has no I/O; `runtime` applies it
//! inside the account worker's transaction. A positively rejected start is never
//! retried here; legacy retry records stay held (see `runtime::freeze_start`).
pub mod runtime;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use studio_runtime_store::sha256_hex;

use studio_runtime_store::MAX_EVENTS_PER_TURN as MAX_BATCH;
const RECEIPT_FIELDS: &[&str] = &[
    "id",
    "epoch",
    "accountKey",
    "threadId",
    "connectionId",
    "nativeOperationId",
    "events",
    "submitted",
    "supervisorIdentity",
    "claudeInputRequest",
];
const SOURCE_FIELDS: &[&str] = &[
    "cwd",
    "rootId",
    "parentId",
    "role",
    "isLead",
    "model",
    "effort",
    "nativeEffort",
    "fastMode",
    "yoloMode",
    "imageWorkspace",
    "imageWorkspaceReady",
    "compactions",
];

#[derive(Debug, thiserror::Error)]
#[error("Invalid Claude input record at {location}")]
pub struct InvalidRecord {
    pub location: String,
}

#[derive(Deserialize)]
struct Agent {
    id: String,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum EventStatus {
    Pending,
    Reserved,
    Dispatching,
    Uncertain,
    Delivered,
    Cancelled,
    Failed,
}

/// SQL columns and the separate metadata row; no provider-derived booleans here.
#[derive(Clone, Deserialize, Serialize)]
pub struct EventRecord {
    id: String,
    agent: String,
    epoch: i64,
    kind: String,
    text: String,
    created: f64,
    status: EventStatus,
    turn_id: Option<String>,
    error: Option<Value>,
    metadata: Map<String, Value>,
}
impl EventRecord {
    fn snapshot(&self) -> Value {
        json!({"id":self.id,"kind":self.kind,"created":self.created,
               "assets":self.metadata.get("assets").cloned().unwrap_or(json!([])),
               "textHash":sha256_hex(self.text.as_bytes())})
    }
}

/// Captured by the account owner, never inferred from a provider response.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportSnapshot {
    account_key: String,
    connection_id: String,
    supervisor_identity: Option<Value>,
    closed: bool,
}

fn invalid(location: &'static str) -> InvalidRecord {
    InvalidRecord {
        location: location.into(),
    }
}
fn decode<T: serde::de::DeserializeOwned>(
    value: &Value,
    location: &'static str,
) -> Result<T, InvalidRecord> {
    serde_json::from_value(value.clone()).map_err(|_| invalid(location))
}
fn source(agent: &Map<String, Value>) -> Map<String, Value> {
    SOURCE_FIELDS
        .iter()
        .map(|key| {
            (
                (*key).into(),
                agent.get(*key).cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
}

/// Capture before Submitted is committed. Configuration and input are retained
/// exactly; dynamic tools are refreshed independently at the next admission.
pub fn capture_input(
    agent: &Value,
    events: &[Value],
    params: &Value,
    text: &str,
) -> Result<Value, InvalidRecord> {
    let actor: Agent = decode(agent, "agent")?;
    let record: Map<String, Value> = decode(agent, "agent")?;
    let events: Vec<EventRecord> = events
        .iter()
        .map(|event| decode(event, "events"))
        .collect::<Result<_, _>>()?;
    let first = events.first().ok_or_else(|| invalid("events"))?;
    if events.len() > MAX_BATCH {
        return Err(invalid("events"));
    }
    let mut configuration: Map<String, Value> = decode(params, "params")?;
    let input: Vec<Value> = decode(
        &configuration
            .remove("input")
            .ok_or_else(|| invalid("params/input"))?,
        "params/input",
    )?;
    let client = configuration
        .remove("clientUserMessageId")
        .ok_or_else(|| invalid("params/clientUserMessageId"))?;
    configuration.remove("dynamicTools");
    Ok(
        json!({"input":input,"source":source(&record),"clientUserMessageId":client,
        "configuration":configuration,"events":events.iter().map(EventRecord::snapshot).collect::<Vec<_>>(),
        "transcriptItemId":format!("{}:{}",actor.id,first.id),"textHash":sha256_hex(text.as_bytes())}),
    )
}
