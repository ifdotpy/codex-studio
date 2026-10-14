//! Exact native-history reconciliation. Missing history never authorizes replay.
//!
//! Capture under the store lock, read the provider outside it, then compare the
//! complete capture inside the write transaction. The account worker supplies a
//! cached connection identity and the shared delivered-manifest writer.
use std::collections::{BTreeMap, BTreeSet};

use crate::account_session_runtime::{
    AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest, ProviderResponse,
    RequestCertainty,
};
use crate::historical_input_receipts::HistoricalBatch;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use studio_runtime_store::{
    AgentRecord, ContextRepairRepository, RuntimeStore, StoreError, Transaction,
};
use thiserror::Error;

pub const MAX_INPUTS: usize = 32;
pub const MAX_TURNS: usize = 1600;
pub const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
const WAIT_PREFIX: &str = "Context repair waits for a confirmed input receipt: ";

#[derive(Debug, Error)]
pub enum RepairError {
    #[error("invalid context recovery object at {0}")]
    Invalid(&'static str),
    #[error("Unknown agent")]
    MissingAgent,
    #[error("Context repair awaits its exact native receipt")]
    ReceiptHold,
    #[error("Not implemented: requires context-repair/native-history-rewrite")]
    RewriteUnavailable,
    #[error("Not implemented: requires turn-prepare/delivered-manifest-writer")]
    ManifestWriterUnavailable,
    #[error("Not implemented: requires store-bootstrap/context-repair-schema")]
    MissingSchema,
    #[error("Context repair provider request outcome is unknown")]
    ProviderUnknown,
    #[error("Context repair provider request was not accepted")]
    ProviderRejected,
    #[error("Context repair history file operation failed")]
    HistoryFile(#[source] Box<crate::context_repair_files::HistoryFileError>),
    #[error("Context repair account session unavailable")]
    AccountSession,
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn terminal_receipt_key(account: &str, thread: &str, turn: &str) -> String {
    studio_runtime_store::python_json_compact(&json!([account, thread, turn]))
}

fn register_source_terminal_receipt(
    tx: &Transaction<'_>,
    repair: &Value,
    new_thread: &str,
) -> Result<(), RepairError> {
    let proof = repair
        .get("nativeTerminalReceipt")
        .filter(|value| value.is_object());
    let Some(proof) = proof else {
        return Ok(());
    };
    let source = repair
        .get("source")
        .filter(|value| value.is_object())
        .ok_or(RepairError::ReceiptHold)?;
    let account = source["accountKey"]
        .as_str()
        .ok_or(RepairError::ReceiptHold)?;
    let thread = source["threadId"]
        .as_str()
        .ok_or(RepairError::ReceiptHold)?;
    let Some(turn) = proof["turnId"].as_str() else {
        // An inherited-empty repair has no source native turn callback to
        // reconcile; its terminal proof belongs entirely to the JSONL ancestry.
        return Ok(());
    };
    let record = json!({
        "operationId":repair["id"],
        "source":source,
        "proof":proof,
        "startAttemptSnapshot":repair["startAttemptSnapshot"],
        "connectionId":repair["connectionId"],
        "connectionGeneration":repair["connectionGeneration"],
        "newThreadId":new_thread,
        "phase":"completed"
    });
    tx.execute(
        "INSERT INTO runtime_context_terminal_receipts(id,agent,operation,record) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,operation=excluded.operation,record=excluded.record WHERE json_extract(runtime_context_terminal_receipts.record,'$.phase')!='completed'",
        rusqlite::params![terminal_receipt_key(account, thread, turn), repair["agent"].as_str().ok_or(RepairError::ReceiptHold)?, repair["id"].as_str().ok_or(RepairError::ReceiptHold)?, record.to_string()],
    )?;
    Ok(())
}

/// Exact late terminal callback lookup used by the retained P04 journal path.
/// Only a completed, persisted context-repair fork can authorize an old source
/// thread callback to settle the original scheduler attempt.
pub(crate) fn source_terminal_attempt(
    tx: &Transaction<'_>,
    account: &str,
    generation: u64,
    params: &Value,
) -> Result<Option<studio_runtime_store::TurnAttemptIdentity>, RepairError> {
    let thread = params["threadId"]
        .as_str()
        .filter(|value| !value.is_empty());
    let turn = params["turn"]["id"]
        .as_str()
        .filter(|value| !value.is_empty());
    let status = params["turn"]["status"].as_str();
    let (Some(thread), Some(turn), Some(status)) = (thread, turn, status) else {
        return Ok(None);
    };
    if !matches!(status, "completed" | "failed" | "interrupted") {
        return Ok(None);
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_context_terminal_receipts WHERE id=?1",
            [terminal_receipt_key(account, thread, turn)],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw: Value = serde_json::from_str(&raw).map_err(|_| RepairError::ReceiptHold)?;
    let source = &raw["source"];
    let proof = &raw["proof"];
    let snapshot = &raw["startAttemptSnapshot"];
    if raw["phase"] != "completed"
        || source["accountKey"] != account
        || source["threadId"] != thread
        || proof["threadId"] != thread
        || proof["turnId"] != turn
        || proof["status"] != status
        || raw["connectionGeneration"].as_u64() != Some(generation)
    {
        return Ok(None);
    }
    let agent_id = source["id"].as_str().ok_or(RepairError::ReceiptHold)?;
    let epoch = source["epoch"].as_i64().ok_or(RepairError::ReceiptHold)?;
    let attempt_id = snapshot["id"].as_str().filter(|value| !value.is_empty());
    let Some(attempt_id) = attempt_id else {
        return Ok(None);
    };
    let Some(agent) = ContextRepairRepository::agent(tx, agent_id)? else {
        return Ok(None);
    };
    let eligible = std::iter::once(agent.get("contextRepair").unwrap_or(&Value::Null))
        .chain(
            agent["contextRepairHistory"]
                .as_array()
                .into_iter()
                .flatten(),
        )
        .filter(|repair| {
            repair["phase"] == "completed"
                && repair["agent"] == agent_id
                && repair["source"]["id"] == agent_id
                && repair["source"]["epoch"] == epoch
                && repair["source"]["accountKey"] == account
                && repair["newThreadId"].is_string()
        })
        .collect::<Vec<_>>();
    let exact = eligible.iter().any(|repair| {
        repair["id"] == raw["operationId"]
            && repair["source"] == *source
            && repair["newThreadId"] == raw["newThreadId"]
            && repair["nativeTerminalReceipt"] == *proof
    });
    let mut destination = raw["newThreadId"].as_str().unwrap_or_default();
    let mut seen = BTreeSet::new();
    while exact && destination != agent["threadId"].as_str().unwrap_or_default() {
        if destination.is_empty() || !seen.insert(destination.to_owned()) {
            break;
        }
        let next = eligible
            .iter()
            .filter(|repair| repair["source"]["threadId"] == destination)
            .filter_map(|repair| repair["newThreadId"].as_str())
            .collect::<BTreeSet<_>>();
        if next.len() != 1 {
            break;
        }
        destination = next.into_iter().next().unwrap_or_default();
    }
    if studio_runtime_store::truthy(agent.get("deletedAt"))
        || agent["epoch"] != epoch
        || agent["accountKey"].as_str().unwrap_or("default") != account
        || !exact
        || destination != agent["threadId"].as_str().unwrap_or_default()
    {
        return Ok(None);
    }
    Ok(Some(studio_runtime_store::TurnAttemptIdentity {
        agent_id: agent_id.to_owned(),
        epoch,
        attempt_id: attempt_id.to_owned(),
    }))
}

fn archive_previous_repair(agent: &mut Value) -> Result<(), RepairError> {
    let previous = agent.get("contextRepair").cloned().unwrap_or(Value::Null);
    if !previous.is_object() {
        return Ok(());
    }
    if previous["phase"] == "completed" || studio_runtime_store::truthy(previous.get("rpcMethod")) {
        if agent
            .get("contextRepairHistory")
            .is_some_and(|history| !history.is_array())
        {
            return Err(RepairError::Invalid("agent.contextRepairHistory"));
        }
        let history = agent
            .as_object_mut()
            .ok_or(RepairError::Invalid("agent"))?
            .entry("contextRepairHistory")
            .or_insert_with(|| json!([]));
        history
            .as_array_mut()
            .ok_or(RepairError::Invalid("agent.contextRepairHistory"))?
            .push(previous);
    } else {
        agent["lastContextRepairCheck"] = previous;
    }
    Ok(())
}
impl From<crate::context_repair_files::HistoryFileError> for RepairError {
    fn from(error: crate::context_repair_files::HistoryFileError) -> Self {
        Self::HistoryFile(Box::new(error))
    }
}
impl From<rusqlite::Error> for RepairError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(StoreError::from(error))
    }
}

/// Account-worker transport identity, taken from the same connection that reads history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionIdentity {
    pub account_key: String,
    pub connection_id: String,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct InputRow {
    pub id: String,
    pub agent: String,
    pub epoch: i64,
    pub kind: String,
    pub text: String,
    pub status: String,
    pub created: f64,
    pub turn_id: Option<String>,
    pub error: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecoverySnapshot {
    raw_agent: Value,
    raw_events: Vec<Value>,
    agent: AgentRecord,
    events: Vec<InputRow>,
    connection: ConnectionIdentity,
    raw_batches: Vec<Value>,
    pending: Option<Vec<Value>>,
    batches: Vec<HistoricalBatch>,
}

impl RecoverySnapshot {
    /// The only constructor; also used by the Python-written differential corpus.
    pub fn from_records(
        agent: Value,
        events: Vec<Value>,
        connection: ConnectionIdentity,
        raw_batches: Vec<Value>,
        pending: Option<Vec<Value>>,
    ) -> Result<Self, RepairError> {
        validate_agent(&agent)?;
        if raw_batches.len() > MAX_INPUTS
            || events.len() > MAX_INPUTS
            || serde_json::to_vec(&(&agent, &events, &pending))
                .map_err(|_| RepairError::Invalid("capture"))?
                .len()
                > MAX_CAPTURE_BYTES
        {
            return Err(RepairError::Invalid("capture.bounds"));
        }
        let parsed =
            AgentRecord::from_value(agent.clone()).map_err(|_| RepairError::Invalid("agent"))?;
        let account = string_default(&agent, "accountKey", "default")?;
        if connection.account_key != account || connection.connection_id.is_empty() {
            return Err(RepairError::Invalid("connection"));
        }
        for field in [
            "contextRepair",
            "contextRepairWait",
            "startAttempt",
            "restartRecovery",
        ] {
            if agent
                .get(field)
                .is_some_and(|value| !value.is_null() && !value.is_object())
            {
                return Err(RepairError::Invalid("agent.recovery"));
            }
        }
        let mut ids = BTreeSet::new();
        let mut rows = Vec::new();
        for value in &events {
            let row: InputRow = serde_json::from_value(value.clone())
                .map_err(|_| RepairError::Invalid("events"))?;
            if row.id.is_empty()
                || !ids.insert(row.id.clone())
                || row.agent != parsed.id.0
                || row.epoch != parsed.epoch
                || !matches!(
                    row.status.as_str(),
                    "reserved" | "dispatching" | "uncertain"
                )
                || !row.created.is_finite()
            {
                return Err(RepairError::Invalid("events.scope"));
            }
            if let Some(meta) = &row.metadata {
                validate("RuntimeEventMetadataRecord", meta, "events.metadata")?;
            }
            rows.push(row);
        }
        if let Some(pending) = &pending
            && (pending.len() > MAX_INPUTS
                || pending.iter().any(|row| {
                    !row.is_object()
                        || row["agent"] != agent["id"]
                        || row["epoch"] != agent["epoch"]
                        || row["status"] != "pending"
                        || !row["turn_id"].is_null()
                }))
        {
            return Err(RepairError::Invalid("pending"));
        }
        let batches = raw_batches
            .iter()
            .map(|source| HistoricalBatch::from_records(&agent, source))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(Self {
            raw_batches,
            pending,
            batches,
            raw_agent: agent,
            raw_events: events,
            agent: parsed,
            events: rows,
            connection,
        })
    }

    pub fn agent(&self) -> &Value {
        &self.raw_agent
    }
    pub fn connection(&self) -> &ConnectionIdentity {
        &self.connection
    }
    pub fn event_ids(&self) -> Vec<&str> {
        self.events
            .iter()
            .filter(|row| self.requested(row))
            .map(|row| row.id.as_str())
            .collect()
    }

    fn requested(&self, event: &InputRow) -> bool {
        if self.restart_wait() {
            match self.raw_agent["restartRecovery"]["startAttempt"]["events"]
                .as_array()
                .filter(|ids| !ids.is_empty())
            {
                Some(ids) => ids.contains(&json!(event.id)),
                None => event.kind == "user",
            }
        } else {
            self.raw_agent["contextRepairWait"]["error"]
                .as_str()
                .and_then(|error| error.strip_prefix(WAIT_PREFIX))
                == Some(event.id.as_str())
        }
    }
    fn historical_wait(&self) -> bool {
        !self.restart_wait()
            && self.raw_agent["contextRepairWait"]["error"]
                .as_str()
                .and_then(|error| error.strip_prefix(WAIT_PREFIX))
                .is_some_and(|id| {
                    !self.raw_agent["startAttempt"]["events"]
                        .as_array()
                        .is_some_and(|ids| ids.contains(&json!(id)))
                })
    }
    fn restart_wait(&self) -> bool {
        let a = &self.raw_agent;
        let marker = &a["restartRecovery"];
        marker["stage"] == "held"
            && marker["autoWake"] == true
            && marker["epoch"] == a["epoch"]
            && marker.get("accountKey").unwrap_or(&json!("default"))
                == a.get("accountKey").unwrap_or(&json!("default"))
            && marker["threadId"] == a["threadId"]
    }
    fn eligible(&self) -> bool {
        let a = &self.raw_agent;
        let wait = &a["contextRepairWait"];
        let source = json!({"id": a["id"], "epoch": a["epoch"], "accountKey": a["accountKey"], "threadId": a["threadId"], "attemptId": if a["startAttempt"]["submitted"] == true { Value::Null } else { a["startAttempt"]["id"].clone() }});
        a["deletedAt"].is_null()
            && a["status"] != "paused"
            && a["threadId"].as_str().is_some_and(|id| !id.is_empty())
            && wait.is_object()
            && (!self.historical_wait()
                || (self.pending.is_some()
                    && a["inFlight"] == false
                    && a["startAttempt"]["submitted"] == false
                    && wait["events"] == a["startAttempt"]["events"]
                    && ["action", "actionRequestId", "actionIdentity"]
                        .iter()
                        .all(|key| wait.get(*key) == a["startAttempt"].get(*key))))
            && !active_repair(a)
            && (self.restart_wait()
                || (a["autoWake"] == true
                    && !studio_runtime_store::truthy(a.get("nativeFailureHold"))
                    && wait["source"] == source
                    && wait["error"]
                        .as_str()
                        .is_some_and(|error| error.starts_with(WAIT_PREFIX))))
    }
}

fn validate_agent(agent: &Value) -> Result<(), RepairError> {
    validate("PersistedAgentRecord", agent, "agent")?;
    for key in ["autoWake", "inFlight"] {
        if agent.get(key).is_some_and(|v| !v.is_boolean()) {
            return Err(RepairError::Invalid("agent.flags"));
        }
    }
    for key in ["threadId", "accountKey", "provider"] {
        if agent
            .get(key)
            .is_some_and(|v| !v.is_null() && !v.is_string())
        {
            return Err(RepairError::Invalid("agent.identity"));
        }
    }
    if let Some(repair) = agent.get("contextRepair").filter(|v| !v.is_null())
        && !matches!(
            repair["phase"].as_str(),
            Some(
                "preparing"
                    | "submitted"
                    | "ready"
                    | "unknown"
                    | "unchanged"
                    | "completed"
                    | "failed"
            )
        )
    {
        return Err(RepairError::Invalid("agent.contextRepair.phase"));
    }
    for attempt in [
        &agent["startAttempt"],
        &agent["restartRecovery"]["startAttempt"],
    ] {
        if attempt.is_null() {
            continue;
        }
        if !attempt.is_object()
            || attempt
                .get("submitted")
                .is_some_and(|v| !v.is_null() && !v.is_boolean())
        {
            return Err(RepairError::Invalid("agent.startAttempt"));
        }
        if let Some(ids) = attempt.get("events") {
            let Some(ids) = ids.as_array().filter(|ids| ids.len() <= MAX_INPUTS) else {
                return Err(RepairError::Invalid("agent.startAttempt.events"));
            };
            let mut unique = BTreeSet::new();
            if ids.iter().any(|id| {
                id.as_str()
                    .is_none_or(|id| id.is_empty() || !unique.insert(id))
            }) {
                return Err(RepairError::Invalid("agent.startAttempt.events"));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate(
    schema: &str,
    value: &Value,
    location: &'static str,
) -> Result<(), RepairError> {
    if !studio_runtime_schema::validate(schema, value)
        .map_err(|_| RepairError::Invalid(location))?
    {
        return Err(RepairError::Invalid(location));
    }
    Ok(())
}
pub(crate) fn string_default<'a>(
    value: &'a Value,
    field: &str,
    default: &'a str,
) -> Result<&'a str, RepairError> {
    value.get(field).map_or(Ok(default), |v| {
        v.as_str().ok_or(RepairError::Invalid("agent.identity"))
    })
}
fn active_repair(a: &Value) -> bool {
    matches!(
        a["contextRepair"]["phase"].as_str(),
        Some("preparing" | "submitted" | "unknown" | "ready")
    )
}

/// Native history returned by a read-only account worker request. It is scoped to
/// the frozen transport and thread; turns may contain only synthetic data in tests.
#[derive(Clone, Debug)]
pub struct NativeHistory {
    connection: ConnectionIdentity,
    thread_id: String,
    turns: Vec<Value>,
    state: Option<String>,
    operations: Vec<Value>,
}
impl NativeHistory {
    pub fn from_records(
        connection: ConnectionIdentity,
        thread_id: String,
        turns: Vec<Value>,
        state: Option<String>,
        operations: Vec<Value>,
    ) -> Result<Self, RepairError> {
        if thread_id.is_empty()
            || operations.len() > MAX_INPUTS * 65
            || turns.len() > MAX_TURNS
            || serde_json::to_vec(&(&turns, &operations))
                .map_err(|_| RepairError::Invalid("history"))?
                .len()
                > MAX_CAPTURE_BYTES
        {
            return Err(RepairError::Invalid("history.bounds"));
        }
        for turn in &turns {
            // Python's native projection writes accepted; the Claude bridge
            // additionally writes preparing/not_applied. Failed is a turn status,
            // never a startOutcome. Legacy history may omit this field or use null.
            if !turn["startOutcome"].is_null()
                && !matches!(
                    turn["startOutcome"].as_str(),
                    Some("accepted" | "preparing" | "not_applied")
                )
            {
                return Err(RepairError::Invalid("history.turn.startOutcome"));
            }
            if !turn.is_object()
                || turn
                    .get("clientUserMessageId")
                    .is_some_and(|v| !v.is_null() && !v.is_string())
                || turn
                    .get("id")
                    .is_some_and(|id| !id.is_null() && !id.is_string())
                || turn
                    .get("items")
                    .is_some_and(|items| !items.is_null() && !items.is_array())
            {
                return Err(RepairError::Invalid("history.turn"));
            }
            if let Some(items) = turn["items"].as_array()
                && items.iter().any(|item| !item.is_object())
            {
                return Err(RepairError::Invalid("history.items"));
            }
        }
        Ok(Self {
            connection,
            thread_id,
            turns,
            state,
            operations,
        })
    }
    pub fn turns(&self) -> &[Value] {
        &self.turns
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum InputDecision {
    Delivered {
        id: String,
        #[serde(rename = "turnId")]
        turn_id: String,
    },
    Waiting {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<&'static str>,
    },
    NotDelivered {
        id: String,
    },
}

/// Acceptance needs exactly one identified turn; replay additionally needs idle
/// native state, identified history, and either a rejection or a saved unsent owner.
pub fn decide(snapshot: &RecoverySnapshot, history: &NativeHistory) -> Vec<InputDecision> {
    let valid = snapshot.eligible()
        && snapshot.connection == history.connection
        && snapshot.raw_agent["threadId"] == history.thread_id;
    let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut preparing = BTreeSet::new();
    let mut rejected = BTreeSet::new();
    let mut supported = false;
    let mut unidentified = false;
    for turn in &history.turns {
        let mut ids = BTreeSet::new();
        if let Some(id) = turn["clientUserMessageId"].as_str() {
            ids.insert(id.to_owned());
        }
        if let Some(items) = turn["items"].as_array() {
            for item in items {
                if item["type"] == "userMessage" {
                    if let Some(id) = item["clientId"].as_str() {
                        ids.insert(id.to_owned());
                    } else if !turn["clientUserMessageId"].is_string()
                        && !matches!(
                            turn["startOutcome"].as_str(),
                            Some("preparing" | "not_applied")
                        )
                    {
                        unidentified = true;
                    }
                }
            }
        }
        supported |= !ids.is_empty();
        match turn["startOutcome"].as_str() {
            Some("preparing") => preparing.extend(ids),
            Some("not_applied") => rejected.extend(ids),
            // The checked constructor admits only accepted, absent or null here.
            _ => {
                for id in ids {
                    found
                        .entry(id)
                        .or_default()
                        .push(turn["id"].as_str().unwrap_or_default().to_owned());
                }
            }
        }
    }
    for batch in &snapshot.batches {
        for (id, turn) in batch.accepted(
            &history.turns,
            &history.operations,
            &format!("account:{}", history.connection.account_key),
        ) {
            let turns = found.entry(id).or_default();
            if !turns.contains(&turn) {
                turns.push(turn);
            }
        }
    }
    snapshot
        .events
        .iter()
        .filter(|event| snapshot.requested(event))
        .map(|event| {
            let a = &snapshot.raw_agent;
            let owners = [&a["startAttempt"], &a["restartRecovery"]["startAttempt"]]
                .into_iter()
                .filter(|owner| {
                    owner["events"]
                        .as_array()
                        .is_some_and(|ids| ids.contains(&json!(event.id)))
                })
                .collect::<Vec<_>>();
            let known_unsent = !snapshot.historical_wait()
                && !owners.is_empty()
                && owners.iter().all(|owner| owner["submitted"] == false);
            let turns = found.get(&event.id);
            let meta_scope = event.metadata.as_ref().is_none_or(|meta| {
                (!studio_runtime_store::truthy(meta.get("native"))
                    || [
                        ("agent", a["id"].clone()),
                        ("epoch", a["epoch"].clone()),
                        ("threadId", a["threadId"].clone()),
                        ("accountKey", json!(snapshot.connection.account_key)),
                    ]
                    .iter()
                    .all(|(key, value)| meta["native"][key] == *value))
                    && (meta["contextManifest"]["epoch"].is_null()
                        || meta["contextManifest"]["epoch"][0] == a["threadId"])
            });
            if valid
                && meta_scope
                && !preparing.contains(&event.id)
                && !rejected.contains(&event.id)
                && let Some(turns) = turns
                && turns.len() == 1
                && !turns[0].is_empty()
            {
                InputDecision::Delivered {
                    id: event.id.clone(),
                    turn_id: turns[0].clone(),
                }
            } else if valid
                && meta_scope
                && a["provider"] != "claude"
                && turns.is_none()
                && !preparing.contains(&event.id)
                && supported
                && !unidentified
                && matches!(history.state.as_deref(), Some("idle" | "notLoaded"))
                && (known_unsent || rejected.contains(&event.id))
            {
                InputDecision::NotDelivered {
                    id: event.id.clone(),
                }
            } else {
                InputDecision::Waiting {
                    id: event.id.clone(),
                    reason: if preparing.contains(&event.id) {
                        Some("The native input preparation has no acceptance receipt")
                    } else if turns.is_some_and(|turns| turns.len() == 1 && turns[0].is_empty()) {
                        Some("The matching native turn has no ID")
                    } else {
                        None
                    },
                }
            }
        })
        .collect()
}

/// Required shared port: no provider call or filesystem work is allowed here.
/// The implementation promotes turn_prepare's manifest write intents in this transaction.
pub trait DeliveredManifestWriter {
    fn remember(
        &self,
        transaction: &Transaction<'_>,
        agent: &str,
        events: &[String],
    ) -> Result<(), RepairError>;
}
/// Integration default: an absent writer is a named refusal and rolls back all writes.
pub struct ManifestWriterUnavailable;
impl DeliveredManifestWriter for ManifestWriterUnavailable {
    fn remember(&self, _: &Transaction<'_>, _: &str, _: &[String]) -> Result<(), RepairError> {
        Err(RepairError::ManifestWriterUnavailable)
    }
}

/// Reads are bounded by the account worker's existing request deadline. Identity
/// returns its cached identity only, and is checked again inside the transaction.
pub trait HistoryReader {
    fn identity(&self) -> ConnectionIdentity;
    fn read(&mut self, snapshot: &RecoverySnapshot) -> Result<NativeHistory, RepairError>;
}

pub fn recover(
    store: &RuntimeStore,
    agent: &str,
    reader: &mut impl HistoryReader,
    manifests: &impl DeliveredManifestWriter,
    now: f64,
) -> Result<Vec<InputDecision>, RepairError> {
    if !now.is_finite() {
        return Err(RepairError::Invalid("now"));
    }
    let snapshot = store.transaction_with(|tx| capture(tx, agent, reader.identity()))?;
    if !snapshot.eligible() {
        return Ok(snapshot
            .event_ids()
            .into_iter()
            .map(|id| InputDecision::Waiting {
                id: id.to_owned(),
                reason: None,
            })
            .collect());
    }
    if snapshot.restart_wait() && snapshot.event_ids().is_empty() {
        return store
            .transaction_with(|tx| apply(tx, &snapshot, &[], reader.identity(), manifests, now));
    }
    let history = reader.read(&snapshot)?;
    store.transaction_with(|tx| {
        reconcile(tx, &snapshot, &history, reader.identity(), manifests, now)
    })
}

pub fn capture(
    tx: &Transaction<'_>,
    agent: &str,
    connection: ConnectionIdentity,
) -> Result<RecoverySnapshot, RepairError> {
    require_schema(
        tx,
        &[
            "runtime_event_meta",
            "runtime_items",
            "runtime_item_fulltext",
            "runtime_completed_turns",
        ],
    )?;
    let (agent, events) = ContextRepairRepository::capture(tx, agent, MAX_INPUTS + 1)?
        .ok_or(RepairError::MissingAgent)?;
    let batches = ContextRepairRepository::batches(tx, &agent, &events)?;
    let pending = ContextRepairRepository::pending(tx, &agent)?;
    RecoverySnapshot::from_records(agent, events, connection, batches, pending)
}

/// Transaction-owned consumer used by the account worker and deterministic tests.
/// The native read has already completed; this performs no external I/O.
pub fn reconcile(
    tx: &Transaction<'_>,
    snapshot: &RecoverySnapshot,
    history: &NativeHistory,
    connection: ConnectionIdentity,
    manifests: &impl DeliveredManifestWriter,
    now: f64,
) -> Result<Vec<InputDecision>, RepairError> {
    if !now.is_finite() {
        return Err(RepairError::Invalid("now"));
    }
    apply(
        tx,
        snapshot,
        &decide(snapshot, history),
        connection,
        manifests,
        now,
    )
}

fn apply(
    tx: &Transaction<'_>,
    snapshot: &RecoverySnapshot,
    decisions: &[InputDecision],
    connection: ConnectionIdentity,
    manifests: &impl DeliveredManifestWriter,
    now: f64,
) -> Result<Vec<InputDecision>, RepairError> {
    let fresh = capture(tx, &snapshot.agent.id.0, connection)?;
    if fresh != *snapshot {
        return Ok(snapshot
            .event_ids()
            .into_iter()
            .map(|id| InputDecision::Waiting {
                id: id.to_owned(),
                reason: None,
            })
            .collect());
    }
    let delivered = decisions
        .iter()
        .filter_map(|decision| match decision {
            InputDecision::Delivered { id, turn_id } => Some((id.clone(), turn_id.clone())),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let pending = decisions
        .iter()
        .filter_map(|d| {
            if let InputDecision::NotDelivered { id } = d {
                Some(id)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    if delivered.is_empty()
        && pending.is_empty()
        && !(snapshot.restart_wait() && decisions.is_empty())
    {
        return Ok(decisions.to_vec());
    }
    let mut agent = snapshot.raw_agent.clone();
    if !agent["startAttempt"].is_object() && agent["restartRecovery"]["startAttempt"].is_object() {
        agent["startAttempt"] = agent["restartRecovery"]["startAttempt"].clone();
    }
    let attempt_ids = agent["startAttempt"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for (id, turn) in &delivered {
        ContextRepairRepository::deliver(tx, id, &snapshot.agent.id.0, snapshot.agent.epoch, turn)?;
    }
    for id in &pending {
        ContextRepairRepository::requeue(tx, id, &snapshot.agent.id.0, snapshot.agent.epoch)?;
    }
    if !delivered.is_empty() {
        manifests.remember(
            tx,
            &snapshot.agent.id.0,
            &delivered.keys().cloned().collect::<Vec<_>>(),
        )?;
    }
    if let Some((_, turn)) = delivered
        .iter()
        .find(|(id, _)| attempt_ids.contains(&json!(id)))
    {
        agent["startAttempt"]["submitted"] = json!(true);
        agent["startAttempt"]["turnId"] = json!(turn);
        agent["startAttempt"]["observedTurnId"] = json!(turn);
        if !ContextRepairRepository::completed(tx, &snapshot.agent.id.0, turn)? {
            agent["status"] = json!("running");
            agent["inFlight"] = json!(true);
            agent["turnId"] = json!(turn);
        }
    }
    if delivered.len() + pending.len() == decisions.len() {
        if delivered.is_empty()
            && !(snapshot.restart_wait() && decisions.is_empty() && agent["inFlight"] == true)
        {
            agent["status"] = json!("queued");
            agent["inFlight"] = json!(false);
        }
        if snapshot.restart_wait() {
            agent["autoWake"] = json!(true);
            agent["nativeFailureHold"] = Value::Null;
            agent["restartRecovery"]["stage"] = json!("finished");
            agent["restartRecovery"]["reconciledAt"] = json!(now);
        }
        if let Some(object) = agent.as_object_mut() {
            object.remove("contextRepairWait");
        }
        if agent["error"]
            .as_str()
            .is_some_and(|error| error.starts_with(WAIT_PREFIX))
        {
            agent["error"] = Value::Null;
        }
    }
    validate("PersistedAgentRecord", &agent, "agent.write")?;
    ContextRepairRepository::put_agent(tx, &agent)?;
    let projected = crate::sync_views::project_persisted_record("agent", &agent)
        .ok_or(RepairError::Invalid("agent.projection"))?;
    let fields = [
        "status",
        "inFlight",
        "turnId",
        "contextRepairWait",
        "error",
        "autoWake",
        "nativeFailureHold",
        "restartRecovery",
    ]
    .iter()
    .map(|field| {
        (
            *field,
            projected.get(*field).cloned().unwrap_or(Value::Null),
        )
    })
    .collect::<Vec<_>>();
    studio_runtime_store::EventWriteRepository::set_agent_sync_values(
        tx,
        &snapshot.agent.id.0,
        &fields,
    )?;
    Ok(decisions.to_vec())
}

/// The scheduler and explicit repair route share the same unresolved-receipt gate.
pub fn assert_available(agent: &Value) -> Result<(), RepairError> {
    validate_agent(agent)?;
    if active_repair(agent) || studio_runtime_store::truthy(agent.get("contextRepairWait")) {
        return Err(RepairError::ReceiptHold);
    }
    Ok(())
}

fn require_schema(tx: &Transaction<'_>, tables: &[&str]) -> Result<(), RepairError> {
    for table in tables {
        if !crate::sync_policy::table_exists(tx, table)? {
            return Err(RepairError::MissingSchema);
        }
    }
    Ok(())
}

/// Partial explicit repair operation: real no-op paths return their saved receipt.
pub fn repair_idle(store: &RuntimeStore, id: &str) -> Result<Value, RepairError> {
    store.transaction_with(|db| {
        let mut agent = ContextRepairRepository::agent(db, id)?.ok_or(RepairError::MissingAgent)?;
        assert_available(&agent)?;
        if agent["environment"] == "linux"
            && studio_runtime_store::truthy(agent.get("imageWorkspaceReady"))
        {
            const NOTE: &str = "Automatic context repair is skipped for Linux workers. The rollout stays in the guest. The next turn can proceed.";
            if agent.get("contextRepairNote").and_then(Value::as_str) != Some(NOTE) {
                agent["contextRepairNote"] = json!(NOTE);
                ContextRepairRepository::put_agent(db, &agent)?;
            }
            return Ok(json!({
                "id": id,
                "repair": agent.get("contextRepair").cloned().unwrap_or(Value::Null)
            }));
        }
        if agent["provider"] == "claude"
            || agent["threadId"].is_null()
            || agent["threadId"] == ""
        {
            return Ok(
                json!({"id": id, "repair": agent.get("contextRepair").cloned().unwrap_or(Value::Null)}),
            );
        }
        require_schema(db, &["runtime_event_meta"])?;
        if !ContextRepairRepository::has_candidates(
            db,
            id,
            agent["epoch"]
                .as_i64()
                .ok_or(RepairError::Invalid("agent.epoch"))?,
        )? {
            return Ok(
                json!({"id": id, "repair": agent.get("contextRepair").cloned().unwrap_or(Value::Null)}),
            );
        }
        Err(RepairError::RewriteUnavailable)
    })
}

/// Reconcile and safely fork an idle Codex history after durable event writes.
/// Provider IO and history copying happen outside the store transaction; the
/// exact fork intent is committed before the only mutating provider request.
pub fn repair_idle_with_broker(
    store: &RuntimeStore,
    id: &str,
    provider_home: &Path,
    broker: &dyn AccountSessionPort,
    deadline: Instant,
) -> Result<Value, RepairError> {
    let previous_operation = store
        .read()
        .ok()
        .and_then(|db| ContextRepairRepository::agent(&db, id).ok().flatten())
        .and_then(|agent| {
            agent
                .pointer("/contextRepair/id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let result = repair_idle_with_broker_inner(store, id, provider_home, broker, deadline);
    if let Err(error) = &result {
        // A failed read/copy before fork submission can be retried under a new
        // operation. Any submitted operation stays held as Unknown.
        let _settlement_result = store.transaction_with(|tx| {
            let Some(mut current) = ContextRepairRepository::agent(tx, id)? else {
                return Ok::<(), RepairError>(());
            };
            let current_operation = current.pointer("/contextRepair/id").and_then(Value::as_str);
            if current_operation.is_some()
                && current_operation != previous_operation.as_deref()
                && matches!(
                    current["contextRepair"]["phase"].as_str(),
                    Some("preparing" | "submitted")
                )
            {
                current["contextRepair"]["phase"] =
                    json!(if current["contextRepair"]["phase"] == "submitted" {
                        "unknown"
                    } else {
                        "failed"
                    });
                current["contextRepair"]["error"] = json!(error.to_string());
                ContextRepairRepository::put_agent(tx, &current)?;
            }
            Ok(())
        });
    }
    result
}

fn repair_idle_with_broker_inner(
    store: &RuntimeStore,
    id: &str,
    provider_home: &Path,
    broker: &dyn AccountSessionPort,
    deadline: Instant,
) -> Result<Value, RepairError> {
    let initial_db = store.read()?;
    let initial =
        ContextRepairRepository::agent(&initial_db, id)?.ok_or(RepairError::MissingAgent)?;
    let initial_account = string_default(&initial, "accountKey", "default")?.to_owned();
    let scope_before = if initial["provider"] != "claude"
        && initial["threadId"]
            .as_str()
            .is_some_and(|thread| !thread.is_empty())
        && !(initial["environment"] == "linux"
            && studio_runtime_store::truthy(initial.get("imageWorkspaceReady")))
    {
        broker.current_scope(&initial_account)
    } else {
        None
    };
    let tool_receipt_snapshot = if initial["provider"] != "claude"
        && initial["threadId"]
            .as_str()
            .is_some_and(|thread| !thread.is_empty())
        && !(initial["environment"] == "linux"
            && studio_runtime_store::truthy(initial.get("imageWorkspaceReady")))
    {
        crate::provider_callbacks::prepare_unresolved_context_tool_receipts(
            &initial_db,
            store.state_dir(),
            id,
            &initial_account,
            initial["threadId"]
                .as_str()
                .ok_or(RepairError::Invalid("agent.threadId"))?,
            initial["accountHistory"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default(),
        )
        .map_err(|_| RepairError::ReceiptHold)?
    } else {
        crate::provider_callbacks::ContextToolReceiptSnapshot::empty()
    };
    drop(initial_db);
    let (agent, events, tool_receipts, historical_inputs, scope, operation_id) = store.transaction_with(|tx| {
        let mut agent = ContextRepairRepository::agent(tx, id)?.ok_or(RepairError::MissingAgent)?;
        if agent["epoch"] != initial["epoch"] || agent["threadId"] != initial["threadId"]
            || string_default(&agent, "accountKey", "default")? != initial_account
            || agent.get("accountHistory") != initial.get("accountHistory")
        {
            return Err(RepairError::ReceiptHold);
        }
        assert_available(&agent)?;
        if agent["environment"] == "linux"
            && studio_runtime_store::truthy(agent.get("imageWorkspaceReady"))
        {
            const NOTE: &str = "Automatic context repair is skipped for Linux workers. The rollout stays in the guest. The next turn can proceed.";
            agent["contextRepairNote"] = json!(NOTE);
            ContextRepairRepository::put_agent(tx, &agent)?;
            return Ok((agent, Vec::new(), Vec::new(), Vec::new(), None, String::new()));
        }
        if agent["provider"] == "claude"
            || agent["threadId"].as_str().is_none_or(str::is_empty)
        {
            return Ok((agent, Vec::new(), Vec::new(), Vec::new(), None, String::new()));
        }
        require_schema(tx, &["runtime_event_meta", "runtime_completed_turns"])?;
        ensure_local_idle(tx, &agent)?;
        let tool_receipts = crate::provider_callbacks::revalidate_unresolved_context_tool_receipts(
            tx,
            id,
            &tool_receipt_snapshot,
        )
        .map_err(|_| RepairError::ReceiptHold)?;
        let historical_inputs = uncertain_input_receipts(tx, id, agent["epoch"].as_i64().ok_or(RepairError::Invalid("agent.epoch"))?)?;
        let events = verified_repair_events(tx, &agent)?;
        if events.is_empty() && historical_inputs.is_empty() && tool_receipts.is_empty() {
            return Ok((agent, events, tool_receipts, historical_inputs, None, String::new()));
        }
        let epoch = agent["epoch"].as_i64().ok_or(RepairError::Invalid("agent.epoch"))?;
        let account = string_default(&agent, "accountKey", "default")?.to_owned();
        let scope = scope_before.clone().ok_or(RepairError::AccountSession)?;
        if scope.account != account || scope.connection.is_empty() {
            return Err(RepairError::AccountSession);
        }
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|_| RepairError::Invalid("clock"))?.as_nanos();
        let operation_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID,
            format!("context-repair:{id}:{epoch}:{}:{stamp}", agent["threadId"].as_str().unwrap_or_default()).as_bytes()).to_string();
        archive_previous_repair(&mut agent)?;
        let repair = json!({
            "id":operation_id,
            "agent":id,
            "source":{"id":id,"epoch":epoch,"accountKey":account,"threadId":agent["threadId"]},
            "phase":"preparing",
            "created":SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| RepairError::Invalid("clock"))?.as_secs_f64(),
            "checkedEventIds":events.iter().filter_map(|e| e["id"].as_str()).collect::<Vec<_>>(),
            "compactions":agent.get("compactions").and_then(Value::as_u64).unwrap_or(0),
            "startAttemptSnapshot":agent.get("startAttempt").cloned().unwrap_or(Value::Null),
            "connectionId":scope.connection,
            "connectionGeneration":scope.generation
        });
        agent["contextRepair"] = repair;
        ContextRepairRepository::put_agent(tx, &agent)?;
        Ok((agent, events, tool_receipts, historical_inputs, Some(scope), operation_id))
    })?;
    let Some(scope) = scope else {
        return Ok(
            json!({"id":id,"repair":agent.get("contextRepair").cloned().unwrap_or(Value::Null)}),
        );
    };
    let account = scope.account.clone();
    let thread = agent["threadId"]
        .as_str()
        .ok_or(RepairError::Invalid("agent.threadId"))?
        .to_owned();
    let epoch = agent["epoch"]
        .as_i64()
        .ok_or(RepairError::Invalid("agent.epoch"))?;
    let actor = crate::account_worker::retained::RequestActor {
        agent: id.to_owned(),
        epoch,
    };
    broker
        .callback_barrier(&account, &scope, deadline)
        .map_err(|_| RepairError::AccountSession)?;
    let read = broker_read(
        broker,
        BrokerRead {
            scope: &scope,
            actor: &actor,
            operation: &operation_id,
            suffix: "thread-read",
            method: studio_runtime_providers::Method::ThreadRead,
            params: json!({"threadId":thread,"includeTurns":false}),
            deadline,
        },
    )?;
    let native = read.get("thread").unwrap_or(&read);
    if native["id"] != thread {
        return Err(RepairError::ReceiptHold);
    }
    let status = native["status"]["type"]
        .as_str()
        .ok_or(RepairError::ReceiptHold)?;
    if !matches!(status, "idle" | "notLoaded" | "systemError") {
        return Err(RepairError::ReceiptHold);
    }
    let native_path = native["path"].as_str().ok_or(RepairError::ReceiptHold)?;
    assert_native_queues_empty(
        broker,
        &scope,
        &actor,
        &operation_id,
        &thread,
        status,
        deadline,
    )?;
    let turns = broker_read(
        broker,
        BrokerRead {
            scope: &scope,
            actor: &actor,
            operation: &operation_id,
            suffix: "turns-latest",
            method: studio_runtime_providers::Method::ThreadTurnsList,
            params: json!({"threadId":thread,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"}),
            deadline,
        },
    )?;
    let turn = turns["data"].as_array().and_then(|v| v.first());
    let inherited_empty = turn.is_none();
    if inherited_empty {
        // Without a current native turn, no uncertain local input or tool
        // callback can be attributed safely. The file helper separately proves
        // that the inherited JSONL ancestry already contains a terminal turn.
        if !historical_inputs.is_empty() || !tool_receipts.is_empty() {
            return Err(RepairError::ReceiptHold);
        }
        if !matches!(status, "idle" | "notLoaded") {
            return Err(RepairError::ReceiptHold);
        }
    }
    let terminal_id = turn
        .and_then(|turn| turn["id"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let terminal_status = turn.and_then(|turn| turn["status"].as_str());
    if !inherited_empty
        && !matches!(
            terminal_status,
            Some("completed" | "failed" | "interrupted")
        )
    {
        return Err(RepairError::ReceiptHold);
    }
    if let Some(terminal_id) = terminal_id.as_deref() {
        verify_native_items(
            broker,
            &scope,
            &actor,
            &operation_id,
            &thread,
            terminal_id,
            deadline,
        )?;
    }
    verify_tool_receipts(
        broker,
        &scope,
        &actor,
        &operation_id,
        &thread,
        &tool_receipts,
        deadline,
    )?;
    let checked = broker_read(
        broker,
        BrokerRead {
            scope: &scope,
            actor: &actor,
            operation: &operation_id,
            suffix: "thread-read-check",
            method: studio_runtime_providers::Method::ThreadRead,
            params: json!({"threadId":thread,"includeTurns":false}),
            deadline,
        },
    )?;
    let checked_thread = checked.get("thread").unwrap_or(&checked);
    let checked_status = checked_thread["status"]["type"].as_str();
    if checked_thread["id"] != thread
        || checked_thread["path"] != native_path
        || !(if inherited_empty {
            matches!(checked_status, Some("idle" | "notLoaded"))
        } else {
            matches!(checked_status, Some("idle" | "notLoaded" | "systemError"))
        })
    {
        return Err(RepairError::ReceiptHold);
    }
    let latest = broker_read(
        broker,
        BrokerRead {
            scope: &scope,
            actor: &actor,
            operation: &operation_id,
            suffix: "turns-latest-check",
            method: studio_runtime_providers::Method::ThreadTurnsList,
            params: json!({"threadId":thread,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"}),
            deadline,
        },
    )?;
    let current_turn = latest["data"].as_array().and_then(|v| v.first());
    if match (turn, current_turn) {
        (Some(before), Some(after)) => {
            before["id"] != after["id"]
                || before["status"] != after["status"]
                || before["startedAt"] != after["startedAt"]
                || before["completedAt"] != after["completedAt"]
        }
        (None, None) => false,
        _ => true,
    } {
        return Err(RepairError::ReceiptHold);
    }
    let turn_started = turn.and_then(|turn| turn["startedAt"].as_f64());
    let mut terminal_completed = turn.and_then(|turn| turn["completedAt"].as_f64());
    if !historical_inputs.is_empty() {
        let latest_input = historical_inputs
            .iter()
            .filter_map(|event| event["created"].as_f64())
            .fold(f64::NEG_INFINITY, f64::max);
        if terminal_completed.is_none()
            && terminal_status == Some("interrupted")
            && turn_started.is_some_and(|started| started > latest_input)
        {
            terminal_completed = Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| RepairError::Invalid("clock"))?
                    .as_secs_f64(),
            );
        }
        if terminal_completed
            .is_none_or(|completed| !completed.is_finite() || completed <= latest_input)
        {
            return Err(RepairError::ReceiptHold);
        }
    }
    let terminal_proof = json!({
        "threadId":thread,
        "turnId":terminal_id,
        "status":terminal_status,
        "nativeState":checked_thread["status"]["type"],
        "startedAt":turn_started,
        "completedAt":terminal_completed,
        "inheritedEmpty":inherited_empty,
    });
    store.transaction_with(|tx| {
        let mut current =
            ContextRepairRepository::agent(tx, id)?.ok_or(RepairError::MissingAgent)?;
        if current["contextRepair"]["id"] != operation_id
            || current["threadId"] != thread
            || current["epoch"] != agent["epoch"]
        {
            return Err(RepairError::ReceiptHold);
        }
        current["contextRepair"]["nativeTerminalReceipt"] = terminal_proof.clone();
        if !historical_inputs.is_empty() {
            current["contextRepair"]["historicalInputProof"] = json!({
                "events":historical_inputs,
                "terminalTurnId":terminal_id,
                "terminalCompletedAt":terminal_completed,
                "source":current["contextRepair"]["source"],
                "deliveryOutcome":"unknown"
            });
        }
        ContextRepairRepository::put_agent(tx, &current)?;
        Ok::<(), RepairError>(())
    })?;
    let located = crate::context_repair_files::locate_provider_history(
        crate::context_repair_files::HistoryProvider::Codex,
        provider_home,
        &thread,
    )?;
    if Path::new(native_path).canonicalize().ok().as_deref()
        != located.canonicalize().ok().as_deref()
    {
        return Err(RepairError::ReceiptHold);
    }
    crate::context_repair_files::verify_history_ancestry(provider_home, &located, &thread)?;
    let terminal = crate::context_repair_files::HistoryTerminal {
        turn_id: terminal_id,
        status: terminal_status.map(str::to_owned),
        completed_at: turn.and_then(|turn| turn["completedAt"].as_f64()),
        inherited_empty,
    };
    let rewrite = crate::context_repair_files::rewrite_provider_history(
        provider_home,
        &located,
        events.clone(),
        &thread,
        &terminal,
        &RepairSummary,
    )?;
    if rewrite.event_ids.is_empty() {
        if rewrite.copy_path.is_some() {
            crate::context_repair_files::cleanup_rewrite_copy(provider_home, &rewrite)?;
        }
        return store.transaction_with(|tx| {
            let mut current =
                ContextRepairRepository::agent(tx, id)?.ok_or(RepairError::MissingAgent)?;
            if current["contextRepair"]["id"] != operation_id || current["threadId"] != thread {
                return Err(RepairError::ReceiptHold);
            }
            current["contextRepair"]["phase"] = json!("unchanged");
            current["contextRepair"]["snapshot"] = repair_receipt(&rewrite);
            ContextRepairRepository::put_agent(tx, &current)?;
            Ok(json!({"id":id,"repair":current["contextRepair"]}))
        });
    }
    let copy_path = rewrite.copy_path.as_ref().ok_or(RepairError::ReceiptHold)?;
    let fork_params = json!({"threadId":rewrite.import_thread_id,"path":copy_path,"excludeTurns":true,"deferGoalContinuation":true});
    let prepared = store.transaction_with(|tx| {
        let mut current =
            ContextRepairRepository::agent(tx, id)?.ok_or(RepairError::MissingAgent)?;
        if current["contextRepair"]["id"] != operation_id
            || current["threadId"] != thread
            || current["epoch"] != agent["epoch"]
        {
            return Err(RepairError::ReceiptHold);
        }
        ensure_local_idle(tx, &current)?;
        current["contextRepair"]["phase"] = json!("submitted");
        current["contextRepair"]["rpcMethod"] = json!("thread/fork");
        current["contextRepair"]["forkOperation"] =
            json!(format!("context-repair:{operation_id}:fork"));
        current["contextRepair"]["snapshot"] = repair_receipt(&rewrite);
        current["contextRepair"]["forkParams"] = fork_params.clone();
        ContextRepairRepository::put_agent(tx, &current)?;
        Ok(())
    });
    if let Err(error) = prepared {
        crate::context_repair_files::cleanup_rewrite_copy(provider_home, &rewrite)?;
        return Err(error);
    }
    let fork_operation = format!("context-repair:{operation_id}:fork");
    let ticket = crate::workspace_operations::ProviderTicket {
        operation: fork_operation.clone(),
        agent: id.to_owned(),
        epoch: u64::try_from(epoch).map_err(|_| RepairError::Invalid("agent.epoch"))?,
        account: scope.account.clone(),
        thread: Some(thread.clone()),
        connection: scope.connection.clone(),
        generation: scope.generation,
        request: fork_operation,
    };
    let request = crate::native_actions::NativeRequest {
        ticket,
        method: "thread/fork".to_owned(),
        params: fork_params,
    };
    let result =
        crate::thread_fork_runtime::dispatch_reserved_via_broker(&request, broker, deadline)
            .map_err(|error| match error {
                crate::native_actions::NativeError::Unknown => RepairError::ProviderUnknown,
                crate::native_actions::NativeError::Unavailable(_)
                | crate::native_actions::NativeError::Changed
                | crate::native_actions::NativeError::Invalid(_) => RepairError::AccountSession,
                _ => RepairError::AccountSession,
            })
            .and_then(|receipt| match receipt.certainty {
                RequestCertainty::Written => match receipt.result {
                    Some(ProviderResponse::Reply(value)) => Ok(value),
                    Some(ProviderResponse::Rejected(_)) => Err(RepairError::ProviderRejected),
                    None => Err(RepairError::ProviderUnknown),
                },
                RequestCertainty::NotSent => Err(RepairError::AccountSession),
                RequestCertainty::Unknown => Err(RepairError::ProviderUnknown),
            });
    match result {
        Ok(value) => {
            let new_thread = value
                .pointer("/thread/id")
                .or_else(|| value.get("threadId"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && *s != thread)
                .ok_or(RepairError::ProviderUnknown)?
                .to_owned();
            store.transaction_with(|tx| {
                let mut current =
                    ContextRepairRepository::agent(tx, id)?.ok_or(RepairError::MissingAgent)?;
                if current["contextRepair"]["id"] != operation_id
                    || current["threadId"] != thread
                    || current["epoch"] != agent["epoch"]
                {
                    return Err(RepairError::ReceiptHold);
                }
                current["contextRepair"]["phase"] = json!("completed");
                current["contextRepair"]["newThreadId"] = json!(new_thread);
                current["contextRepair"]["repairedEventIds"] = json!(rewrite.event_ids);
                current["contextRepair"]["sourceSha256"] = json!(rewrite.source_sha256);
                current["threadId"] = json!(new_thread);
                register_source_terminal_receipt(tx, &current["contextRepair"], &new_thread)?;
                ContextRepairRepository::put_agent(tx, &current)?;
                Ok(json!({"id":id,"repair":current["contextRepair"]}))
            })
        }
        Err(RepairError::ProviderUnknown) => store.transaction_with(|tx| {
            let mut current =
                ContextRepairRepository::agent(tx, id)?.ok_or(RepairError::MissingAgent)?;
            if current["contextRepair"]["id"] == operation_id {
                current["contextRepair"]["phase"] = json!("unknown");
                ContextRepairRepository::put_agent(tx, &current)?;
            }
            Ok(json!({"id":id,"repair":current["contextRepair"]}))
        }),
        Err(error) => {
            crate::context_repair_files::cleanup_rewrite_copy(provider_home, &rewrite)?;
            store.transaction_with(|tx| {
                let mut current =
                    ContextRepairRepository::agent(tx, id)?.ok_or(RepairError::MissingAgent)?;
                if current["contextRepair"]["id"] == operation_id {
                    current["contextRepair"]["phase"] = json!("failed");
                    current["contextRepair"]["error"] = json!(error.to_string());
                    ContextRepairRepository::put_agent(tx, &current)?;
                }
                Ok::<(), RepairError>(())
            })?;
            Err(error)
        }
    }
}

struct RepairSummary;
impl RepairEventSummary for RepairSummary {
    fn summarize(&self, event: &Value) -> Result<String, RepairError> {
        let text = event["text"]
            .as_str()
            .ok_or(RepairError::Invalid("event.text"))?;
        if text.len() <= 3000 {
            return Ok(text.to_owned());
        }
        let excerpt = text.chars().take(1400).collect::<String>();
        let tail = text
            .chars()
            .rev()
            .take(900)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let summary = json!({"eventId":event["id"],"kind":event["kind"],"truncated":true,
            "textBytes":text.len(),"read":{"tool":"orchestration_read","output_ref":format!("event:{}",event["id"])},
            "excerpt":excerpt,"tail":tail});
        serde_json::to_string(&summary).map_err(|_| RepairError::Invalid("event.summary"))
    }
}

struct BrokerRead<'a> {
    scope: &'a ConnectionScope,
    actor: &'a crate::account_worker::retained::RequestActor,
    operation: &'a str,
    suffix: &'a str,
    method: studio_runtime_providers::Method,
    params: Value,
    deadline: Instant,
}

fn broker_read(
    broker: &dyn AccountSessionPort,
    request: BrokerRead<'_>,
) -> Result<Value, RepairError> {
    broker_call(
        broker,
        request.scope,
        request.actor,
        &format!("context-repair:{}:{}", request.operation, request.suffix),
        request.method,
        request.params,
        request.deadline,
    )
}
fn broker_call(
    broker: &dyn AccountSessionPort,
    scope: &ConnectionScope,
    actor: &crate::account_worker::retained::RequestActor,
    operation: &str,
    method: studio_runtime_providers::Method,
    params: Value,
    deadline: Instant,
) -> Result<Value, RepairError> {
    match broker.provider_request(ProviderRequest {
        account: scope.account.clone(),
        operation: operation.to_owned(),
        actor: Some(actor.clone()),
        expected_scope: Some(scope.clone()),
        deleted_actor_session: None,
        method,
        params,
        deadline,
    }) {
        Ok(receipt) if receipt.scope != *scope || receipt.operation != operation => {
            Err(RepairError::ProviderUnknown)
        }
        Ok(receipt) if receipt.certainty == RequestCertainty::Written => match receipt.result {
            Some(ProviderResponse::Reply(value)) => Ok(value),
            Some(ProviderResponse::Rejected(_)) => Err(RepairError::ProviderRejected),
            None => Err(RepairError::ProviderUnknown),
        },
        Ok(receipt) if receipt.certainty == RequestCertainty::NotSent => {
            Err(RepairError::AccountSession)
        }
        Ok(_) | Err(AccountSessionError::Unknown) => Err(RepairError::ProviderUnknown),
        Err(_) => Err(RepairError::AccountSession),
    }
}
fn assert_native_queues_empty(
    broker: &dyn AccountSessionPort,
    scope: &ConnectionScope,
    actor: &crate::account_worker::retained::RequestActor,
    operation: &str,
    thread: &str,
    status: &str,
    deadline: Instant,
) -> Result<(), RepairError> {
    let methods = if status == "notLoaded" {
        vec![studio_runtime_providers::Method::ThreadQueueList]
    } else {
        vec![
            studio_runtime_providers::Method::ThreadBackgroundTerminalsList,
            studio_runtime_providers::Method::ThreadQueueList,
        ]
    };
    for (index, method) in methods.into_iter().enumerate() {
        let suffix = if index == 0 {
            "queue-check"
        } else {
            "background-check"
        };
        let result = broker_read(
            broker,
            BrokerRead {
                scope,
                actor,
                operation,
                suffix,
                method,
                params: json!({"threadId":thread}),
                deadline,
            },
        )?;
        if result["data"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
            || !result["nextCursor"].is_null()
        {
            return Err(RepairError::ReceiptHold);
        }
    }
    Ok(())
}
fn verify_native_items(
    broker: &dyn AccountSessionPort,
    scope: &ConnectionScope,
    actor: &crate::account_worker::retained::RequestActor,
    operation: &str,
    thread: &str,
    turn: &str,
    deadline: Instant,
) -> Result<Vec<Value>, RepairError> {
    let mut cursor: Option<String> = None;
    let mut seen = BTreeSet::new();
    let mut entries = Vec::new();
    for page_index in 0..100 {
        if Instant::now() >= deadline {
            return Err(RepairError::ReceiptHold);
        }
        let mut params =
            json!({"threadId":thread,"turnId":turn,"limit":1000,"sortDirection":"asc"});
        if let Some(cursor_value) = cursor.as_ref() {
            params["cursor"] = json!(cursor_value);
        }
        let page = broker_read(
            broker,
            BrokerRead {
                scope,
                actor,
                operation,
                suffix: &item_read_suffix(turn, page_index),
                method: studio_runtime_providers::Method::ThreadItemsList,
                params,
                deadline,
            },
        )?;
        let data = page["data"].as_array().ok_or(RepairError::ReceiptHold)?;
        for entry in data {
            if entry
                .get("turnId")
                .and_then(Value::as_str)
                .is_some_and(|value| value != turn)
            {
                return Err(RepairError::ReceiptHold);
            }
            let item = entry.get("item").ok_or(RepairError::ReceiptHold)?;
            if !native_item_terminal(item) {
                return Err(RepairError::ReceiptHold);
            }
            entries.push(item.clone());
        }
        let Some(next) = page["nextCursor"]
            .as_str()
            .filter(|value| !value.is_empty())
        else {
            return Ok(entries);
        };
        if !seen.insert(next.to_owned()) {
            return Err(RepairError::ReceiptHold);
        }
        cursor = Some(next.to_owned());
    }
    Err(RepairError::ReceiptHold)
}

fn item_read_suffix(turn: &str, page_index: usize) -> String {
    format!(
        "items-{}-{page_index}",
        studio_runtime_store::sha256_hex(turn.as_bytes())
    )
}

#[cfg(test)]
mod item_read_identity_tests {
    use super::item_read_suffix;

    #[test]
    fn item_read_identity_is_stable_per_turn_and_page() {
        assert_eq!(item_read_suffix("turn-a", 0), item_read_suffix("turn-a", 0));
        assert_ne!(item_read_suffix("turn-a", 0), item_read_suffix("turn-b", 0));
        assert_ne!(item_read_suffix("turn-a", 0), item_read_suffix("turn-a", 1));
    }
}

#[cfg(test)]
mod repair_history_tests {
    use super::archive_previous_repair;
    use serde_json::json;

    #[test]
    fn completed_repair_is_kept_in_the_history_used_by_late_callbacks() {
        let completed = json!({
            "id":"repair-1",
            "agent":"agent",
            "phase":"completed",
            "source":{"id":"agent","epoch":2,"accountKey":"default","threadId":"source"},
            "newThreadId":"destination"
        });
        let mut agent = json!({"contextRepair":completed});

        archive_previous_repair(&mut agent).expect("archive prior completed repair");

        assert_eq!(agent["contextRepairHistory"], json!([completed]));
        assert!(agent.get("lastContextRepairCheck").is_none());
    }

    #[test]
    fn repair_with_rpc_method_is_kept_in_history_before_next_repair() {
        let started = json!({
            "id":"repair-rpc",
            "phase":"preparing",
            "rpcMethod":"thread/fork"
        });
        let mut agent = json!({"contextRepair":started});

        archive_previous_repair(&mut agent).expect("archive prior dispatched repair");

        assert_eq!(agent["contextRepairHistory"], json!([started]));
        assert!(agent.get("lastContextRepairCheck").is_none());
    }
}

fn verify_tool_receipts(
    broker: &dyn AccountSessionPort,
    scope: &ConnectionScope,
    actor: &crate::account_worker::retained::RequestActor,
    operation: &str,
    thread: &str,
    receipts: &[Value],
    deadline: Instant,
) -> Result<(), RepairError> {
    for receipt in receipts {
        let turn = receipt["turnId"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or(RepairError::ReceiptHold)?;
        let call = receipt["callId"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or(RepairError::ReceiptHold)?;
        let items = verify_native_items(broker, scope, actor, operation, thread, turn, deadline)?;
        let matches = items
            .iter()
            .filter(|item| item["id"] == call && item["type"] == "dynamicToolCall")
            .collect::<Vec<_>>();
        if matches.len() != 1 || !native_item_terminal(matches[0]) {
            return Err(RepairError::ReceiptHold);
        }
    }
    Ok(())
}

fn uncertain_input_receipts(
    tx: &Transaction<'_>,
    agent: &str,
    epoch: i64,
) -> Result<Vec<Value>, RepairError> {
    let mut statement = tx.prepare(
        "SELECT id,kind,text,created,turn_id FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='uncertain' ORDER BY created,id",
    )?;
    let rows = statement.query_map(rusqlite::params![agent, epoch], |row| {
        Ok(json!({
            "id": row.get::<_, String>(0)?,
            "kind": row.get::<_, String>(1)?,
            "text": row.get::<_, String>(2)?,
            "created": row.get::<_, f64>(3)?,
            "turnId": row.get::<_, Option<String>>(4)?,
        }))
    })?;
    let mut receipts = Vec::new();
    for row in rows {
        let receipt = row?;
        if !receipt["created"].as_f64().is_some_and(f64::is_finite) {
            return Err(RepairError::ReceiptHold);
        }
        receipts.push(receipt);
    }
    Ok(receipts)
}
fn native_item_terminal(item: &Value) -> bool {
    match item["type"].as_str() {
        Some("commandExecution" | "fileChange") => matches!(
            item["status"].as_str(),
            Some("completed" | "failed" | "declined")
        ),
        Some("dynamicToolCall" | "mcpToolCall" | "computerToolCall" | "collabAgentToolCall") => {
            matches!(item["status"].as_str(), Some("completed" | "failed"))
        }
        _ => true,
    }
}
fn ensure_local_idle(tx: &Transaction<'_>, agent: &Value) -> Result<(), RepairError> {
    if agent["inFlight"] == true
        || matches!(
            agent["status"].as_str(),
            Some("starting" | "running" | "compacting" | "queued" | "approval")
        )
        || studio_runtime_store::truthy(agent.get("nativeFailureHold"))
        || studio_runtime_store::truthy(agent.get("accountTransferId"))
        || studio_runtime_store::truthy(agent.get("workspaceOperation"))
        || studio_runtime_store::truthy(agent.get("activeTools"))
        || studio_runtime_store::truthy(agent.get("contextRepairWait"))
    {
        return Err(RepairError::ReceiptHold);
    }
    for (table, column, states) in [
        (
            "runtime_tasks",
            "status",
            "'starting','running','pending','unknown'",
        ),
        (
            "runtime_monitors",
            "status",
            "'starting','running','pending','stopping'",
        ),
        ("runtime_requests", "status", "'pending'"),
        ("runtime_tool_requests", "stage", "'queued','running'"),
    ] {
        if crate::sync_policy::table_exists(tx, table)? {
            let sql = format!(
                "SELECT EXISTS(SELECT 1 FROM {table} WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.{column}') IN ({states}))"
            );
            if tx.query_row(&sql, [agent["id"].as_str()], |row| row.get::<_, bool>(0))? {
                return Err(RepairError::ReceiptHold);
            }
        }
    }
    if crate::sync_policy::table_exists(tx, "voice_sessions")? {
        let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM voice_sessions WHERE agent=?1 AND state IS NOT NULL AND ended IS NULL)",[agent["id"].as_str()],|r|r.get(0))?;
        if active {
            return Err(RepairError::ReceiptHold);
        }
    }
    Ok(())
}
fn verified_repair_events(tx: &Transaction<'_>, agent: &Value) -> Result<Vec<Value>, RepairError> {
    let mut statement=tx.prepare("SELECT e.id,e.kind,e.text,e.turn_id,m.record FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id WHERE e.agent=?1 AND e.status='delivered' AND e.kind IN ('monitor_exit','agent_message','work_review','work_decision') AND ((length(CAST(e.text AS BLOB))>3000 AND coalesce(json_extract(m.record,'$.modelEventProjection'),0)!=1) OR json_extract(m.record,'$.contextManifest.versions.roleSkill') IS NOT NULL) ORDER BY e.created,e.id LIMIT 33")?;
    let rows = statement.query_map([agent["id"].as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let repaired = agent["contextRepair"]["repairedEventIds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut events = Vec::new();
    for row in rows {
        let (id, kind, text, turn, meta) = row?;
        let (Some(turn), Some(meta)) = (turn, meta) else {
            continue;
        };
        let metadata: Value =
            serde_json::from_str(&meta).map_err(|_| RepairError::Invalid("event.metadata"))?;
        let quoted:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND status='delivered' AND kind IN ('user','followup') AND turn_id=?2 AND instr(text,?3)>0)",rusqlite::params![agent["id"].as_str(),turn,format!("[Orchestration event: {kind}]\n{text}")],|r|r.get(0))?;
        if quoted {
            continue;
        }
        if !repaired.contains(&id) && text.len() > 3000 {
            events.push(json!({"id":id,"kind":kind,"text":text,"turn_id":turn}));
        }
        if let Some(version) = metadata
            .pointer("/contextManifest/versions/roleSkill")
            .and_then(Value::as_str)
            && !repaired.contains(&format!("studio-role:{id}"))
        {
            events.push(json!({"id":format!("studio-role:{id}"),"kind":"studio_role","text":text,"turn_id":turn,
                "roleVersion":version,"prefix":format!("[Orchestration event: {kind}]\n{text}")}));
        }
    }
    if events.len() > MAX_INPUTS {
        return Err(RepairError::ReceiptHold);
    }
    Ok(events)
}
fn repair_receipt(receipt: &crate::context_repair_files::HistoryRewriteReceipt) -> Value {
    json!({"sourcePath":receipt.source_path,"sourceSha256":receipt.source_sha256,"sourceBytes":receipt.source_bytes,
        "sourceFileIdentity":{"device":receipt.source_file_identity[0],"inode":receipt.source_file_identity[1],"bytes":receipt.source_file_identity[2],"modifiedNs":receipt.source_file_identity[3]},"importThreadId":receipt.import_thread_id,"copyPath":receipt.copy_path,
        "copySha256":receipt.copy_sha256,"copyBytes":receipt.copy_bytes,"eventIds":receipt.event_ids,
        "terminalTurnId":receipt.terminal_turn_id,"terminalStatus":receipt.terminal_status,
        "terminalCompletedAt":receipt.terminal_completed_at,"inheritedEmpty":receipt.inherited_empty,
        "savedBytes":receipt.saved_bytes,"ancestry":receipt.ancestry.iter().map(|s|json!({"path":s.path,"threadId":s.thread_id,"endByteOffset":s.end_byte_offset,"endOrdinalExclusive":s.end_ordinal_exclusive,"sha256":s.sha256})).collect::<Vec<_>>()})
}

/// The existing efficiency compiler supplies summaries. No user text is passed
/// to it: only stored, turn-scoped Studio event records may be shortened.
pub trait RepairEventSummary {
    fn summarize(&self, event: &Value) -> Result<String, RepairError>;
}

/// Immutable rollout-copy decision. Filesystem identity, ancestry and fsync are
/// the history owner's responsibility; this core never modifies a source file.
pub struct HistoryRewrite {
    records: Vec<Value>,
    events: Vec<Value>,
    thread: String,
    terminal: String,
}
impl HistoryRewrite {
    pub fn from_records(
        records: Vec<Value>,
        events: Vec<Value>,
        thread: String,
        terminal: String,
    ) -> Result<Self, RepairError> {
        if records.len() > MAX_TURNS
            || events.len() > MAX_INPUTS
            || serde_json::to_vec(&(&records, &events))
                .map_err(|_| RepairError::Invalid("rewrite"))?
                .len()
                > MAX_CAPTURE_BYTES
            || thread.is_empty()
            || terminal.is_empty()
        {
            return Err(RepairError::Invalid("rewrite.bounds"));
        }
        let first = records
            .first()
            .ok_or(RepairError::Invalid("rewrite.records"))?;
        if first["type"] != "session_meta" || first["payload"]["id"] != thread {
            return Err(RepairError::Invalid("rewrite.thread"));
        }
        // Flattened ancestry and role versions need the shared history compiler.
        if studio_runtime_store::truthy(first["payload"].get("history_base")) {
            return Err(RepairError::RewriteUnavailable);
        }
        for event in &events {
            let kind = event["kind"].as_str();
            if !matches!(
                kind,
                Some("monitor_exit" | "agent_message" | "work_review" | "work_decision")
            ) {
                return Err(RepairError::RewriteUnavailable);
            }
            for key in ["id", "turn_id", "text"] {
                if !event[key].is_string() || (key != "text" && event[key] == "") {
                    return Err(RepairError::Invalid("rewrite.event"));
                }
            }
        }
        Ok(Self {
            records,
            events,
            thread,
            terminal,
        })
    }
    /// Return the full copy and exact matched event IDs. Missing, quoted,
    /// ambiguous or differently scoped prefixes cannot authorize replacement.
    pub fn rewrite(
        &self,
        import_thread: &str,
        summaries: &impl RepairEventSummary,
    ) -> Result<(Vec<Value>, Vec<String>), RepairError> {
        if import_thread.is_empty() || import_thread == self.thread {
            return Err(RepairError::Invalid("rewrite.importThread"));
        }
        let mut records = self.records.clone();
        let mut matched: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        let mut turn = None;
        let mut started = None;
        let mut terminal = None;
        for (index, record) in records.iter_mut().enumerate() {
            if index == 0 {
                record["payload"]["id"] = json!(import_thread);
                continue;
            }
            match record["type"].as_str() {
                Some("turn_context") => {
                    turn = record["payload"]["turn_id"].as_str().map(str::to_owned);
                    started = turn.clone();
                }
                Some("event_msg") => match record["payload"]["type"].as_str() {
                    Some("task_started") => {
                        started = record["payload"]["turn_id"].as_str().map(str::to_owned)
                    }
                    Some("task_complete" | "turn_aborted") => {
                        terminal = record["payload"]["turn_id"].as_str().map(str::to_owned)
                    }
                    _ => {}
                },
                Some("response_item") => {
                    if let Some(payload) = record.get_mut("payload") {
                        self.rewrite_item(payload, turn.as_deref(), summaries, &mut matched)?;
                    }
                }
                Some("compacted") => {
                    if let Some(items) = record
                        .get_mut("payload")
                        .and_then(|p| p.get_mut("replacement_history"))
                        .and_then(Value::as_array_mut)
                    {
                        for item in items {
                            self.rewrite_item(item, None, summaries, &mut matched)?;
                        }
                    }
                }
                _ => {}
            }
        }
        if started.as_deref() != Some(&self.terminal) || terminal != started {
            return Err(RepairError::ReceiptHold);
        }
        if matched.values().any(|ids| ids.len() > 1) {
            return Err(RepairError::Invalid("rewrite.ambiguousEvent"));
        }
        let ids = matched
            .into_keys()
            .map(|(_, id)| id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok((records, ids))
    }
    fn rewrite_item(
        &self,
        item: &mut Value,
        turn: Option<&str>,
        summaries: &impl RepairEventSummary,
        matched: &mut BTreeMap<(String, String), BTreeSet<String>>,
    ) -> Result<(), RepairError> {
        if item["type"] != "message" || item["role"] != "user" {
            return Ok(());
        }
        let Some(id) = item["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
        else {
            return Ok(());
        };
        let turn = item["internal_chat_message_metadata_passthrough"]["turn_id"]
            .as_str()
            .or(turn)
            .map(str::to_owned);
        let Some(turn) = turn else {
            return Ok(());
        };
        let Some(contents) = item["content"].as_array_mut() else {
            return Ok(());
        };
        for content in contents {
            if content["type"] != "input_text" {
                continue;
            }
            let Some(mut text) = content["text"].as_str().map(str::to_owned) else {
                continue;
            };
            let mut changed = false;
            let mut rest = text.as_str();
            let mut replacement = String::new();
            while !rest.is_empty() {
                let candidates = self
                    .events
                    .iter()
                    .filter(|e| e["turn_id"] == turn)
                    .filter_map(|event| {
                        let prefix = format!(
                            "[Orchestration event: {}]\n{}",
                            event["kind"].as_str()?,
                            event["text"].as_str()?
                        );
                        rest.strip_prefix(&prefix)
                            .filter(|tail| tail.is_empty() || tail.starts_with("\n\n"))
                            .map(|tail| (event, tail))
                    })
                    .collect::<Vec<_>>();
                if candidates.len() != 1 {
                    break;
                }
                let (event, tail) = candidates[0];
                let event_id = event["id"]
                    .as_str()
                    .ok_or(RepairError::Invalid("rewrite.event.id"))?;
                matched
                    .entry((turn.clone(), event_id.to_owned()))
                    .or_default()
                    .insert(id.clone());
                let summary = summaries.summarize(event)?;
                if summary.len() > 3000 {
                    return Err(RepairError::Invalid("rewrite.summary.bounds"));
                }
                replacement.push_str(&format!(
                    "[Orchestration event: {}]\n{summary}",
                    event["kind"]
                        .as_str()
                        .ok_or(RepairError::Invalid("rewrite.kind"))?
                ));
                rest = tail;
                if let Some(tail) = rest.strip_prefix("\n\n") {
                    replacement.push_str("\n\n");
                    rest = tail;
                }
            }
            if !replacement.is_empty() {
                replacement.push_str(rest);
                text = replacement;
                changed = true;
            }
            if changed {
                content["text"] = json!(text);
            }
        }
        Ok(())
    }
}
