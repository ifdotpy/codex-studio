//! Exact native-history reconciliation. Missing history never authorizes replay.
//!
//! Capture under the store lock, read the provider outside it, then compare the
//! complete capture inside the write transaction. The account worker supplies a
//! cached connection identity and the shared delivered-manifest writer.
use std::collections::{BTreeMap, BTreeSet};

use crate::historical_input_receipts::HistoricalBatch;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
    #[error(transparent)]
    Store(#[from] StoreError),
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
    let mut connection = store.read()?;
    let db = connection.transaction().map_err(StoreError::from)?;
    let agent = ContextRepairRepository::agent(&db, id)?.ok_or(RepairError::MissingAgent)?;
    assert_available(&agent)?;
    if agent["provider"] == "claude" || agent["threadId"].is_null() || agent["threadId"] == "" {
        return Ok(
            json!({"id": id, "repair": agent.get("contextRepair").cloned().unwrap_or(Value::Null)}),
        );
    }
    require_schema(&db, &["runtime_event_meta"])?;
    if !ContextRepairRepository::has_candidates(
        &db,
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
}
