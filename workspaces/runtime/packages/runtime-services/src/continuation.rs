//! Checked, bounded snapshots and write intents shared by continuation policies.
//! Stored JSON is retained verbatim except for the fields a decision changes.
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};
use studio_runtime_schema::continuation_snapshot::{
    CapacityContinuation, CapacityContinuationStatus, ContinuationAgent, ContinuationRecord,
    ContinuationSnapshotRecord, UsageContinuation, UsageContinuationCause, UsageContinuationStatus,
};
use studio_runtime_schema::provider_errors::is_misalignment_policy_violation;
use studio_runtime_schema::shared_values::MAX_SAFE_INTEGER;
use studio_runtime_store::AgentRecord;
use thiserror::Error;

pub const POLL_SECONDS: f64 = 180.0;
pub const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
#[cfg(test)]
thread_local! { static REUSED_EMBEDDED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
thread_local! { static IDENTITY_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[derive(Debug, Error)]
pub enum ContinuationError {
    #[error("invalid continuation snapshot at {0}")]
    Invalid(String),
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
    #[error("continuation wake failures: {0:?}")]
    WakeFailures(Vec<String>),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
}
pub type Result<T> = std::result::Result<T, ContinuationError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitStatus {
    Scheduled,
    Started,
    Starting,
    Finished,
    Cancelled,
    Unknown,
    Exhausted,
    Failed,
}
#[derive(Clone)]
pub struct Snapshot {
    identity: AgentRecord,
    identity_current: bool,
    typed: Arc<ContinuationSnapshotRecord>,
    pub(crate) agent: Value,
    pub(crate) ancestors: Vec<Value>,
    pub(crate) claims: Vec<String>,
    pub(crate) record: Option<Value>,
}
impl Snapshot {
    #[cfg(test)]
    pub(crate) fn from_records_with_checked_record(
        raw: Value,
        record: ContinuationRecord,
    ) -> Result<Self> {
        Self::from_records_with_checked_record_and_agent(raw, record, None)
    }
    pub(crate) fn from_records_with_checked_record_and_agent(
        mut raw: Value,
        record: ContinuationRecord,
        agent: Option<&AgentRecord>,
    ) -> Result<Self> {
        if crate::analytics_capture::encoded_len(&raw) > MAX_RECORD_BYTES {
            return Err(ContinuationError::Invalid("/size".into()));
        }
        let key = match &record {
            ContinuationRecord::Usage(_) => Some("usageResume"),
            ContinuationRecord::Capacity(_) => Some("capacityRetry"),
        };
        let reuse =
            key.filter(|key| raw.get("agent").and_then(|a| a.get(*key)) == raw.get("record"));
        let embedded = reuse.map(|key| raw["agent"][key].take());
        #[cfg(test)]
        if reuse.is_some() {
            REUSED_EMBEDDED.with(|count| count.set(count.get() + 1));
        }
        let raw_record = raw["record"].take();
        let mut snapshot = match Self::from_records_inner(&raw, agent) {
            Ok(snapshot) => snapshot,
            Err(_) => {
                raw["record"] = raw_record;
                if let (Some(key), Some(value)) = (reuse, embedded) {
                    raw["agent"][key] = value;
                }
                return Self::from_records_inner(&raw, agent);
            }
        };
        snapshot.record = (!raw_record.is_null()).then_some(raw_record);
        if let (Some(key), Some(value)) = (reuse, embedded) {
            snapshot.agent[key] = value.clone();
            snapshot.identity.extra.insert(key.to_owned(), value);
        }
        let typed = &mut Arc::get_mut(&mut snapshot.typed)
            .expect("new snapshot is uniquely owned")
            .0;
        match (&record, reuse) {
            (ContinuationRecord::Usage(value), Some("usageResume")) => {
                typed.agent.usage_resume = Some(value.clone())
            }
            (ContinuationRecord::Capacity(value), Some("capacityRetry")) => {
                typed.agent.capacity_retry = Some(value.clone())
            }
            _ => {}
        }
        typed.record = Some(record);
        Ok(snapshot)
    }
    /// The only constructor. It accepts raw Python-written records, never trusted flags.
    pub fn from_records(raw: Value) -> Result<Self> {
        Self::from_records_with_agent(&raw, None)
    }
    pub(crate) fn from_records_with_agent(
        raw: &Value,
        agent: Option<&AgentRecord>,
    ) -> Result<Self> {
        if crate::analytics_capture::encoded_len(raw) > MAX_RECORD_BYTES {
            return Err(ContinuationError::Invalid("/size".into()));
        }
        Self::from_records_inner(raw, agent)
    }
    fn from_records_inner(raw: &Value, checked_agent: Option<&AgentRecord>) -> Result<Self> {
        use serde::Deserialize;
        let typed =
            ContinuationSnapshotRecord::deserialize(raw).map_err(|_| invalid_snapshot(raw))?;
        let get_vec = |key: &str| {
            raw.get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let identity = match checked_agent {
            Some(agent) => agent.clone(),
            None => {
                #[cfg(test)]
                IDENTITY_DECODES.with(|count| count.set(count.get() + 1));
                AgentRecord::deserialize(&raw["agent"])
                    .map_err(|_| ContinuationError::Invalid("/agent".into()))?
            }
        };
        Ok(Self {
            identity,
            identity_current: true,
            claims: typed
                .0
                .claims
                .as_ref()
                .map(|claims| {
                    claims
                        .as_slice()
                        .iter()
                        .map(|id| id.as_str().to_owned())
                        .collect()
                })
                .unwrap_or_default(),
            typed: Arc::new(typed),
            agent: raw["agent"].clone(),
            ancestors: get_vec("ancestors"),
            record: raw.get("record").filter(|v| !v.is_null()).cloned(),
        })
    }
    pub fn agent_id(&self) -> &str {
        &self.identity.id.0
    }
    pub(crate) fn agent_record(&self) -> &AgentRecord {
        &self.identity
    }
    pub(crate) fn current_agent_record(&self) -> Option<&AgentRecord> {
        self.identity_current.then_some(&self.identity)
    }
    pub(crate) fn invalidate_agent_record(&mut self) {
        self.identity_current = false;
    }
    pub(crate) fn agent_epoch(&self) -> i64 {
        self.identity.epoch
    }
    pub(crate) fn agent_view(&self) -> &ContinuationAgent {
        &self.typed.0.agent
    }
    pub(crate) fn agent_account(&self) -> &str {
        &self.identity.account_key
    }
    pub(crate) fn ancestors_view(&self) -> &[ContinuationAgent] {
        self.typed
            .0
            .ancestors
            .as_ref()
            .map_or(&[], |a| a.as_slice())
    }
    pub(crate) fn record_id(&self) -> Option<&str> {
        self.typed.0.record.as_ref().map(ContinuationRecord::id)
    }
    pub(crate) fn checked_record(&self) -> Option<&ContinuationRecord> {
        self.typed.0.record.as_ref()
    }
    pub(crate) fn record_matches_agent_identity(&self) -> bool {
        match self.checked_record() {
            Some(ContinuationRecord::Usage(r)) => {
                self.agent.get("threadId").and_then(Value::as_str) == Some(r.thread_id.as_str())
                    && self.agent.get("epoch").and_then(Value::as_number) == Some(r.epoch.number())
                    && self.agent.get("lastCompletedTurn").and_then(Value::as_str)
                        == Some(r.turn_id.as_str())
            }
            Some(ContinuationRecord::Capacity(r)) => {
                self.agent.get("threadId").and_then(Value::as_str) == Some(r.thread_id.as_str())
                    && self.agent.get("epoch").and_then(Value::as_number) == Some(r.epoch.number())
                    && self.agent.get("lastCompletedTurn").and_then(Value::as_str)
                        == Some(r.turn_id.as_str())
            }
            None => false,
        }
    }
    pub(crate) fn record_account(&self) -> Option<&str> {
        self.typed.0.record.as_ref().map(|r| match r {
            ContinuationRecord::Usage(r) => r.account_key.as_str(),
            ContinuationRecord::Capacity(r) => r.account_key.as_str(),
        })
    }
    pub(crate) fn record_due_at(&self) -> Option<f64> {
        self.typed
            .0
            .record
            .as_ref()
            .and_then(|r| match r {
                ContinuationRecord::Usage(r) => r.due_at.as_ref(),
                ContinuationRecord::Capacity(r) => r.due_at.as_ref(),
            })
            .map(|due| due.get())
    }
    pub(crate) fn record_is_auth(&self) -> bool {
        matches!(self.typed.0.record.as_ref(), Some(ContinuationRecord::Usage(r))
            if matches!(r.cause, Some(UsageContinuationCause::Auth)))
    }
    pub(crate) fn usage_record(&self) -> Option<&UsageContinuation> {
        match self.typed.0.record.as_ref()? {
            ContinuationRecord::Usage(record) => Some(record),
            ContinuationRecord::Capacity(_) => None,
        }
    }
    pub(crate) fn record_failed_at(&self) -> Option<f64> {
        self.usage_record().map(|r| r.failed_at.get()).or_else(|| {
            self.record
                .as_ref()
                .and_then(|r| r.get("failedAt"))
                .and_then(Value::as_f64)
        })
    }
    pub(crate) fn record_reset_at(&self) -> Option<f64> {
        match self.usage_record() {
            Some(r) => r.reset_at.as_ref().map(|v| v.get()),
            None => self
                .record
                .as_ref()
                .and_then(|r| r.get("resetAt"))
                .and_then(Value::as_f64),
        }
    }
    pub(crate) fn record_reason(&self) -> Option<&str> {
        match self.typed.0.record.as_ref()? {
            ContinuationRecord::Usage(r) => r.reason.as_ref().map(|v| v.as_str()),
            ContinuationRecord::Capacity(r) => r.reason.as_ref().map(|v| v.as_str()),
        }
    }
    pub(crate) fn capacity_record(&self) -> Option<&CapacityContinuation> {
        match self.typed.0.record.as_ref()? {
            ContinuationRecord::Capacity(record) => Some(record),
            ContinuationRecord::Usage(_) => None,
        }
    }
    pub(crate) fn status(&self) -> Option<WaitStatus> {
        use CapacityContinuationStatus as C;
        use UsageContinuationStatus as U;
        Some(match self.typed.0.record.as_ref()? {
            ContinuationRecord::Usage(record) => match record.status {
                U::Scheduled => WaitStatus::Scheduled,
                U::Started => WaitStatus::Started,
                U::Cancelled => WaitStatus::Cancelled,
                U::Unknown => WaitStatus::Unknown,
            },
            ContinuationRecord::Capacity(record) => match record.status {
                C::Scheduled => WaitStatus::Scheduled,
                C::Starting => WaitStatus::Starting,
                C::Finished => WaitStatus::Finished,
                C::Cancelled => WaitStatus::Cancelled,
                C::Unknown => WaitStatus::Unknown,
                C::Exhausted => WaitStatus::Exhausted,
                C::Failed => WaitStatus::Failed,
            },
        })
    }
    // Pending settings are merged into the raw agent before a capacity claim.
    // Those legacy settings are open JSON, so check the updated identity directly.
    pub(crate) fn current(&self, key: &str) -> bool {
        self.record_id().is_some_and(|id| {
            self.agent
                .get(key)
                .and_then(|a| a.get("id"))
                .and_then(Value::as_str)
                == Some(id)
        })
    }
    pub(crate) fn claims_valid(&self) -> bool {
        let claims = match self.typed.0.record.as_ref() {
            Some(ContinuationRecord::Usage(r)) => r.task_claims.as_ref(),
            Some(ContinuationRecord::Capacity(r)) => r.task_claims.as_ref(),
            None => None,
        };
        let saved: BTreeSet<&str> = claims
            .into_iter()
            .flat_map(|items| items.as_slice())
            .map(|id| id.as_str())
            .collect();
        saved == self.claims.iter().map(String::as_str).collect()
    }
    pub(crate) fn parents_running(&self, auth: bool) -> bool {
        let mut parent = &self.agent;
        let mut seen = BTreeSet::new();
        while let Some(id) = parent.get("parentId").and_then(Value::as_str) {
            if !seen.insert(text(parent, "id")) {
                return false;
            }
            let Some(next) = self.ancestors.iter().find(|p| text(p, "id") == id) else {
                return false;
            };
            if !flag(next, "autoWake")
                || present(next, "deletedAt")
                || auth && text(next, "status") == "paused"
            {
                return false;
            }
            parent = next;
        }
        !auth || text(parent, "id") == text(&self.agent, "rootId")
    }
}
// Keep the existing schema diagnostic paths when typed deserialization fails.
// The schema is only evaluated on invalid input, not on the normal read path.
fn invalid_snapshot(raw: &Value) -> ContinuationError {
    static VALIDATOR: OnceLock<std::result::Result<jsonschema::Validator, String>> =
        OnceLock::new();
    let validator = VALIDATOR.get_or_init(|| {
        let schema: Value = serde_json::from_str(include_str!(
            "../../runtime-schema/src/schemas/persisted-continuation-snapshot.schema.json"
        ))
        .map_err(|e| e.to_string())?;
        jsonschema::validator_for(&schema).map_err(|e| e.to_string())
    });
    match validator {
        Ok(validator) => ContinuationError::Invalid(
            validator
                .iter_errors(raw)
                .next()
                .map(|error| error.instance_path().to_string())
                .unwrap_or_default(),
        ),
        Err(_) => ContinuationError::Invalid("schema".into()),
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Followup {
        id: String,
        text: String,
    },
    WorkerHold {
        operation: String,
        transition: &'static str,
        reason: String,
    },
}

/// A transition proposal retains the checked source and unbounded edits until
/// the writer validates the complete resulting snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct ProposedRecord(ProposedRecordData);

#[derive(Clone, Debug, PartialEq)]
enum ProposedRecordData {
    Existing {
        source: ContinuationRecord,
        updates: Vec<ContinuationUpdate>,
    },
    Usage(UsageDraft),
    Capacity(CapacityDraft),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ContinuationUpdate {
    Status(&'static str),
    DueAt(Option<f64>),
    Reason(Option<String>),
    StartedAt(f64),
    UpdatedAt(f64),
    PlannedAt(Option<f64>),
    ResetAt(Option<f64>),
    ProofAt(f64),
    LastCheckedAt(Option<Value>),
    WaitingForAuth(bool),
    RemoveClaimedAt,
    ClaimedAt(f64),
    AcceptedTurnId(String),
    Waits(u64),
    Settings(Value),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageDraft {
    pub id: String,
    pub status: &'static str,
    pub account_key: String,
    pub thread_id: Option<String>,
    pub epoch: i64,
    pub turn_id: String,
    pub cause: crate::usage_resume::Cause,
    pub failed_at: f64,
    pub auth_attempt: u64,
    pub auth_refresh_marker: Option<String>,
    pub due_at: Option<f64>,
    pub planned_at: Option<f64>,
    pub reset_at: Option<f64>,
    pub reason: Option<String>,
    pub task_claims: Vec<String>,
    pub updated_at: f64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CapacityDraft {
    pub id: String,
    pub thread_id: Option<String>,
    pub turn_id: String,
    pub account_key: String,
    pub epoch: i64,
    pub cause: &'static str,
    pub status: &'static str,
    pub due_at: Option<f64>,
    pub attempt: u64,
    pub max_attempts: usize,
    pub cwd: Value,
    pub settings: Value,
    pub task_claims: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub updated_at: f64,
}

impl ProposedRecord {
    /// Validate only the proposed fields on an already checked source. New
    /// records and edits which can change the untagged variant use the full
    /// snapshot validator so its diagnostics and timing remain unchanged.
    pub(crate) fn checked(&self) -> Option<ContinuationRecord> {
        use serde::Deserialize;
        use studio_runtime_schema::queue_command::Field;
        let ProposedRecordData::Existing { source, updates } = &self.0 else {
            return None;
        };
        let mut record = source.clone();
        for update in updates {
            match update {
                ContinuationUpdate::Status(v) => {
                    let wire = Value::String((*v).to_owned());
                    match &mut record {
                        ContinuationRecord::Usage(r) => {
                            r.status = studio_runtime_schema::string_enum::deserialize(wire).ok()?
                        }
                        ContinuationRecord::Capacity(r) => {
                            r.status = studio_runtime_schema::string_enum::deserialize(wire).ok()?
                        }
                    }
                }
                ContinuationUpdate::DueAt(v) => match &mut record {
                    ContinuationRecord::Usage(r) => r.due_at = Field::deserialize(json!(v)).ok()?,
                    ContinuationRecord::Capacity(r) => {
                        r.due_at = Field::deserialize(json!(v)).ok()?
                    }
                },
                ContinuationUpdate::Reason(v) => match &mut record {
                    ContinuationRecord::Usage(r) => r.reason = Field::deserialize(json!(v)).ok()?,
                    ContinuationRecord::Capacity(r) => {
                        r.reason = Field::deserialize(json!(v)).ok()?
                    }
                },
                ContinuationUpdate::PlannedAt(v) => match &mut record {
                    ContinuationRecord::Usage(r) => {
                        r.planned_at = Field::deserialize(json!(v)).ok()?
                    }
                    ContinuationRecord::Capacity(_) => {
                        record.extra_mut().insert("plannedAt".into(), json!(v));
                    }
                },
                ContinuationUpdate::ResetAt(v) => match &mut record {
                    ContinuationRecord::Usage(r) => {
                        r.reset_at = Field::deserialize(json!(v)).ok()?
                    }
                    ContinuationRecord::Capacity(_) => {
                        record.extra_mut().insert("resetAt".into(), json!(v));
                    }
                },
                ContinuationUpdate::ProofAt(v) => match &mut record {
                    ContinuationRecord::Usage(r) => {
                        r.proof_at = Field::deserialize(json!(v)).ok()?
                    }
                    ContinuationRecord::Capacity(_) => {
                        record.extra_mut().insert("proofAt".into(), json!(v));
                    }
                },
                ContinuationUpdate::RemoveClaimedAt => match &mut record {
                    ContinuationRecord::Capacity(r) => r.claimed_at = Field::Missing,
                    ContinuationRecord::Usage(_) => {
                        record.extra_mut().remove("claimedAt");
                    }
                },
                ContinuationUpdate::ClaimedAt(v) => match &mut record {
                    ContinuationRecord::Capacity(r) => {
                        r.claimed_at = Field::deserialize(json!(v)).ok()?
                    }
                    ContinuationRecord::Usage(_) => {
                        record.extra_mut().insert("claimedAt".into(), json!(v));
                    }
                },
                ContinuationUpdate::AcceptedTurnId(v) => match &mut record {
                    ContinuationRecord::Capacity(r) => {
                        r.accepted_turn_id = Field::deserialize(json!(v)).ok()?
                    }
                    ContinuationRecord::Usage(_) => {
                        record.extra_mut().insert("acceptedTurnId".into(), json!(v));
                    }
                },
                ContinuationUpdate::Waits(v) => match &mut record {
                    ContinuationRecord::Capacity(r) => {
                        r.waits = Some(Deserialize::deserialize(json!(v)).ok()?)
                    }
                    ContinuationRecord::Usage(_) => {
                        record.extra_mut().insert("waits".into(), json!(v));
                    }
                },
                ContinuationUpdate::Settings(v) => match &mut record {
                    ContinuationRecord::Capacity(r) => {
                        r.settings = v
                            .as_object()?
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    }
                    ContinuationRecord::Usage(_) => {
                        record.extra_mut().insert("settings".into(), v.clone());
                    }
                },
                ContinuationUpdate::StartedAt(v) => {
                    record.extra_mut().insert("startedAt".into(), json!(v));
                }
                ContinuationUpdate::UpdatedAt(v) => {
                    record.extra_mut().insert("updatedAt".into(), json!(v));
                }
                ContinuationUpdate::LastCheckedAt(v) => {
                    record.extra_mut().insert("lastCheckedAt".into(), json!(v));
                }
                ContinuationUpdate::WaitingForAuth(v) => {
                    record.extra_mut().insert("waitingForAuth".into(), json!(v));
                }
            }
        }
        Some(record)
    }
    pub fn value(&self) -> Value {
        match &self.0 {
            ProposedRecordData::Usage(record) => json!(record),
            ProposedRecordData::Capacity(record) => json!(record),
            ProposedRecordData::Existing { source, updates } => {
                let mut value = json!(source);
                for update in updates {
                    match update {
                        ContinuationUpdate::Status(v) => value["status"] = json!(v),
                        ContinuationUpdate::DueAt(v) => value["dueAt"] = json!(v),
                        ContinuationUpdate::Reason(v) => value["reason"] = json!(v),
                        ContinuationUpdate::StartedAt(v) => value["startedAt"] = json!(v),
                        ContinuationUpdate::UpdatedAt(v) => value["updatedAt"] = json!(v),
                        ContinuationUpdate::PlannedAt(v) => value["plannedAt"] = json!(v),
                        ContinuationUpdate::ResetAt(v) => value["resetAt"] = json!(v),
                        ContinuationUpdate::ProofAt(v) => value["proofAt"] = json!(v),
                        ContinuationUpdate::LastCheckedAt(v) => value["lastCheckedAt"] = json!(v),
                        ContinuationUpdate::WaitingForAuth(v) => value["waitingForAuth"] = json!(v),
                        ContinuationUpdate::RemoveClaimedAt => {
                            value
                                .as_object_mut()
                                .expect("record object")
                                .remove("claimedAt");
                        }
                        ContinuationUpdate::ClaimedAt(v) => value["claimedAt"] = json!(v),
                        ContinuationUpdate::AcceptedTurnId(v) => value["acceptedTurnId"] = json!(v),
                        ContinuationUpdate::Waits(v) => value["waits"] = json!(v),
                        ContinuationUpdate::Settings(v) => value["settings"] = v.clone(),
                    }
                }
                value
            }
        }
    }
    pub(crate) fn existing(snapshot: &Snapshot) -> Option<Self> {
        let source = snapshot.checked_record()?.clone();
        let mut updates = Vec::new();
        if let Some(current) = snapshot
            .record
            .as_ref()
            .and_then(|record| record.get("settings"))
        {
            let unchanged = match &source {
                ContinuationRecord::Capacity(record) => current.as_object().is_some_and(|object| {
                    object.len() == record.settings.len()
                        && object
                            .iter()
                            .all(|(key, value)| record.settings.get(key) == Some(value))
                }),
                ContinuationRecord::Usage(record) => record.extra.get("settings") == Some(current),
            };
            if !unchanged {
                updates.push(ContinuationUpdate::Settings(current.clone()));
            }
        }
        Some(Self(ProposedRecordData::Existing { source, updates }))
    }
    pub(crate) fn usage(record: UsageDraft) -> Self {
        Self(ProposedRecordData::Usage(record))
    }
    pub(crate) fn capacity(record: CapacityDraft) -> Self {
        Self(ProposedRecordData::Capacity(record))
    }
    pub(crate) fn update(&mut self, update: ContinuationUpdate) {
        match &mut self.0 {
            ProposedRecordData::Existing { updates, .. } => updates.push(update),
            ProposedRecordData::Usage(_) | ProposedRecordData::Capacity(_) => {
                unreachable!("new drafts are built directly")
            }
        }
    }
}
/// Agent evidence retains its JSON shape through pre-write validation while the
/// checked agent follows ordinary typed mutations. Pending settings can make
/// the checked projection stale; those are checked at the original boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct ProposedAgent {
    evidence: Value,
    identity: Option<AgentRecord>,
}
impl ProposedAgent {
    pub(crate) fn new(snapshot: &Snapshot) -> Self {
        Self {
            evidence: snapshot.agent.clone(),
            identity: snapshot.identity_current.then(|| snapshot.identity.clone()),
        }
    }
    pub fn value(&self) -> &Value {
        &self.evidence
    }
    pub(crate) fn checked(&self) -> Option<&AgentRecord> {
        self.identity.as_ref()
    }
    fn set_extra(&mut self, key: &str, value: Value) {
        self.evidence[key] = value.clone();
        if let Some(agent) = &mut self.identity {
            agent.extra.insert(key.to_owned(), value);
        }
    }
    pub(crate) fn continuation(&mut self, key: &str, value: Value) {
        self.set_extra(key, value);
    }
    pub(crate) fn auth_attempt(&mut self, value: u64) {
        self.set_extra("authResumeAttempt", json!(value));
    }
    pub(crate) fn enable_usage(&mut self) {
        self.evidence["usageResumeEnabled"] = json!(true);
        if let Some(agent) = &mut self.identity {
            agent.usage_resume_enabled = Some(true);
        }
    }
    pub(crate) fn capacity_count(&mut self, value: u64) {
        self.set_extra("capacityRetryCount", json!(value));
    }
    pub(crate) fn clear_hold(&mut self) {
        remove(&mut self.evidence, "nativeFailureHold");
        if let Some(agent) = &mut self.identity {
            agent.extra.remove("nativeFailureHold");
        }
    }
    pub(crate) fn queued(&mut self) {
        self.evidence["status"] = json!("queued");
        if let Some(agent) = &mut self.identity {
            agent.set_status(studio_runtime_store::AgentStatus::Queued);
        }
        self.set_extra("error", Value::Null);
    }
    pub(crate) fn failed(&mut self, reason: &str) {
        self.evidence["status"] = json!("failed");
        if let Some(agent) = &mut self.identity {
            agent.set_status(studio_runtime_store::AgentStatus::Failed);
        }
        self.evidence["inFlight"] = json!(false);
        if let Some(agent) = &mut self.identity {
            agent.in_flight = Some(false);
        }
        self.set_extra("error", json!(reason));
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decision {
    pub agent: Option<ProposedAgent>,
    pub record: Option<ProposedRecord>,
    pub effects: Vec<Effect>,
}
impl Decision {
    pub(crate) fn save(s: &Snapshot, mut record: ProposedRecord, key: &str, now: f64) -> Self {
        if matches!(record.0, ProposedRecordData::Existing { .. }) {
            record.update(ContinuationUpdate::UpdatedAt(now));
        }
        let mut agent = ProposedAgent::new(s);
        agent.continuation(key, record.value());
        Self {
            agent: Some(agent),
            record: Some(record),
            ..Self::default()
        }
    }
    pub(crate) fn hold(&mut self, operation: &str, transition: &'static str, reason: &str) {
        self.effects.push(Effect::WorkerHold {
            operation: operation.into(),
            transition,
            reason: reason.into(),
        });
    }
}
pub(crate) fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
pub(crate) fn number(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}
pub(crate) fn flag(v: &Value, key: &str) -> bool {
    v.get(key) == Some(&Value::Bool(true))
}
pub(crate) fn present(v: &Value, key: &str) -> bool {
    crate::event_write::truthy(v.get(key))
}
pub(crate) fn remove(v: &mut Value, key: &str) {
    if let Some(o) = v.as_object_mut() {
        o.remove(key);
    }
}
pub(crate) fn account(v: &Value) -> &str {
    v.get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
}
pub(crate) fn now_checked(now: f64) -> Result<()> {
    if !now.is_finite() || !(0.0..=(MAX_SAFE_INTEGER as f64)).contains(&now) {
        Err(ContinuationError::Invalid("/now".into()))
    } else {
        Ok(())
    }
}

pub(crate) fn native_thread_blocked(agent: &Value) -> bool {
    let thread = agent.get("threadId");
    (present(&agent["nativeThreadBlock"], "threadId")
        && agent["nativeThreadBlock"].get("threadId") == thread)
        || (present(agent, "threadId")
            && (is_misalignment_policy_violation(&agent["error"])
                || (present(&agent["nativeTurnError"], "turnId")
                    && agent["nativeTurnError"].get("turnId") == agent.get("turnId")
                    && is_misalignment_policy_violation(&agent["nativeTurnError"]["error"]))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_snapshot_preserves_future_status_and_nullable_evidence() {
        let agent = json!({"compactions":0,"accountKey":"default","id":"a","rootId":"a","epoch":1,"status":"future-state","provider":"codex","titleJob":null,"nativeRelease":null,"firedEvents":[],"opaque":{"n":1}});
        let snapshot = Snapshot::from_records(json!({"agent":agent})).unwrap();
        assert_eq!(snapshot.agent, agent);
        assert_eq!(snapshot.identity.status.as_str(), "future-state");
        assert!(snapshot.identity.value().get("titleJob").is_none());
        assert!(snapshot.identity.value().get("nativeRelease").is_none());
        assert!(snapshot.identity.value().get("firedEvents").is_none());
        let mut proposal = ProposedAgent::new(&snapshot);
        assert_eq!(proposal.value(), &agent);
        proposal.auth_attempt(0);
        assert_eq!(proposal.value()["titleJob"], Value::Null);
        assert_eq!(proposal.value()["nativeRelease"], Value::Null);
        assert_eq!(proposal.value()["firedEvents"], json!([]));
        assert_eq!(proposal.checked().unwrap().extra["authResumeAttempt"], 0);
        assert_eq!(proposal.checked().unwrap().status.as_str(), "future-state");
    }

    #[test]
    fn agent_snapshot_checks_original_wire_size_before_normalization() {
        let mut agent = json!({"compactions":0,"id":"a","rootId":"a","epoch":1,"status":"future","provider":"codex","titleJob":null,"nativeRelease":null,"firedEvents":[]});
        agent["opaque"] = json!("x".repeat(MAX_RECORD_BYTES));
        let error = Snapshot::from_records(json!({"agent":agent}))
            .err()
            .unwrap()
            .to_string();
        assert_eq!(error, "invalid continuation snapshot at /size");
    }

    #[test]
    fn proposal_reuses_current_identity_and_fences_pending_settings() {
        let raw = json!({"agent":{"compactions":0,"accountKey":"default","id":"a","rootId":"a","epoch":1,"status":"failed","provider":"codex","titleJob":null}});
        let mut original = Snapshot::from_records(raw.clone()).unwrap();
        let proposal = ProposedAgent::new(&original);
        IDENTITY_DECODES.with(|count| count.set(0));
        Snapshot::from_records_with_agent(&raw, proposal.checked()).unwrap();
        IDENTITY_DECODES.with(|count| assert_eq!(count.get(), 0));
        original.agent["status"] = json!("future");
        original.invalidate_agent_record();
        let proposal = ProposedAgent::new(&original);
        assert!(proposal.checked().is_none());
        let applied = Snapshot::from_records_with_agent(
            &json!({"agent":proposal.value()}),
            proposal.checked(),
        )
        .unwrap();
        assert_eq!(applied.agent_record().status.as_str(), "future");
    }

    #[test]
    fn checked_snapshot_reuses_matching_embedded_usage_and_capacity() {
        use serde::Deserialize;
        let usage = json!({"id":"u","accountKey":"a","threadId":"t","epoch":1.0,"turnId":"v","status":"scheduled","cause":"auth","failedAt":1,"future":{"x":[1]}});
        let capacity = json!({"id":"c","accountKey":"a","threadId":"t","epoch":1,"turnId":"v","status":"scheduled","cause":"serverOverloaded","cwd":"","settings":{},"attempt":1,"maxAttempts":4});
        for (key, record) in [("usageResume", usage), ("capacityRetry", capacity)] {
            let mut raw = json!({"agent":{"compactions":0,"accountKey":"default","id":"a","rootId":"a","epoch":1,"status":"failed","provider":"codex"},"record":record});
            raw["agent"][key] = record.clone();
            let checked = ContinuationRecord::deserialize(record.clone()).unwrap();
            REUSED_EMBEDDED.with(|count| count.set(0));
            let snapshot =
                Snapshot::from_records_with_checked_record(raw.clone(), checked).unwrap();
            REUSED_EMBEDDED.with(|count| assert_eq!(count.get(), 1));
            assert_eq!(snapshot.agent, raw["agent"]);
            assert_eq!(snapshot.record, Some(record.clone()));
            assert_eq!(json!(snapshot.checked_record().unwrap()), record);
            assert_eq!(snapshot.identity.extra.get(key), Some(&record));
            assert_eq!(
                match key {
                    "usageResume" => json!(snapshot.agent_view().usage_resume.as_ref().unwrap()),
                    _ => json!(snapshot.agent_view().capacity_retry.as_ref().unwrap()),
                },
                record
            );
            raw["agent"][key]["id"] = json!("other");
            REUSED_EMBEDDED.with(|count| count.set(0));
            let checked = ContinuationRecord::deserialize(record).unwrap();
            Snapshot::from_records_with_checked_record(raw, checked).unwrap();
            REUSED_EMBEDDED.with(|count| assert_eq!(count.get(), 0));
        }
    }

    #[test]
    fn checked_snapshot_keeps_invalid_embedded_and_size_error_order() {
        use serde::Deserialize;
        let record = json!({"id":"u","accountKey":"a","threadId":"t","epoch":1,"turnId":"v","status":"scheduled","cause":"auth","failedAt":1});
        let mut raw = json!({"agent":{"compactions":0,"accountKey":"default","id":"a","rootId":"a","epoch":1,"status":"failed","provider":"codex","usageResume":record},"record":record});
        raw["agent"]["usageResume"]["status"] = json!(false);
        let expected = Snapshot::from_records(raw.clone())
            .err()
            .unwrap()
            .to_string();
        let checked = ContinuationRecord::deserialize(record.clone()).unwrap();
        assert_eq!(
            Snapshot::from_records_with_checked_record(raw.clone(), checked)
                .err()
                .unwrap()
                .to_string(),
            expected
        );
        raw["agent"]["extra"] = json!("x".repeat(MAX_RECORD_BYTES));
        let checked = ContinuationRecord::deserialize(record).unwrap();
        assert_eq!(
            Snapshot::from_records_with_checked_record(raw, checked)
                .err()
                .unwrap()
                .to_string(),
            "invalid continuation snapshot at /size"
        );
    }

    #[test]
    fn checked_updates_match_full_untagged_deserialization() {
        use serde::Deserialize;
        let samples = [
            json!({"id":"u","accountKey":"a","threadId":"t","epoch":1.0,"turnId":"v","status":"scheduled","cause":"auth","failedAt":1,"future":true}),
            json!({"id":"c","accountKey":"a","threadId":"t","epoch":1,"turnId":"v","status":"scheduled","cause":"serverOverloaded","cwd":"","settings":{},"attempt":1,"maxAttempts":4}),
        ];
        let updates = vec![
            ContinuationUpdate::Status("scheduled"),
            ContinuationUpdate::Status("failed"),
            ContinuationUpdate::DueAt(None),
            ContinuationUpdate::DueAt(Some(42.0)),
            ContinuationUpdate::DueAt(Some(-1.0)),
            ContinuationUpdate::Reason(None),
            ContinuationUpdate::Reason(Some("text".into())),
            ContinuationUpdate::Reason(Some("x".repeat(1_048_577))),
            ContinuationUpdate::StartedAt(1.0),
            ContinuationUpdate::UpdatedAt(2.0),
            ContinuationUpdate::PlannedAt(Some(3.0)),
            ContinuationUpdate::ResetAt(None),
            ContinuationUpdate::ProofAt(4.0),
            ContinuationUpdate::LastCheckedAt(Some(json!({"opaque":true}))),
            ContinuationUpdate::WaitingForAuth(true),
            ContinuationUpdate::RemoveClaimedAt,
            ContinuationUpdate::ClaimedAt(5.0),
            ContinuationUpdate::AcceptedTurnId("turn".into()),
            ContinuationUpdate::AcceptedTurnId(String::new()),
            ContinuationUpdate::Waits(6),
            ContinuationUpdate::Settings(json!({"x":[1]})),
            ContinuationUpdate::Settings(json!(false)),
        ];
        for sample in samples {
            let source = ContinuationRecord::deserialize(sample).unwrap();
            for update in &updates {
                let proposal = ProposedRecord(ProposedRecordData::Existing {
                    source: source.clone(),
                    updates: vec![update.clone()],
                });
                let full = ContinuationRecord::deserialize(proposal.value());
                let checked = proposal.checked();
                assert_eq!(checked.is_some(), full.is_ok(), "{update:?}");
                if let (Some(actual), Ok(expected)) = (checked, full) {
                    assert_eq!(json!(actual), json!(expected), "{update:?}");
                }
            }
        }
    }

    #[test]
    fn current_uses_the_agent_after_pending_settings_are_applied() {
        let retry = json!({"id":"r","accountKey":"default","threadId":"t","epoch":1,"turnId":"turn",
            "status":"scheduled","cause":"serverOverloaded","cwd":"","settings":{},"attempt":0,"maxAttempts":1});
        let mut snapshot = Snapshot::from_records(
            json!({"agent":{"compactions":0,"accountKey":"default","id":"a","rootId":"a","epoch":1,
            "status":"failed","provider":"codex","capacityRetry":retry},"record":retry}),
        )
        .expect("snapshot");
        assert!(snapshot.current("capacityRetry"));
        snapshot.agent["capacityRetry"]["id"] = json!("replaced");
        assert!(!snapshot.current("capacityRetry"));
    }
}
