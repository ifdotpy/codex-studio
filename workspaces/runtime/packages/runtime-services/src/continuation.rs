//! Checked, bounded snapshots and write intents shared by continuation policies.
//! Stored JSON is retained verbatim except for the fields a decision changes.
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
use studio_runtime_schema::continuation_snapshot::{
    AuthWaitReceiptStatus, CapacityContinuation, CapacityContinuationStatus, ContinuationAgent,
    ContinuationRecord, ContinuationSnapshotRecord, UsageContinuation, UsageContinuationCause,
    UsageContinuationStatus,
};
use studio_runtime_schema::provider_errors::is_misalignment_policy_violation;
use studio_runtime_schema::shared_values::MAX_SAFE_INTEGER;
use studio_runtime_store::AgentRecord;
use thiserror::Error;

pub const POLL_SECONDS: f64 = 180.0;
pub const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
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
    /// Retired Claude auth probe wait; startup recovery migrates it once.
    AuthProbeWait,
    Completed,
}
#[derive(Clone)]
pub struct Snapshot {
    identity: AgentRecord,
    typed: Arc<ContinuationSnapshotRecord>,
    pub(crate) agent: Value,
    pub(crate) ancestors: Vec<Value>,
    pub(crate) claims: Vec<String>,
    pub(crate) record: Option<Value>,
    pub(crate) events: Vec<Value>,
    pub(crate) metadata: BTreeMap<String, Option<Value>>,
    pub(crate) complaints: BTreeMap<String, Value>,
    pub(crate) work: BTreeMap<String, Value>,
}
impl Snapshot {
    /// The only constructor. It accepts raw Python-written records, never trusted flags.
    pub fn from_records(raw: Value) -> Result<Self> {
        if raw.to_string().len() > MAX_RECORD_BYTES {
            return Err(ContinuationError::Invalid("/size".into()));
        }
        let typed: ContinuationSnapshotRecord =
            serde_json::from_value(raw.clone()).map_err(|_| invalid_snapshot(&raw))?;
        let get_vec = |key: &str| {
            raw.get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let get_map = |key: &str| {
            raw.get(key)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect()
        };
        let identity = serde_json::from_value(raw["agent"].clone())
            .map_err(|_| ContinuationError::Invalid("/agent".into()))?;
        let snapshot = Self {
            identity,
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
            events: get_vec("events"),
            metadata: raw
                .get("metadata")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|m| m.iter())
                .map(|(k, v)| (k.clone(), (!v.is_null()).then(|| v.clone())))
                .collect(),
            complaints: get_map("complaints"),
            work: get_map("work"),
        };
        let mut ids = BTreeSet::new();
        if snapshot.typed.0.events.as_ref().is_some_and(|events| {
            events
                .as_slice()
                .iter()
                .zip(&snapshot.events)
                .any(|(event, raw)| {
                    !ids.insert(event.id.as_str())
                        || event.agent.as_str() != snapshot.typed.0.agent.id.as_str()
                        || raw.get("epoch") != snapshot.agent.get("epoch")
                })
        }) {
            return Err(ContinuationError::Invalid("/events/identity".into()));
        }
        Ok(snapshot)
    }
    pub fn agent_id(&self) -> &str {
        &self.identity.id.0
    }
    pub(crate) fn agent_epoch(&self) -> i64 {
        self.identity.epoch
    }
    pub(crate) fn agent_view(&self) -> &ContinuationAgent {
        &self.typed.0.agent
    }
    pub(crate) fn agent_account(&self) -> &str {
        self.agent_view()
            .account_key
            .as_ref()
            .map_or("default", |key| key.as_str())
    }
    pub(crate) fn ancestors_view(&self) -> &[ContinuationAgent] {
        self.typed
            .0
            .ancestors
            .as_ref()
            .map_or(&[], |a| a.as_slice())
    }
    pub(crate) fn record_id(&self) -> Option<&str> {
        self.typed.0.record.as_ref().map(|r| match r {
            ContinuationRecord::Usage(r) => r.id.as_str(),
            ContinuationRecord::Capacity(r) => r.id.as_str(),
            ContinuationRecord::Auth(r) => r.id.as_str(),
        })
    }
    pub(crate) fn checked_record(&self) -> Option<&ContinuationRecord> {
        self.typed.0.record.as_ref()
    }
    pub(crate) fn record_account(&self) -> Option<&str> {
        self.typed.0.record.as_ref().map(|r| match r {
            ContinuationRecord::Usage(r) => r.account_key.as_str(),
            ContinuationRecord::Capacity(r) => r.account_key.as_str(),
            ContinuationRecord::Auth(r) => r.account_key.as_str(),
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
                ContinuationRecord::Auth(r) => r.due_at.as_ref(),
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
            ContinuationRecord::Capacity(_) | ContinuationRecord::Auth(_) => None,
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
            ContinuationRecord::Auth(r) => r.extra.get("reason").and_then(Value::as_str),
        }
    }
    pub(crate) fn capacity_record(&self) -> Option<&CapacityContinuation> {
        match self.typed.0.record.as_ref()? {
            ContinuationRecord::Capacity(record) => Some(record),
            ContinuationRecord::Usage(_) | ContinuationRecord::Auth(_) => None,
        }
    }
    pub(crate) fn status(&self) -> Option<WaitStatus> {
        use AuthWaitReceiptStatus as A;
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
            ContinuationRecord::Auth(record) => match record.status {
                A::AuthProbeWait => WaitStatus::AuthProbeWait,
                A::Completed => WaitStatus::Completed,
                A::Cancelled => WaitStatus::Cancelled,
            },
        })
    }
    // Pending settings are merged into the raw agent before a capacity claim.
    // Those legacy settings are open JSON, so check the updated identity directly.
    pub(crate) fn current(&self, key: &str) -> bool {
        self.record.as_ref().is_some_and(|r| {
            self.agent
                .get(key)
                .and_then(|a| a.get("id"))
                .and_then(Value::as_str)
                == r.get("id").and_then(Value::as_str)
        })
    }
    pub(crate) fn claims_valid(&self) -> bool {
        let claims = match self.typed.0.record.as_ref() {
            Some(ContinuationRecord::Usage(r)) => r.task_claims.as_ref(),
            Some(ContinuationRecord::Capacity(r)) => r.task_claims.as_ref(),
            Some(ContinuationRecord::Auth(r)) => Some(&r.task_claims),
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decision {
    pub agent: Option<Value>,
    pub record: Option<Value>,
    pub events: Vec<Value>,
    pub metadata: BTreeMap<String, Value>,
    pub effects: Vec<Effect>,
    /// Ephemeral model projection, never written over the original event text.
    pub batch: Vec<Value>,
}
impl Decision {
    pub(crate) fn save(s: &Snapshot, mut record: Value, key: &str, now: f64) -> Self {
        record["updatedAt"] = json!(now);
        let mut agent = s.agent.clone();
        agent[key] = record.clone();
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
pub(crate) fn patch(v: &mut Value, fields: Value) {
    if let (Some(o), Some(fields)) = (v.as_object_mut(), fields.as_object()) {
        o.extend(fields.clone());
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
    fn current_uses_the_agent_after_pending_settings_are_applied() {
        let retry = json!({"id":"r","accountKey":"default","threadId":"t","epoch":1,"turnId":"turn",
            "status":"scheduled","cause":"serverOverloaded","cwd":"","settings":{},"attempt":0,"maxAttempts":1});
        let mut snapshot = Snapshot::from_records(json!({"agent":{"id":"a","rootId":"a","epoch":1,
            "status":"failed","provider":"codex","capacityRetry":retry},"record":retry}))
        .expect("snapshot");
        assert!(snapshot.current("capacityRetry"));
        snapshot.agent["capacityRetry"]["id"] = json!("replaced");
        assert!(!snapshot.current("capacityRetry"));
    }
}
